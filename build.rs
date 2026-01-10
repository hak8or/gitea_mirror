use std::process::Command;

fn main() {
    // Only re-run if .git state changes
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/tags");

    let output = Command::new("git")
        .args(["describe", "--tags", "--exact-match"])
        .output();

    let version = if let Ok(output) = output {
        if output.status.success() {
            // Exact tag match
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        } else {
            // Not an exact match, construct version string
            let tag_output = Command::new("git")
                .args(["describe", "--tags", "--abbrev=0"])
                .output()
                .expect("Failed to execute git describe");
            
            let sha_output = Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .expect("Failed to execute git rev-parse");

            if tag_output.status.success() && sha_output.status.success() {
                let tag = String::from_utf8(tag_output.stdout).unwrap().trim().to_string();
                let sha = String::from_utf8(sha_output.stdout).unwrap().trim().to_string();
                format!("{}-g{}", tag, sha)
            } else {
                // Fallback if git fails or no tags
                "unknown".to_string()
            }
        }
    } else {
        "unknown".to_string()
    };

    println!("cargo:rustc-env=GIT_VERSION={}", version);
}
