use std::{
    env,
    fs::{self, File},
    io::ErrorKind,
    path::Path,
    process::Command,
    sync::atomic::AtomicUsize,
    thread,
};

use anyhow::anyhow;
use serde::Deserialize;
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

    let base_path = env::var("BASE_PATH").unwrap_or("repos".to_owned());
    fs::create_dir_all(&base_path)?;

    let i = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..16 {
            scope.spawn(|| {
                while let Some(Repo {
                    full_name,
                    clone_url,
                }) = repos.get(i.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
                {
                    let repo_path = Path::new(&base_path)
                        .join(clone_url.split("/").last().expect("unexpected invalid URL"));

                    let is_new =
                        File::open(&repo_path).is_err_and(|e| e.kind() == ErrorKind::NotFound);
                    let mut command = Command::new("git");
                    if let Err(e) = if is_new {
                        command
                            .arg("clone")
                            .arg("--mirror")
                            .arg(clone_url)
                            .current_dir(&base_path)
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

    Ok(())
}

#[derive(Debug, Deserialize)]
struct Repo {
    full_name: String,
    clone_url: String,
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
