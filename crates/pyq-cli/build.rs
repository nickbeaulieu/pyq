//! Capture build-time identity so `pyq --version`, `pyq channel`, and `pyq
//! upgrade` know exactly which build is running and how to ask for a newer one:
//!
//!   * `PYQ_GIT_SHA`    — short commit sha (the canary comparison key).
//!   * `PYQ_BUILD_DATE` — UTC `YYYY-MM-DD` the binary was built.
//!   * `PYQ_CHANNEL`    — the release channel that produced it (`stable` /
//!                        `canary`), or `dev` for an unmarked local build.
//!   * `PYQ_TARGET`     — the target triple, so the updater asks for the asset
//!                        named for exactly this build (`pyq-<ver>-<target>...`).
//!
//! The release workflow sets `PYQ_CHANNEL`/`PYQ_BUILD_DATE` in the environment;
//! everything falls back to a sensible local-build default so the env vars are
//! always present for `env!`.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

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

    // Channel: the workflow exports it; a local build is `dev`.
    let channel = std::env::var("PYQ_CHANNEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "dev".to_string());
    println!("cargo:rustc-env=PYQ_CHANNEL={channel}");

    // Build date: honor an explicit override (reproducible/CI builds), else the
    // current UTC day computed without pulling in a date crate.
    let date = std::env::var("PYQ_BUILD_DATE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(today_utc);
    println!("cargo:rustc-env=PYQ_BUILD_DATE={date}");

    // Cargo hands build scripts the resolved target triple — the one truth the
    // asset name must match.
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=PYQ_TARGET={target}");

    // Rebuild when the checked-out commit or the injected env changes so the
    // embedded identity stays accurate.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
    println!("cargo:rerun-if-env-changed=PYQ_CHANNEL");
    println!("cargo:rerun-if-env-changed=PYQ_BUILD_DATE");
}

/// Today's date in UTC as `YYYY-MM-DD`, derived from the wall clock with the
/// standard days-since-epoch → civil-date algorithm (Howard Hinnant's
/// `civil_from_days`). Avoids a date-crate dependency in the build graph.
fn today_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;

    // civil_from_days: shift the epoch to 0000-03-01 so leap years sit at the
    // end of each 400/100/4-year era, then unwind the eras back to y/m/d.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}")
}
