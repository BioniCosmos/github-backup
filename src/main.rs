use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        LazyLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

use anyhow::{Context as _, Result, anyhow};
use serde::{Deserialize, Deserializer};
use ureq::{
    Agent, SendBody,
    config::Config,
    http::{HeaderMap, HeaderValue, Request, Uri, header, uri::Authority},
    middleware::MiddlewareNext,
};

static TOKEN: LazyLock<&str> = LazyLock::new(|| {
    let token = env::var("TOKEN")
        .expect("failed to read env `TOKEN`")
        .into_boxed_str();
    Box::leak(token)
});
static HTTP: LazyLock<Agent> = LazyLock::new(|| {
    let mut headers = HeaderMap::with_capacity(3);
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::try_from(format!("Bearer {}", *TOKEN)).expect("invalid token"),
    );
    headers.insert(
        "X-GitHub-Api-Version",
        HeaderValue::from_static("2026-03-10"),
    );
    Agent::new_with_config(
        Config::builder()
            .user_agent("github-backup")
            .middleware(move |mut req: Request<SendBody>, next: MiddlewareNext| {
                *req.headers_mut() = headers.clone();
                next.handle(req)
            })
            .build(),
    )
});

fn main() -> Result<()> {
    dotenvy::dotenv()?;
    let base_path = PathBuf::from(env::var("BASE_PATH").unwrap_or("repos".to_owned()));
    let local = Local::new(&base_path)?;

    let args = env::args().collect::<Vec<_>>();
    if args.len() > 1 {
        let version = "0.1";
        let help_message = format!("GitHub Backup {version}

If there are no arguments, a backup operation will be performed.
  -l, --list [repo name]/[owner name/repo name]      list the information of one or all local repositories
  -r, --remote [repo name]/[owner name/repo name]    list the information of one or all remote repositories
  -v, --version                                      print the programme version
  -h, --help                                         print this help message");
        match args[1].as_str() {
            "-l" | "--list" => match args.len() {
                3 => list_repo(&local, &args[2])?,
                2 => list_all_repos(&local)?,
                _ => eprintln!("Too many arguments. Try `--help` for more information."),
            },
            "-r" | "--remote" => match args.len() {
                3 => list_remote_repo(&args[2])?,
                2 => todo!(),
                _ => eprintln!("Too many arguments. Try `--help` for more information."),
            },
            "-v" | "--version" => println!("GitHub Backup {version}"),
            "-h" | "--help" => println!("{help_message}"),
            _ => eprintln!("invalid arguments\n\n{help_message}"),
        }
        return Ok(());
    }

    let mut repos = Vec::<Repo>::new();
    let mut url = String::from("https://api.github.com/user/repos");
    while !url.is_empty() {
        let mut res = HTTP.get(&url).call()?;
        let next = get_next(res.headers().get("link"));
        repos.append(&mut res.body_mut().read_json()?);
        url = next;
    }
    add_user_to_url(&mut repos)?;

    let mut owners: HashMap<_, HashSet<_>> = HashMap::new();
    for repo in &repos {
        owners
            .entry(repo.owner.as_str())
            .or_default()
            .insert(repo.name.as_str());
    }

    for owner in owners.keys() {
        fs::create_dir_all(base_path.join(owner))?;
    }

    let has_err = AtomicBool::new(false);
    let i = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..16 {
            scope.spawn(|| {
                while !has_err.load(Ordering::Relaxed)
                    && let Some(Repo {
                        name,
                        full_name,
                        owner,
                        clone_url,
                    }) = repos.get(i.fetch_add(1, Ordering::Relaxed))
                {
                    let repo_path = base_path.join(format!("{full_name}.git"));

                    let is_new = local.query_with_full_name(owner, name).is_some();
                    let mut command = Command::new("git");
                    if let Err(e) = if is_new {
                        command
                            .arg("clone")
                            .arg("--mirror")
                            .arg(clone_url)
                            .current_dir(base_path.join(owner))
                    } else {
                        command
                            .arg("remote")
                            .arg("update")
                            .arg("--prune")
                            .current_dir(&repo_path)
                    }
                    .output()
                    .map_err(anyhow::Error::new)
                    .and_then(|output| match output.status.success() {
                        true => Ok(()),
                        false => Err(anyhow!(
                            String::try_from(output.stderr)
                                .expect("unexpected invalid stderr")
                                .trim()
                                .to_owned()
                        )),
                    }) {
                        has_err.store(true, Ordering::Relaxed);
                        eprintln!("Error when fetching {}: {e}", full_name);
                    }
                }
            });
        }
    });

    for (owner, repos) in local.0 {
        if !owners.contains_key(owner.as_str()) {
            fs::remove_dir_all(base_path.join(owner))?;
            continue;
        }
        let remote_repos = owners.get(owner.as_str()).unwrap();
        for (repo_name, repo) in repos {
            if !remote_repos.contains(repo_name.as_str()) {
                fs::remove_dir_all(&repo.path)?;
            }
        }
    }

    Ok(())
}

