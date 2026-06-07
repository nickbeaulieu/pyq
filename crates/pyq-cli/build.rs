//! Capture the short commit sha at build time so `--help`/`--version` can show
//! exactly which build is running. Falls back to `unknown` when git isn't
//! available (e.g. a source tarball), so the env var is always set.

use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=PYQ_GIT_SHA={sha}");

    // Rebuild when the checked-out commit changes so the sha stays accurate.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
}
