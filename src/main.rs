use std::{
    cell::OnceCell,
    collections::HashMap,
    env,
    fmt::Debug,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Sender},
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

#[derive(Default)]
struct Context {
    base_dir: OnceCell<PathBuf>,
    token: OnceCell<String>,
    local: OnceCell<Local>,
    remote: OnceCell<Remote>,
    http: OnceCell<Agent>,
}

impl Context {
    fn base_dir(&self) -> &Path {
        self.base_dir.get_or_init(|| {
            PathBuf::from(dotenvy::var("STATE_DIRECTORY").unwrap_or_default()).join("repos")
        })
    }

    fn token(&self) -> &str {
        self.token
            .get_or_init(|| dotenvy::var("TOKEN").expect("failed to read env `TOKEN`"))
    }

    fn local(&self) -> &Local {
        self.local
            .get_or_init(|| Local::new(self.base_dir()).expect("failed to initialize `local`"))
    }

    fn remote(&self) -> &Remote {
        self.remote.get_or_init(|| {
            Remote::new(self.http(), self.token()).expect("failed to initialize `remote`")
        })
    }

    fn http(&self) -> &Agent {
        self.http.get_or_init(|| {
            let mut headers = HeaderMap::with_capacity(3);
            headers.insert(
                header::ACCEPT,
                HeaderValue::from_static("application/vnd.github+json"),
            );
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::try_from(format!("Bearer {}", self.token())).expect("invalid token"),
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
        })
    }
}

