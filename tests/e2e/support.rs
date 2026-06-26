use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct LatestVersion {
    version: String,
}

pub async fn fetch_latest_version(cdn_base_url: &str) -> String {
    let url = format!("{}/latest.json", cdn_base_url.trim_end_matches('/'));
    reqwest::get(&url)
        .await
        .unwrap_or_else(|error| panic!("failed to fetch {url}: {error}"))
        .error_for_status()
        .unwrap_or_else(|error| panic!("failed to fetch {url}: {error}"))
        .text()
        .await
        .map(|body| {
            serde_json::from_str::<LatestVersion>(&body)
                .unwrap_or_else(|error| panic!("failed to parse {url}: {error}"))
                .version
        })
        .unwrap_or_else(|error| panic!("failed to read {url}: {error}"))
}
