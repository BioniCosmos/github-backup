use std::{
    collections::{HashMap, HashSet},
    env,
    fs::{self, File},
    io::ErrorKind,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
    thread,
};

use anyhow::{Context as _, anyhow};
use serde::{Deserialize, Deserializer};
use ureq::{
    Agent, SendBody,
    config::Config,
    http::{HeaderMap, HeaderValue, Request, header},
    middleware::MiddlewareNext,
};

fn main() -> anyhow::Result<()> {
    dotenvy::dotenv()?;

    let mut headers = HeaderMap::with_capacity(3);
    headers.insert(
        header::ACCEPT,
        HeaderValue::try_from("application/vnd.github+json")?,
    );
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::try_from(format!("Bearer {}", env::var("TOKEN")?))?,
    );
    headers.insert("X-GitHub-Api-Version", HeaderValue::try_from("2026-03-10")?);
    let agent = Agent::new_with_config(
        Config::builder()
            .user_agent("github-backup")
            .middleware(move |mut req: Request<SendBody>, next: MiddlewareNext| {
                *req.headers_mut() = headers.clone();
                next.handle(req)
            })
            .build(),
    );

    let mut repos = Vec::<Repo>::new();
    let mut url = String::from("https://api.github.com/user/repos");
    while !url.is_empty() {
        let mut res = agent.get(&url).call()?;
        let next = get_next(res.headers().get("link"));
        repos.append(&mut res.body_mut().read_json()?);
        url = next;
    }

    let mut owners: HashMap<_, HashSet<_>> = HashMap::new();
    for repo in &repos {
        owners.entry(&repo.owner).or_default().insert(
            repo.clone_url
                .split('/')
                .next_back()
                .with_context(|| format!("invalid `clone_url` on {}", repo.full_name))?,
        );
    }

    let base_path = PathBuf::from(env::var("BASE_PATH").unwrap_or("repos".to_owned()));
    for owner in owners.keys() {
        fs::create_dir_all(base_path.join(owner))?;
    }

    let i = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..16 {
            scope.spawn(|| {
                while let Some(Repo {
                    full_name,
                    owner,
                    clone_url,
                }) = repos.get(i.fetch_add(1, Ordering::Relaxed))
                {
                    let repo_path = base_path.join(format!("{full_name}.git"));

                    let is_new =
                        File::open(&repo_path).is_err_and(|e| e.kind() == ErrorKind::NotFound);
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
                        eprintln!("Error when fetching {}: {e}", full_name);
                    }
                }
            });
        }
    });

    for owner_dir in fs::read_dir(&base_path)? {
        let owner_dir = owner_dir?;
        let owner_dir_name = owner_dir
            .file_name()
            .into_string()
            .map_err(|e| anyhow!("invalid owner directory name: {}", e.display()))?;

        let Some(repos) = owners
            .get(&owner_dir_name)
            .filter(|repos| !repos.is_empty())
        else {
            fs::remove_dir_all(owner_dir.path())?;
            continue;
        };

        let repo_dirs = fs::read_dir(owner_dir.path())?
            .map(|x| x.map(|x| x.file_name()))
            .collect::<Result<Vec<_>, _>>()?;
        let repo_dir_set = repo_dirs
            .iter()
            .map(|x| {
                x.to_str()
                    .ok_or_else(|| anyhow!("invalid repository directory name: {}", x.display()))
            })
            .collect::<Result<HashSet<_>, _>>()?;

        for repo_dir in repo_dir_set.difference(repos) {
            fs::remove_dir_all(owner_dir.path().join(repo_dir))?;
        }
    }

    Ok(())
}

#[derive(Clone, Debug, Deserialize)]
struct Repo {
    full_name: String,
    #[serde(deserialize_with = "deserialize_owner")]
    owner: String,
    clone_url: String,
}

fn deserialize_owner<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct Owner {
        login: String,
    }
    Owner::deserialize(deserializer).map(|x| x.login)
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