#[rustfmt::skip]
macro_rules! version { () => { "0.1" }; }
const HELP_MESSAGE: &str = concat!("GitHub Backup ", version!(), "\n
If there are no arguments, a backup operation will be performed.
  -l, --list [repo name]/[owner name/repo name]      list the information of one or all local repositories
  -r, --remote [repo name]/[owner name/repo name]    list the information of one or all remote repositories
  -v, --version                                      print the programme version
  -h, --help                                         print this help message");

fn main() -> Result<()> {
    let ctx = Context::default();
    let args = env::args().collect::<Vec<_>>();
    if args.len() > 1 {
        match args[1].as_str() {
            "-l" | "--list" => match args.len() {
                3 => list_repo(&ctx, &args[2])?,
                2 => list_all_repos(&ctx)?,
                _ => eprintln!("Too many arguments. Try `--help` for more information."),
            },
            "-r" | "--remote" => match args.len() {
                3 => list_remote_repo(&ctx, &args[2])?,
                2 => todo!(),
                _ => eprintln!("Too many arguments. Try `--help` for more information."),
            },
            "-v" | "--version" => println!("GitHub Backup {}", version!()),
            "-h" | "--help" => println!("{HELP_MESSAGE}"),
            _ => eprintln!("invalid arguments\n\n{HELP_MESSAGE}"),
        }
    } else {
        backup(&ctx)?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct User {
    login: String,
}

#[derive(Clone, Debug, Deserialize)]
struct RemoteRepo {
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

fn backup(ctx: &Context) -> Result<()> {
    let base_dir = ctx.base_dir();
    let local = ctx.local();

    for owner in ctx.remote().0.keys() {
        fs::create_dir_all(base_dir.join(owner))?;
    }

    let repos = ctx.remote().flat();
    let has_err = AtomicBool::new(false);
    let i = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..16 {
            scope.spawn(|| {
                while !has_err.load(Ordering::Relaxed)
                    && let Some(RemoteRepo {
                        name,
                        full_name,
                        owner,
                        clone_url,
                    }) = repos.get(i.fetch_add(1, Ordering::Relaxed))
                {
                    let repo_path = base_dir.join(format!("{full_name}.git"));

                    let is_new = local.query_by_full_name(owner, name).is_some();
                    let mut command = Command::new("git");
                    if let Err(e) = if is_new {
                        command
                            .arg("clone")
                            .arg("--mirror")
                            .arg(clone_url)
                            .current_dir(base_dir.join(owner))
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

    for (owner, repos) in &local.0 {
        if !ctx.remote().0.contains_key(owner) {
            fs::remove_dir_all(base_dir.join(owner))?;
            continue;
        }
        let remote_repos = ctx.remote().0.get(owner).unwrap();
        for (repo_name, repo) in repos {
            if !remote_repos.contains_key(repo_name) {
                fs::remove_dir_all(&repo.path)?;
            }
        }
    }

    Ok(())
}

fn list_repo(ctx: &Context, repo_name: &str) -> Result<()> {
    let parts = repo_name.split('/').collect::<Vec<_>>();
    match parts.len() {
        2 => {
            let Some(repo) = ctx.local().query_by_full_name(parts[0], parts[1]) else {
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
            let repos = ctx.local().query_by_name(repo_name);
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

fn list_all_repos(ctx: &Context) -> Result<()> {
    let repos = ctx.local().flat_sorted();
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

fn list_remote_repo(ctx: &Context, repo_name: &str) -> Result<()> {
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
            } = ctx
                .http()
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
        0 => {
            let http = ctx.http();
            let repos = ctx.remote().query_by_name(repo_name);
            let latest_commits = Task::scope(repos.len().min(8), |scope| {
                for repo in &repos {
                    scope.run(|| {
                        Ok(http
                            .get(format!(
                                "https://api.github.com/repos/{}/commits",
                                repo.full_name
                            ))
                            .query("per_page", "1")
                            .call()?
                            .body_mut()
                            .read_json::<[CommitMeta; 1]>()?
                            .into_iter()
                            .next()
                            .unwrap())
                    });
                }
            })?;
            for (
                CommitMeta {
                    sha,
                    commit: Commit { author, message },
                },
                RemoteRepo { full_name, .. },
            ) in latest_commits.into_iter().zip(repos)
            {
                println!(
                    "{full_name} [{sha}] {}; {}: {}",
                    author.name,
                    author.date,
                    message.lines().next().unwrap_or_default(),
                );
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

    fn query_by_full_name(&self, owner: &str, repo: &str) -> Option<&LocalRepo> {
        self.0.get(owner).and_then(|repos| repos.get(repo))
    }

    fn query_by_name(&self, repo_name: &str) -> Vec<&LocalRepo> {
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

struct Remote(HashMap<String, HashMap<String, RemoteRepo>>);

impl Remote {
    fn new(http: &Agent, token: &str) -> Result<Self> {
        let mut repos = vec![];
        let mut url = "https://api.github.com/user/repos";
        let mut res;
        while !url.is_empty() {
            res = http.get(url).call()?;
            repos.append(&mut res.body_mut().read_json()?);
            url = Self::next_page(res.headers().get("link"));
        }
        Self::add_user_info_to_url(http, token, &mut repos)?;

        let mut owners = HashMap::new();
        for repo in repos {
            owners
                .entry(repo.owner.clone())
                .or_insert_with(HashMap::new)
                .insert(repo.name.clone(), repo);
        }
        Ok(Self(owners))
    }

    fn query_by_name(&self, repo_name: &str) -> Vec<&RemoteRepo> {
        self.0
            .values()
            .flat_map(|repos| repos.values().filter(|repo| repo.name == repo_name))
            .collect()
    }

    fn flat(&self) -> Vec<&RemoteRepo> {
        self.0.values().flat_map(|owner| owner.values()).collect()
    }

    fn next_page(raw: Option<&HeaderValue>) -> &str {
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
                    return url;
                }
            }
        }
        ""
    }

    fn add_user_info_to_url(http: &Agent, token: &str, repos: &mut Vec<RemoteRepo>) -> Result<()> {
        let user = http
            .get("https://api.github.com/user")
            .call()?
            .body_mut()
            .read_json::<User>()?;
        for repo in repos {
            let mut parts = Uri::try_from(&repo.clone_url)?.into_parts();
            parts.authority = Some(Authority::try_from(format!(
                "{}:{token}@{}",
                user.login,
                parts.authority.with_context(|| format!(
                    "no authority component in `clone_url`: {}",
                    repo.clone_url,
                ))?,
            ))?);
            repo.clone_url = Uri::from_parts(parts)?.to_string();
        }
        Ok(())
    }
}

struct SharedPtr<T>(*mut T);

impl<T> SharedPtr<T> {
    fn get(&self) -> *mut T {
        self.0
    }
}

unsafe impl<T> Sync for SharedPtr<T> {}

struct Task<'scope, T: 'scope> {
    tx: Sender<TaskPayload<'scope, T>>,
    i: AtomicUsize,
}

type TaskPayload<'scope, T> = (usize, Box<dyn FnOnce() -> Result<T> + Send + 'scope>);

struct TaskResult<T>(usize, T);

impl<T> Eq for TaskResult<T> {}

impl<T> Ord for TaskResult<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

impl<T> PartialOrd for TaskResult<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> PartialEq for TaskResult<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<'scope, T: Send> Task<'scope, T> {
    fn scope(n: usize, f: impl for<'s> FnOnce(&'s Self)) -> Result<Vec<T>> {
        let (tx, rx) = mpsc::channel::<TaskPayload<T>>();
        let rx = Mutex::new(rx);
        let err = OnceLock::new();
        let results = Mutex::new(vec![]);

        thread::scope(|scope| {
            let mut workers = vec![];
            for _ in 0..n {
                workers.push(scope.spawn(|| {
                    loop {
                        let Ok((i, task)) = rx.lock().unwrap().recv() else {
                            return;
                        };
                        if err.get().is_some() {
                            return;
                        }
                        match task() {
                            Ok(result) => results.lock().unwrap().push(TaskResult(i, result)),
                            Err(e) => _ = err.set(e),
                        }
                    }
                }));
            }

            let task_scope = Self {
                tx,
                i: AtomicUsize::new(0),
            };
            f(&task_scope);
            drop(task_scope.tx);

            for worker in workers {
                worker.join().unwrap();
            }
        });

        if let Some(e) = err.into_inner() {
            return Err(e);
        }
        let mut results = results.into_inner().unwrap();
        results.sort();
        Ok(results.into_iter().map(|x| x.1).collect())
    }

    fn run(&self, f: impl FnOnce() -> Result<T> + Send + 'scope) {
        self.tx
            .send((self.i.fetch_add(1, Ordering::Relaxed), Box::new(f)))
            .unwrap();
    }
}
