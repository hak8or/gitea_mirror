use clap::Parser;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tracing::{Level, error, info, instrument, warn};

#[derive(Parser, Debug)]
#[command(name = "gitea-mirror")]
#[command(about = "Syncs Git repositories to Gitea based on a TOML config.")]
struct Args {
    /// Path to the TOML configuration file.
    #[clap(short, long, value_parser, env = "GITEA_MIRROR_CONFIG_FILEPATH")]
    config: PathBuf,

    /// Gitea API Key.
    #[clap(short, long, env = "GITEA_MIRROR_API_KEY")]
    api_key: Option<String>,

    /// Calculate the plan but do not execute API calls.
    #[clap(short, long, default_value_t = false)]
    dry_run: bool,

    /// Skip the interactive confirmation prompt.
    #[clap(long, default_value_t = false)]
    no_confirm: bool,

    /// Do not delete repositories from Gitea.
    #[clap(long, default_value_t = false)]
    no_delete: bool,
}

#[derive(Deserialize, Debug, Clone)]
struct RepoConfig {
    url: String,
    rename: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct OrgConfig {
    url: String,
    api_key: Option<String>,
}

#[derive(Deserialize, Debug)]
struct Config {
    gitea_url: String,
    api_key: Option<String>,
    repos: Option<Vec<RepoConfig>>,
    organizations: Option<Vec<OrgConfig>>,
    repo_owner: Option<String>,
}

#[derive(serde::Serialize, Debug)]
struct MigrateRepoPayload<'a> {
    clone_addr: &'a str,
    repo_name: &'a str,
    repo_owner: &'a str,
    mirror: bool,
    private: bool,
    description: &'a str,
}

#[derive(Deserialize, Debug)]
struct GiteaUser {
    login: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();
    let args = Args::parse();
    let config = load_config(&args.config)?;
    let http_client = reqwest::Client::new();

    // Resolve API Key: CLI/Env > Config File
    let final_api_key = args
        .api_key
        .or(config.api_key.clone())
        .ok_or("API Key must be provided via --api-key, GITEA_MIRROR_API_KEY, or config file.")?;

    // 1. Determine Target Owner
    let owner_name = if let Some(owner) = &config.repo_owner {
        owner.clone()
    } else {
        get_authenticated_username(&http_client, &config.gitea_url, &final_api_key).await?
    };
    info!("Target Owner: {}", owner_name);

    // 2. Build 'Desired' State (Map<RepoName, CloneUrl>)
    info!("Resolving desired state from configuration...");
    let mut desired_repos: HashMap<String, String> = HashMap::new();

    // 2a. Static Repos
    if let Some(repos) = &config.repos {
        for r in repos {
            let name = r
                .rename
                .as_deref()
                .or_else(|| extract_repo_name(&r.url))
                .ok_or_else(|| format!("Invalid URL: {}", r.url))?;
            desired_repos.insert(name.to_string(), r.url.clone());
        }
    }

    // 2b. Organization Repos
    if let Some(orgs) = &config.organizations {
        for org in orgs {
            info!("Fetching repos from source: {}", org.url);
            let urls =
                fetch_external_org_repos(&http_client, &org.url, org.api_key.as_deref()).await?;
            for url in urls {
                if let Some(name) = extract_repo_name(&url) {
                    desired_repos.insert(name.to_string(), url);
                }
            }
        }
    }

    // 3. Build 'Current' State (Set<RepoName>)
    info!("Fetching existing repositories from Gitea ({})", owner_name);
    let existing_repos =
        fetch_all_target_repos(&http_client, &config.gitea_url, &final_api_key, &owner_name)
            .await?;
    let existing_set: HashSet<String> = existing_repos.into_iter().collect();

    // 4. Calculate Diff
    let mut to_add: Vec<(String, String)> = desired_repos
        .iter()
        .filter(|(name, _)| !existing_set.contains(*name))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Sort for consistent output
    to_add.sort_by(|a, b| a.0.cmp(&b.0));

    let mut to_delete: Vec<String> = existing_set
        .iter()
        .filter(|name| !desired_repos.contains_key(*name))
        .cloned()
        .collect();

    to_delete.sort();

    let mut to_keep: Vec<String> = desired_repos
        .keys()
        .filter(|name| existing_set.contains(*name))
        .cloned()
        .collect();

    to_keep.sort();

    // 5. Present Plan
    println!("\n--- Execution Plan ---");
    for name in &to_keep {
        println!("  [=] KEEP:   {}", name);
    }
    for (name, url) in &to_add {
        println!("  [+] ADD:    {}  (Source: {})", name, url);
    }
    for name in &to_delete {
        if args.no_delete {
            println!("  [~] SKIP DELETE: {} (--no-delete active)", name);
        } else {
            println!("  [-] DELETE: {}", name);
        }
    }
    println!("----------------------");

    if args.no_delete {
        println!(
            "Summary: {} to add, {} to delete (SKIPPED), {} unchanged.",
            to_add.len(),
            to_delete.len(),
            to_keep.len()
        );
    } else {
        println!(
            "Summary: {} to add, {} to delete, {} unchanged.",
            to_add.len(),
            to_delete.len(),
            to_keep.len()
        );
    }

    // If nothing to add, and (deletes are empty OR we are skipping deletes), then done.
    if to_add.is_empty() && (to_delete.is_empty() || args.no_delete) {
        info!("Sync complete. No changes to apply.");
        return Ok(());
    }

    // 6. Confirmation / Dry Run
    if args.dry_run {
        info!("Dry run enabled. Exiting without changes.");
        return Ok(());
    }

