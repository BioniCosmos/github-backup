use std::env;

use reqwest::{
    blocking::Client,
    header::{self, HeaderMap, HeaderValue},
};
use serde::Deserialize;

fn main() -> anyhow::Result<()> {
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
        let res = client.get(&url).send()?;
        let next = get_next(res.headers().get("link"));
        repos.append(&mut res.json()?);
        url = next;
    }

    println!("{repos:#?}");
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
