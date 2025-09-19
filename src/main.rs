use clap::Parser;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{debug, error, info, warn};

#[derive(Parser, Debug)]
#[command(name = "gitea-mirror")]
#[command(about = "Ensures Git repositories are mirrored to Gitea, generated with Claude Opus 4.1")]
struct Args {
    /// Path to TOML configuration file
    #[arg(short, long, env = "GITEA_MIRROR_CONFIG_FILEPATH")]
    config: PathBuf,

    /// Dry run - check but don't create migrations
    #[arg(short, long, default_value_t = false)]
    dry_run: bool,
}

#[derive(Deserialize, Debug)]
struct Config {
    gitea_url: String,
    api_key: String,
    git_urls: Vec<String>,
}

#[derive(Deserialize, Debug)]
struct Repository {
    name: String,
    mirror: bool,
    original_url: Option<String>,
}

#[derive(Serialize)]
struct MigrateRepoRequest {
    clone_addr: String,
    repo_name: String,
    mirror: bool,
    private: bool,
    description: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    // Load configuration
    let config_content = std::fs::read_to_string(&args.config)?;
    let config: Config = toml::from_str(&config_content)?;

    info!("Starting Gitea mirror sync");
    info!("Dry run: {}", args.dry_run);
    info!("Gitea URL: {}", config.gitea_url);
    info!("Checking {} repositories", config.git_urls.len());

    // Create HTTP client with auth header
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("token {}", config.api_key))?,
    );
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()?;

    // Process each Git URL
    for git_url in &config.git_urls {
        info!("Processing: {}", git_url);

        let repo_name = extract_repo_name(git_url);
        let is_mirrored =
            check_if_mirrored(&client, &config.gitea_url, git_url, &repo_name).await?;

        if is_mirrored {
            info!("✓ Already mirrored: {}", repo_name);
        } else {
            warn!("✗ Not mirrored: {}", repo_name);

            if !args.dry_run {
                info!("Creating migration for: {}", repo_name);
                create_migration(&client, &config.gitea_url, git_url, &repo_name).await?;
                info!("✓ Migration created for: {}", repo_name);
            } else {
                info!("[DRY RUN] Would create migration for: {}", repo_name);
            }
        }
    }

    info!("Gitea mirror sync complete");
    Ok(())
}

fn extract_repo_name(git_url: &str) -> String {
    let url = git_url.trim_end_matches(".git");
    url.split('/').last().unwrap_or("unknown").to_string()
}

async fn check_if_mirrored(
    client: &reqwest::Client,
    gitea_url: &str,
    git_url: &str,
    repo_name: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    // Search for repositories by name
    let search_url = format!("{}/api/v1/repos/search", gitea_url);
    let response = client
        .get(&search_url)
        .query(&[("q", repo_name), ("limit", "50")])
        .send()
        .await?;

    if !response.status().is_success() {
        error!("Failed to search repos: {}", response.status());
        return Ok(false);
    }

    let search_result: serde_json::Value = response.json().await?;

    if let Some(data) = search_result.get("data").and_then(|d| d.as_array()) {
        for repo_json in data {
            if let Ok(repo) = serde_json::from_value::<Repository>(repo_json.clone()) {
                debug!("Found repo: {} (mirror: {})", repo.name, repo.mirror);

                // Check if this is a mirror and matches our URL
                if repo.mirror {
                    if let Some(original) = &repo.original_url {
                        // Normalize URLs for comparison
                        let normalized_original = normalize_git_url(original);
                        let normalized_target = normalize_git_url(git_url);

                        if normalized_original == normalized_target {
                            return Ok(true);
                        }
                    }
                }
            }
        }
    }

    Ok(false)
}

fn normalize_git_url(url: &str) -> String {
    let mut normalized = url.to_lowercase();

    // Remove trailing .git
    if normalized.ends_with(".git") {
        normalized = normalized[..normalized.len() - 4].to_string();
    }

    // Convert git@ to https://
    if normalized.starts_with("git@") {
        normalized = normalized.replace("git@", "https://").replace(":", "/");
    }

    // Remove protocol variations
    normalized = normalized
        .replace("https://", "")
        .replace("http://", "")
        .replace("git://", "");

    normalized
}

async fn create_migration(
    client: &reqwest::Client,
    gitea_url: &str,
    git_url: &str,
    repo_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let migrate_url = format!("{}/api/v1/repos/migrate", gitea_url);

    let request = MigrateRepoRequest {
        clone_addr: git_url.to_string(),
        repo_name: repo_name.to_string(),
        mirror: true,
        private: false,
        description: format!("Mirror of {}", git_url),
    };

    let response = client.post(&migrate_url).json(&request).send().await?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response.text().await?;
        error!("Failed to create migration: {} - {}", status, error_text);
        return Err(format!("Migration failed: {}", status).into());
    }

    Ok(())
}
