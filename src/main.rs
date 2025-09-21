use anyhow::{Context, Result};
use clap::Parser;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use tracing::{error, info, warn};
use url::Url;

// --- Structs (Unchanged) ---
#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "A simple tool to ensure git repositories are mirrored to Gitea."
)]
struct Cli {
    #[arg(short, long, env = "GITEA_MIRROR_CONFIG")]
    config: PathBuf,
    #[arg(long)]
    dry_run: bool,
}

#[derive(Deserialize, Debug)]
struct RepoToMirror {
    url: String,
    rename: Option<String>,
}

#[derive(Deserialize, Debug)]
struct Config {
    gitea_url: String,
    api_key: String,
    repos: Vec<RepoToMirror>,
}

// --- Gitea API Structs (Corrected) ---
#[derive(Deserialize, Debug)]
struct GiteaUser {
    id: i64,
    login: String,
}

// **MODIFIED**: This struct now includes `name` and the correct `mirror_url` field.
#[derive(Deserialize, Debug)]
struct GiteaRepo {
    name: String,
    mirror: bool,
    mirror_url: Option<String>, // The original source URL of the mirror
}

#[derive(Serialize, Debug)]
struct MigrationRequest<'a> {
    clone_addr: &'a str,
    uid: i64,
    repo_name: &'a str,
    mirror: bool,
    private: bool,
    description: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let config_content = fs::read_to_string(&cli.config)
        .with_context(|| format!("Failed to read config file at {:?}", cli.config))?;
    let config: Config =
        toml::from_str(&config_content).context("Failed to parse TOML configuration")?;

    if cli.dry_run {
        info!("🔬 Performing a dry run. No migrations will be created.");
    }

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(ACCEPT, "application/json".parse()?);
    headers.insert(CONTENT_TYPE, "application/json".parse()?);
    headers.insert(USER_AGENT, "gitea-mirror-tool/0.1.0".parse()?);
    headers.insert(AUTHORIZATION, format!("token {}", config.api_key).parse()?);
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()?;

    info!("🔗 Connecting to Gitea instance at {}", config.gitea_url);

    let user_url = format!("{}/api/v1/user", config.gitea_url);
    let user = client
        .get(&user_url)
        .send()
        .await?
        .error_for_status()?
        .json::<GiteaUser>()
        .await
        .context("Failed to get Gitea user info. Check your API key and Gitea URL.")?;
    info!(
        "🔑 Authenticated as user '{}' (ID: {})",
        user.login, user.id
    );

    // **MODIFIED**: We now build two sets: one for source URLs and one for existing repo names.
    info!("🔍 Fetching all existing repositories to build a local cache...");
    let mut existing_mirror_sources: HashSet<String> = HashSet::new();
    let mut existing_repo_names: HashSet<String> = HashSet::new();
    let mut page = 1;
    loop {
        let repos_url = format!("{}/api/v1/user/repos", config.gitea_url);
        let repos_on_page = client
            .get(&repos_url)
            .query(&[("limit", "50"), ("page", &page.to_string())])
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<GiteaRepo>>()
            .await
            .context("Failed to fetch a page of existing repositories.")?;

        if repos_on_page.is_empty() {
            break;
        }

        for repo in repos_on_page {
            // Add the name of EVERY repo to prevent any name collisions.
            existing_repo_names.insert(repo.name);

            // If it's a mirror, store its ORIGINAL source URL for an exact match.
            if repo.mirror {
                if let Some(mirror_url) = repo.mirror_url {
                    existing_mirror_sources.insert(mirror_url);
                }
            }
        }
        page += 1;
    }

    info!(
        "Found {} existing repositories and {} configured mirrors.",
        existing_repo_names.len(),
        existing_mirror_sources.len()
    );

    // **MODIFIED**: The main checking logic is now much more robust.
    for repo_config in &config.repos {
        let url_to_mirror = &repo_config.url;

        // CHECK 1: Has this exact source URL already been mirrored?
        if existing_mirror_sources.contains(url_to_mirror) {
            info!(
                "✅ Mirror for source URL '{}' already exists. Skipping.",
                url_to_mirror
            );
            continue;
        }

        // Determine the target name for the new repository.
        let target_repo_name = match &repo_config.rename {
            Some(name) => name.clone(),
            None => get_repo_name_from_url(url_to_mirror).with_context(|| {
                format!("Could not parse repo name from URL: {}", url_to_mirror)
            })?,
        };

        // CHECK 2: Will creating this mirror cause a name collision?
        if existing_repo_names.contains(&target_repo_name) {
            warn!(
                "⚠️ Cannot create mirror for '{}'. A repository named '{}' already exists. Skipping.",
                url_to_mirror, target_repo_name
            );
            continue;
        }

        // If both checks pass, we are clear to create the migration.
        info!(
            "🔎 Mirror for '{}' not found and name '{}' is available. Needs creation.",
            url_to_mirror, target_repo_name
        );

        if cli.dry_run {
            warn!(
                "--dry-run enabled, skipping migration for '{}'.",
                url_to_mirror
            );
            continue;
        }

        let migration_payload = MigrationRequest {
            clone_addr: url_to_mirror,
            uid: user.id,
            repo_name: &target_repo_name,
            mirror: true,
            private: true,
            description: format!("Mirror of {}", url_to_mirror),
        };

        info!(
            "🚀 Creating migration for '{}' as new repo '{}'...",
            url_to_mirror, target_repo_name
        );

        let migrate_url = format!("{}/api/v1/repos/migrate", config.gitea_url);
        let response = client
            .post(&migrate_url)
            .json(&migration_payload)
            .send()
            .await?;

        if response.status().is_success() {
            info!(
                "✅ Successfully initiated migration for '{}'.",
                url_to_mirror
            );
        } else {
            let status = response.status();
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "Could not read error body".to_string());
            error!(
                "🔥 Failed to create migration for '{}'. Status: {}. Body: {}",
                url_to_mirror, status, error_body
            );
        }
    }

    info!("✨ All tasks completed.");
    Ok(())
}

fn get_repo_name_from_url(git_url: &str) -> Option<String> {
    Url::parse(git_url)
        .ok()
        .and_then(|url| url.path_segments()?.last().map(|s| s.to_string()))
        .map(|name| name.strip_suffix(".git").unwrap_or(&name).to_string())
}
