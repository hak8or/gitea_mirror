//! # Gitea Mirror
//!
//! A simple command-line tool to ensure a list of remote git repositories are mirrored to a Gitea instance.
//! It checks if a repository already exists and, if not, creates a mirror migration.

use clap::Parser;
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{Level, error, info, instrument, warn};
use tracing_subscriber;

// Represents the command-line arguments.
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Path to the TOML configuration file.
    #[clap(short, long, value_parser)]
    config: PathBuf,

    /// Perform a dry run without creating any migrations.
    #[clap(short, long)]
    dry_run: bool,
}

// Represents a single repository entry in the config file.
#[derive(Deserialize, Debug, Clone)]
struct RepoConfig {
    url: String,
    rename: Option<String>,
}

// Represents the main structure of the TOML configuration file.
#[derive(Deserialize, Debug)]
struct Config {
    gitea_url: String,
    api_key: String,
    repos: Option<Vec<RepoConfig>>,
    organizations: Option<Vec<String>>,
}

// Represents the payload for creating a migration in Gitea.
#[derive(serde::Serialize, Debug)]
struct MigrateRepoPayload<'a> {
    clone_addr: &'a str,
    repo_name: &'a str,
    mirror: bool,
    private: bool,
    description: &'a str,
    uid: i64, // The user ID of the owner. We'll fetch this.
}

// Represents a user as returned by the Gitea API.
#[derive(Deserialize, Debug)]
struct GiteaUser {
    id: i64,
}

/// Entry point of the application.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize the tracing subscriber for logging.
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();

    // Parse command-line arguments or get config path from environment variable.
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(_) => {
            // If parsing fails, check for the environment variable.
            if let Ok(config_path) = env::var("GITEA_MIRROR_CONFIG") {
                Args {
                    config: PathBuf::from(config_path),
                    // Check for a dry-run env var as well, defaulting to false.
                    dry_run: env::var("GITEA_MIRROR_DRY_RUN")
                        .unwrap_or_else(|_| "false".to_string())
                        .parse()
                        .unwrap_or(false),
                }
            } else {
                // If no env var, show help and exit.
                Args::parse()
            }
        }
    };

    info!("Starting Gitea mirror process. Dry run: {}", args.dry_run);

    // Read and parse the configuration file.
    let config = load_config(&args.config)?;
    let http_client = reqwest::Client::new();

    // Fetch the Gitea user ID for the authenticated user.
    let user_id = get_gitea_user_id(&http_client, &config.gitea_url, &config.api_key).await?;
    info!(
        "Successfully authenticated and retrieved user ID: {}",
        user_id
    );

    // Process repositories from the static list.
    if let Some(repos) = &config.repos {
        for repo_config in repos {
            process_repo(
                &repo_config.url,
                repo_config.rename.as_deref(),
                user_id,
                &http_client,
                &config,
                args.dry_run,
            )
            .await?;
        }
    }

    // Process repositories from the organizations/users list.
    if let Some(org_urls) = &config.organizations {
        for org_url in org_urls {
            info!("Fetching repositories from organization: {}", org_url);
            match fetch_org_repos(&http_client, org_url, &config.api_key).await {
                Ok(repo_urls) => {
                    info!("Found {} repositories for {}", repo_urls.len(), org_url);
                    for url in repo_urls {
                        process_repo(
                            &url,
                            None, // No rename support for orgs
                            user_id,
                            &http_client,
                            &config,
                            args.dry_run,
                        )
                        .await?;
                    }
                }
                Err(e) => error!("Failed to fetch repos from {}: {}", org_url, e),
            }
        }
    }

    info!("Gitea mirror process completed.");
    Ok(())
}

/// Loads and parses the TOML configuration file.
#[instrument(skip(path))]
fn load_config(path: &Path) -> Result<Config, Box<dyn std::error::Error>> {
    info!("Loading configuration from: {:?}", path);
    let content = fs::read_to_string(path)?;
    let config: Config = toml::from_str(&content)?;
    Ok(config)
}

/// Fetches the authenticated user's ID from Gitea.
#[instrument(skip(http_client, gitea_url, api_key))]
async fn get_gitea_user_id(
    http_client: &reqwest::Client,
    gitea_url: &str,
    api_key: &str,
) -> Result<i64, reqwest::Error> {
    let url = format!("{}/api/v1/user", gitea_url);
    let user: GiteaUser = http_client
        .get(&url)
        .header("Authorization", format!("token {}", api_key))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(user.id)
}