    if !args.no_confirm {
        print!("\nProceed with these changes? [y/N]: ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            info!("Aborted by user.");
            return Ok(());
        }
    }

    // 7. Execute
    // Additions
    for (name, url) in to_add {
        info!("Migrating {}...", name);
        let payload = MigrateRepoPayload {
            clone_addr: &url,
            repo_name: &name,
            repo_owner: &owner_name,
            mirror: true,
            private: false,
            description: "Mirrored via gitea-mirror",
        };

        match create_migration(&http_client, &config.gitea_url, &final_api_key, &payload).await {
            Ok(_) => info!("Successfully migrated {}", name),
            Err(e) => error!("Failed to migrate {}: {}", name, e),
        }
    }

    // Deletions
    if !args.no_delete {
        for name in to_delete {
            info!("Deleting {}...", name);
            match delete_repo(
                &http_client,
                &config.gitea_url,
                &final_api_key,
                &owner_name,
                &name,
            )
            .await
            {
                Ok(_) => info!("Successfully deleted {}", name),
                Err(e) => error!("Failed to delete {}: {}", name, e),
            }
        }
    } else if !to_delete.is_empty() {
        info!("Skipping deletions due to --no-delete flag.");
    }

    info!("Process completed.");
    Ok(())
}

// --- Helpers ---

#[instrument(skip(path))]
fn load_config(path: &Path) -> Result<Config, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)?;
    let config: Config = toml::from_str(&content)?;
    Ok(config)
}

fn extract_repo_name(url: &str) -> Option<&str> {
    url.split('/').next_back().map(|s| s.trim_end_matches(".git"))
}

// --- API Calls ---

async fn get_authenticated_username(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<String, reqwest::Error> {
    let url = format!("{}/api/v1/user", base_url);
    let user: GiteaUser = client
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(user.login)
}

/// Fetches ALL repos for the target owner on the Gitea instance.
async fn fetch_all_target_repos(
    client: &reqwest::Client,
    gitea_url: &str,
    api_key: &str,
    owner: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let org_url = format!("{}/api/v1/orgs/{}/repos", gitea_url, owner);
    match fetch_repos_from_endpoint(client, &org_url, api_key).await {
        Ok(repos) => Ok(repos),
        Err(e) => {
            if e.downcast_ref::<reqwest::Error>()
                .is_some_and(|r| r.status() == Some(reqwest::StatusCode::NOT_FOUND))
            {
                info!("Owner '{}' not found as org, trying as user...", owner);
                let user_url = format!("{}/api/v1/users/{}/repos", gitea_url, owner);
                return fetch_repos_from_endpoint(client, &user_url, api_key).await;
            }
            Err(e)
        }
    }
}

async fn fetch_repos_from_endpoint(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut names = Vec::new();
    let mut page = 1;

    loop {
        let params = [("limit", "50"), ("page", &page.to_string())];

        let res = client
            .get(url)
            .bearer_auth(api_key)
            .query(&params)
            .send()
            .await?
            .error_for_status()?;

        let json: serde_json::Value = res.json().await?;
        let data = json.as_array().ok_or("Invalid API response")?;

        if data.is_empty() {
            break;
        }

        for repo in data {
            if let Some(name) = repo.get("name").and_then(|n| n.as_str()) {
                names.push(name.to_string());
            }
        }
        page += 1;
    }
    Ok(names)
}

/// Fetches clone URLs from external source (GitHub/Gitea).
async fn fetch_external_org_repos(
    client: &reqwest::Client,
    org_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let api_url = if org_url.contains("github.com") {
        let parts: Vec<&str> = org_url.trim_end_matches('/').split('/').collect();
        let user_or_org = parts.last().ok_or("Invalid GitHub URL")?;
        format!("https://api.github.com/users/{}/repos", user_or_org)
    } else {
        // Assuming Gitea
        let parts: Vec<&str> = org_url.trim_end_matches('/').split('/').collect();
        let user_or_org = parts.last().ok_or("Invalid Gitea URL")?;
        // Heuristic to find API endpoint from web URL
        format!(
            "{}s/{}/repos",
            org_url.replace(user_or_org, "api/v1/user"),
            user_or_org
        )
    };

    let mut repos = Vec::new();
    let mut page = 1;

    loop {
        let mut req = client
            .get(&api_url)
            .query(&[("page", page.to_string())])
            .header("User-Agent", "gitea-mirror-rust");

        if let Some(key) = api_key {
            req = req.bearer_auth(key);
        }

        let res = req.send().await?.error_for_status()?;
        let json: Vec<serde_json::Value> = res.json().await?;

        if json.is_empty() {
            break;
        }

        for repo in json {
            if let Some(url) = repo.get("clone_url").and_then(|u| u.as_str()) {
                repos.push(url.to_string());
            }
        }
        page += 1;
    }

    Ok(repos)
}

async fn create_migration(
    client: &reqwest::Client,
    gitea_url: &str,
    api_key: &str,
    payload: &MigrateRepoPayload<'_>,
) -> Result<(), reqwest::Error> {
    let url = format!("{}/api/v1/repos/migrate", gitea_url);
    client
        .post(&url)
        .bearer_auth(api_key)
        .json(payload)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn delete_repo(
    client: &reqwest::Client,
    gitea_url: &str,
    api_key: &str,
    owner: &str,
    repo_name: &str,
) -> Result<(), reqwest::Error> {
    let url = format!("{}/api/v1/repos/{}/{}", gitea_url, owner, repo_name);
    client
        .delete(&url)
        .bearer_auth(api_key)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}
