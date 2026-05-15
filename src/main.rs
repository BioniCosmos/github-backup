use std::{env, fs::File, io::ErrorKind, path::Path, sync::Arc};

use anyhow::anyhow;
use reqwest::{
    Client,
    header::{self, HeaderMap, HeaderValue},
};
use serde::Deserialize;
use tokio::{fs, process::Command, task::JoinSet};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv()?;

    let mut headers = HeaderMap::with_capacity(4);
    headers.insert(
        header::ACCEPT,
        HeaderValue::try_from("application/vnd.github+json")?,
    );
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::try_from(format!("Bearer {}", env::var("TOKEN")?))?,
    );
    headers.insert("X-GitHub-Api-Version", HeaderValue::try_from("2026-03-10")?);
    headers.insert(header::USER_AGENT, HeaderValue::try_from("github-backup")?);

    let client = Client::builder()
        .default_headers(headers)
        .user_agent("github-backup")
        .build()?;

    let mut repos = Vec::<Repo>::new();
    let mut url = String::from("https://api.github.com/user/repos");
    while !url.is_empty() {
        let res = client.get(&url).send().await?;
        let next = get_next(res.headers().get("link"));
        repos.append(&mut res.json().await?);
        url = next;
    }

    let base_path = Arc::new(env::var("BASE_PATH").unwrap_or("repos".to_owned()));
    fs::create_dir_all(base_path.as_ref()).await?;

    let mut tasks = JoinSet::new();
    for Repo {
        full_name,
        clone_url,
    } in repos
    {
        let base_path = Arc::clone(&base_path);
        tasks.spawn(async move {
            let base_path = base_path.as_ref();
            let repo_path = Path::new(base_path)
                .join(clone_url.split("/").last().expect("unexpected invalid URL"));

            let is_new = File::open(&repo_path).is_err_and(|e| e.kind() == ErrorKind::NotFound);
            let mut command = Command::new("git");
            if let Err(e) = if is_new {
                command
                    .arg("clone")
                    .arg("--mirror")
                    .arg(clone_url)
                    .current_dir(base_path)
            } else {
                command
                    .arg("remote")
                    .arg("update")
                    .arg("--prune")
                    .current_dir(&repo_path)
            }
            .output()
            .await
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
        });
    }
    tasks.join_all().await;

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