/// Checks if a repository already exists in Gitea for the user.
#[instrument(skip(http_client, gitea_url, api_key))]
async fn repo_exists(
    http_client: &reqwest::Client,
    gitea_url: &str,
    api_key: &str,
    repo_name: &str,
) -> Result<bool, reqwest::Error> {
    let url = format!("{}/api/v1/repos/search", gitea_url);
    let response: serde_json::Value = http_client
        .get(&url)
        .query(&[("q", repo_name), ("limit", "1")])
        .header("Authorization", format!("token {}", api_key))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    if let Some(data) = response.get("data").and_then(|d| d.as_array()) {
        for repo in data {
            if let Some(name) = repo.get("name").and_then(|n| n.as_str()) {
                if name.eq_ignore_ascii_case(repo_name) {
                    return Ok(true);
                }
            }
        }
    }

    Ok(false)
}

/// Creates a mirror migration in Gitea.
#[instrument(skip(http_client, config, payload))]
async fn create_migration(
    http_client: &reqwest::Client,
    config: &Config,
    payload: &MigrateRepoPayload<'_>,
) -> Result<(), reqwest::Error> {
    let url = format!("{}/api/v1/repos/migrate", config.gitea_url);
    http_client
        .post(&url)
        .header("Authorization", format!("token {}", config.api_key))
        .json(payload)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

/// Fetches all repository clone URLs from a given Gitea/GitHub organization/user page.
#[instrument(skip(http_client, api_key))]
async fn fetch_org_repos(
    http_client: &reqwest::Client,
    org_url: &str,
    api_key: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    // This is a simplified fetcher. It assumes Gitea API compatibility.
    // For GitHub, you might need a different base URL and auth method.
    let api_url = if org_url.contains("github.com") {
        let parts: Vec<&str> = org_url.trim_end_matches('/').split('/').collect();
        let user_or_org = parts.last().ok_or("Invalid GitHub URL")?;
        format!("https://api.github.com/users/{}/repos", user_or_org)
    } else {
        // Assuming Gitea-like URL structure
        let parts: Vec<&str> = org_url.trim_end_matches('/').split('/').collect();
        let user_or_org = parts.last().ok_or("Invalid Gitea URL")?;
        format!(
            "{}s/{}/repos",
            org_url.replace(user_or_org, &format!("api/v1/user")),
            user_or_org
        )
    };

    info!("Querying API endpoint: {}", api_url);

    let mut repos: Vec<String> = Vec::new();
    let mut page = 1;
    loop {
        let response: Vec<serde_json::Value> = http_client
            .get(&api_url)
            .query(&[("page", page.to_string())])
            // For GitHub, a User-Agent is required.
            .header("User-Agent", "gitea-mirror-rust-client")
            .header("Authorization", format!("token {}", api_key))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if response.is_empty() {
            break; // No more pages
        }

        for repo in response {
            if let Some(clone_url) = repo.get("clone_url").and_then(|u| u.as_str()) {
                repos.push(clone_url.to_string());
            }
        }
        page += 1;
    }

    Ok(repos)
}

/// Core logic to process a single repository.
#[instrument(skip(user_id, http_client, config, dry_run))]
async fn process_repo(
    repo_url: &str,
    rename: Option<&str>,
    user_id: i64,
    http_client: &reqwest::Client,
    config: &Config,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let repo_name = match rename {
        Some(name) => name,
        None => extract_repo_name(repo_url).ok_or("Could not extract repo name from URL")?,
    };

    info!("Processing repo '{}' -> '{}'", repo_url, repo_name);

    if repo_exists(http_client, &config.gitea_url, &config.api_key, repo_name).await? {
        info!("Repo '{}' already exists. Skipping.", repo_name);
    } else {
        warn!("Repo '{}' does not exist. Migration needed.", repo_name);
        if !dry_run {
            info!("Initiating migration for '{}'...", repo_name);
            let payload = MigrateRepoPayload {
                clone_addr: repo_url,
                repo_name,
                mirror: true,
                private: false, // Defaulting to public, change if needed
                description: "",
                uid: user_id,
            };
            if let Err(e) = create_migration(http_client, config, &payload).await {
                error!("Failed to create migration for '{}': {}", repo_name, e);
            } else {
                info!("Successfully started migration for '{}'.", repo_name);
            }
        } else {
            info!(
                "Dry run enabled. Skipping actual migration for '{}'.",
                repo_name
            );
        }
    }
    Ok(())
}

/// Extracts a repository name from a git URL (e.g., "https://.../repo.git" -> "repo").
fn extract_repo_name(url: &str) -> Option<&str> {
    url.split('/').last().map(|s| s.trim_end_matches(".git"))
}