#[derive(Deserialize)]
struct User {
    login: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Repo {
    name: String,
    full_name: String,
    #[serde(deserialize_with = "deserialize_owner")]
    owner: String,
    clone_url: String,
}

#[derive(Debug, Deserialize)]
struct CommitMeta {
    sha: String,
    commit: Commit,
}

#[derive(Debug, Deserialize)]
struct Commit {
    author: Author,
    message: String,
}

#[derive(Debug, Deserialize)]
struct Author {
    name: String,
    email: String,
    date: String,
}

fn deserialize_owner<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    User::deserialize(deserializer).map(|x| x.login)
}

fn get_next(raw: Option<&HeaderValue>) -> String {
    if let Some(raw) = raw
        && let Ok(raw) = raw.to_str()
    {
        for item in raw.split(", ") {
            let mut item = item.split("; ");
            let url = item.next().map(|url| &url[1..url.len() - 1]);
            let rel = item.next().map(|rel| &rel[5..rel.len() - 1]);
            if let Some(url) = url
                && let Some(rel) = rel
                && rel == "next"
            {
                return url.to_owned();
            }
        }
    }
    String::new()
}

fn add_user_to_url(repos: &mut Vec<Repo>) -> Result<()> {
    let user = HTTP
        .get("https://api.github.com/user")
        .call()?
        .body_mut()
        .read_json::<User>()?;
    for repo in repos {
        let mut parts = Uri::try_from(&repo.clone_url)?.into_parts();
        parts.authority = Some(Authority::try_from(format!(
            "{}:{}@{}",
            user.login,
            *TOKEN,
            parts
                .authority
                .with_context(|| format!(
                    "no authority component in `clone_url`: {}",
                    repo.clone_url
                ))?
                .as_str()
        ))?);
        repo.clone_url = Uri::from_parts(parts)?.to_string();
    }
    Ok(())
}

fn list_repo(local: &Local, repo_name: &str) -> Result<()> {
    let parts = repo_name.split('/').collect::<Vec<_>>();
    match parts.len() {
        2 => {
            let Some(repo) = local.query_with_full_name(parts[0], parts[1]) else {
                return Err(anyhow!("`{repo_name}` not found"));
            };
            let output = Command::new("git")
                .current_dir(&repo.path)
                .arg("log")
                .arg("-1")
                .output()?;
            if !output.stderr.is_empty() {
                return Err(anyhow!(
                    "Encounter error on `git` when listing `{repo_name}`: {}",
                    String::from_utf8_lossy_owned(output.stderr),
                ));
            }
            if !output.status.success() {
                return Err(anyhow!(
                    "Encounter error on `git` when listing `{repo_name}`: `git` exited unexpectedly with {}",
                    output.status,
                ));
            }
            print!("{}", String::from_utf8_lossy_owned(output.stdout));
        }
        1 => {
            let repos = local.query_with_name(repo_name);
            if repos.is_empty() {
                return Err(anyhow!("`{repo_name}` not found"));
            }
            for repo in repos {
                let output = Command::new("git")
                    .current_dir(&repo.path)
                    .arg("log")
                    .arg("-1")
                    .arg("--pretty=[%h] %an; %ar: %s")
                    .output()?;
                if !output.stderr.is_empty() {
                    return Err(anyhow!(
                        "Encounter error on `git` when listing `{repo_name}`: {}",
                        String::from_utf8_lossy_owned(output.stderr),
                    ));
                }
                if !output.status.success() {
                    return Err(anyhow!(
                        "Encounter error on `git` when listing `{repo_name}`: `git` exited unexpectedly with {}",
                        output.status,
                    ));
                }
                print!(
                    "{} {}",
                    repo.full_name,
                    String::from_utf8_lossy_owned(output.stdout),
                );
            }
        }
        _ => return Err(anyhow!("invalid repository name: `{repo_name}`")),
    }
    Ok(())
}

