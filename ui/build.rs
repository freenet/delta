fn main() {
    // Embed build timestamp in ISO 8601 format so the UI can convert to local time
    let now = chrono::Utc::now();
    println!(
        "cargo:rustc-env=BUILD_TIMESTAMP_ISO={}",
        now.format("%Y-%m-%dT%H:%M:%SZ")
    );

    // Get git commit hash if available
    let git_hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_COMMIT={git_hash}");
}