fn list_all_repos(local: &Local) -> Result<()> {
    let repos = local.flat_sorted();
    let has_err = AtomicBool::default();
    let i = AtomicUsize::default();
    let mut result_data = vec![String::new(); repos.len()];
    let result = SharedPtr(result_data.as_mut_ptr());
    thread::scope(|scope| {
        for _ in 0..16 {
            scope.spawn(|| loop {
                let i = i.fetch_add(1, Ordering::Relaxed);
                if i >= repos.len() || has_err.load(Ordering::Relaxed) {
                    return;
                }
                let repo = repos[i];
                let output = Command::new("git")
                    .current_dir(&repo.path)
                    .arg("log")
                    .arg("-1")
                    .arg("--pretty=[%h] %an; %ar: %s")
                    .output()
                    .unwrap();
                if !output.stderr.is_empty() {
                    has_err.store(true, Ordering::Relaxed);
                    eprintln!(
                        "Encounter error on `git` when listing `{}`: {}",
                        String::from_utf8_lossy_owned(output.stderr),
                        repo.full_name,
                    );
                    return;
                }
                if !output.status.success() {
                    has_err.store(true, Ordering::Relaxed);
                    eprintln!(
                        "Encounter error on `git` when listing `{}`: `git` exited unexpectedly with {}",
                        repo.full_name,
                        output.status,
                    );
                    return;
                }
                unsafe {
                    *result.get().add(i) = format!(
                        "{} {}",
                        repo.full_name,
                        String::from_utf8_lossy_owned(output.stdout),
                    );
                }
            });
        }
    });
    for result in result_data {
        print!("{result}");
    }
    Ok(())
}

fn list_remote_repo(repo_name: &str) -> Result<()> {
    let n = repo_name.chars().filter(|&c| c == '/').count();
    match n {
        1 => {
            let CommitMeta {
                sha,
                commit:
                    Commit {
                        author: Author { name, email, date },
                        message,
                    },
            } = HTTP
                .get(format!("https://api.github.com/repos/{repo_name}/commits"))
                .query("per_page", "1")
                .call()?
                .body_mut()
                .read_json::<[CommitMeta; 1]>()?
                .into_iter()
                .next()
                .unwrap();
            println!(
                "commit {sha}
Author: {name} <{email}>
Date:   {date}\n"
            );
            for line in message.lines() {
                println!("    {line}");
            }
        }
        _ => todo!(),
    }
    Ok(())
}

struct Local(HashMap<String, HashMap<String, LocalRepo>>);

#[derive(Eq, Ord, PartialEq, PartialOrd)]
struct LocalRepo {
    path: PathBuf,
    full_name: String,
}

impl Local {
    fn new(base_dir: &Path) -> Result<Self> {
        let mut repos = HashMap::new();
        for owner in fs::read_dir(base_dir)? {
            let owner = owner?;
            let owner_name = owner
                .file_name()
                .into_string()
                .map_err(|e| anyhow!("invalid owner directory name: {}", e.display()))?;
            let mut owner_repos = HashMap::new();
            for repo in fs::read_dir(owner.path())? {
                let repo = repo?;
                let repo_name = repo
                    .file_name()
                    .into_string()
                    .map_err(|e| anyhow!("invalid repo directory name: {}", e.display()))?
                    .trim_suffix(".git")
                    .to_owned();
                let full_name = format!("{owner_name}/{repo_name}");
                owner_repos.insert(
                    repo_name,
                    LocalRepo {
                        path: repo.path(),
                        full_name,
                    },
                );
            }
            repos.insert(owner_name, owner_repos);
        }
        Ok(Self(repos))
    }

    fn query_with_full_name(&self, owner: &str, repo: &str) -> Option<&LocalRepo> {
        self.0.get(owner).and_then(|repos| repos.get(repo))
    }

    fn query_with_name(&self, repo_name: &str) -> Vec<&LocalRepo> {
        self.0
            .values()
            .flat_map(|repos| {
                repos
                    .iter()
                    .filter(|&(name, _)| name == repo_name)
                    .map(|(_, repo)| repo)
            })
            .collect()
    }

    fn flat_sorted(&self) -> Vec<&LocalRepo> {
        let mut repos = self
            .0
            .values()
            .flat_map(|repos| repos.values())
            .collect::<Vec<_>>();
        repos.sort();
        repos
    }
}

struct SharedPtr<T>(*mut T);

impl<T> SharedPtr<T> {
    fn get(&self) -> *mut T {
        self.0
    }
}

unsafe impl<T> Sync for SharedPtr<T> {}
