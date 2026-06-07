//! `pyq upgrade` — pull the latest build on the configured channel, verify its
//! checksum, and replace the running binary in place.
//!
//! Stable follows tagged `v*` releases (compared by semver); canary follows the
//! rolling `canary` release that tracks `main` (compared by the embedded commit
//! sha, since the tag itself never moves). Either way the asset fetched is the
//! one named for this build's exact target triple (`PYQ_TARGET`), and its
//! `.sha256` sidecar is verified before anything touches the installed binary.

use crate::channel::{self, ReleaseChannel};
use anyhow::{anyhow, bail, Context, Result};
use pyq_output::Envelope;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

const REPO: &str = "nickbeaulieu/pyq";
/// Cap a download so a bad URL can't stream forever (the binary is a few MB).
const MAX_ASSET_BYTES: u64 = 200 * 1024 * 1024;

/// A release resolved against the GitHub API, narrowed to this build's target.
struct Release {
    tag: String,
    /// The published commit sha for a canary build, parsed from the release
    /// body marker; `None` for stable, which compares by tag.
    sha: Option<String>,
    tarball_url: String,
    sha256_url: String,
}

/// Run the upgrade. `check` reports without installing; `force` reinstalls even
/// when already current.
pub fn run(check: bool, force: bool) -> Result<Envelope> {
    let ch = channel::configured();
    let target = env!("PYQ_TARGET");
    if target == "unknown" {
        bail!("this build has no target triple recorded — `upgrade` can't pick an asset");
    }

    let release = fetch_release(ch, target)?;
    let (current, latest, behind) = compare(ch, &release);

    let q = json!({
        "kind": "upgrade",
        "channel": ch.as_str(),
        "current": current,
        "latest": latest,
        "target": target,
    });

    if !behind && !force {
        return Ok(Envelope::new(q, Vec::new())
            .with_summary(format!("already up to date on {} ({current})", ch.as_str())));
    }

    if check {
        let verb = if behind { "available" } else { "reinstallable" };
        return Ok(Envelope::new(q, Vec::new()).with_summary(format!(
            "upgrade {verb}: {current} → {latest} on {}\nrun `pyq upgrade` to install",
            ch.as_str()
        )));
    }

    install(&release)?;
    Ok(Envelope::new(q, Vec::new())
        .with_summary(format!("upgraded {current} → {latest} on {}", ch.as_str())))
}

/// Compare this build against the release, returning `(current, latest,
/// is-behind)` display strings. Stable compares semver; canary compares sha.
fn compare(ch: ReleaseChannel, release: &Release) -> (String, String, bool) {
    match ch {
        ReleaseChannel::Stable => {
            let current = env!("CARGO_PKG_VERSION").to_string();
            let latest = release.tag.trim_start_matches('v').to_string();
            let behind = is_newer(&latest, &current);
            (current, latest, behind)
        }
        ReleaseChannel::Canary => {
            let current = env!("PYQ_GIT_SHA").to_string();
            let latest = release
                .sha
                .clone()
                .unwrap_or_else(|| release.tag.clone());
            // Compare on the shorter sha's length — the build's `git --short` sha
            // and the workflow's 7-char prefix needn't be the same width. The
            // canary tag only rolls forward, so a differing prefix means behind.
            let n = current.len().min(latest.len());
            let behind =
                latest != "unknown" && n > 0 && current.get(..n) != latest.get(..n);
            (current, latest, behind)
        }
    }
}

/// Resolve the channel's release and the two asset URLs for `target`.
fn fetch_release(ch: ReleaseChannel, target: &str) -> Result<Release> {
    let url = match ch {
        ReleaseChannel::Stable => format!("https://api.github.com/repos/{REPO}/releases/latest"),
        ReleaseChannel::Canary => {
            format!("https://api.github.com/repos/{REPO}/releases/tags/canary")
        }
    };
    let body = http_json(&url).with_context(|| format!("querying {url}"))?;

    let tag = body
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            let hint = if ch == ReleaseChannel::Stable {
                " — no stable release exists yet; try `pyq channel canary`"
            } else {
                ""
            };
            anyhow!("no {} release found on {REPO}{hint}", ch.as_str())
        })?
        .to_string();

    let assets = body
        .get("assets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // Consumers match assets by target suffix rather than reconstructing the
    // full name, so the version-in-name detail stays the workflow's business.
    let find = |suffix: &str| {
        assets.iter().find_map(|a| {
            let name = a.get("name").and_then(Value::as_str)?;
            name.ends_with(suffix)
                .then(|| a.get("browser_download_url").and_then(Value::as_str))
                .flatten()
                .map(str::to_string)
        })
    };
    let tarball_url = find(&format!("-{target}.tar.gz"))
        .ok_or_else(|| anyhow!("the {} release has no asset for target {target}", ch.as_str()))?;
    let sha256_url = find(&format!("-{target}.tar.gz.sha256"))
        .ok_or_else(|| anyhow!("the {} release is missing the .sha256 for {target}", ch.as_str()))?;

    let sha = body
        .get("body")
        .and_then(Value::as_str)
        .and_then(parse_sha_marker);

    Ok(Release {
        tag,
        sha,
        tarball_url,
        sha256_url,
    })
}

/// Download, checksum-verify, extract, and atomically replace the live binary.
fn install(release: &Release) -> Result<()> {
    let tarball = http_bytes(&release.tarball_url).context("downloading the release tarball")?;
    let sha_text =
        String::from_utf8(http_bytes(&release.sha256_url)?).context("reading the .sha256 sidecar")?;
    let expected = sha_text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if expected.len() != 64 {
        bail!("malformed .sha256 sidecar: {sha_text:?}");
    }

    let mut hasher = Sha256::new();
    hasher.update(&tarball);
    let got = hex(&hasher.finalize());
    if got != expected {
        bail!("checksum mismatch: expected {expected}, got {got} — refusing to install");
    }

    let dir = tempfile::tempdir().context("creating a temp dir for the download")?;
    let tar_path = dir.path().join("pyq.tar.gz");
    std::fs::write(&tar_path, &tarball).context("writing the tarball to disk")?;

    // Extract with the system `tar` (universally present on macOS/Linux; keeps
    // flate2/tar out of the dependency graph). The archive holds a single `pyq`.
    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tar_path)
        .arg("-C")
        .arg(dir.path())
        .status()
        .context("running tar to extract the archive")?;
    if !status.success() {
        bail!("tar exited non-zero extracting the release archive");
    }

    let new_bin = dir.path().join("pyq");
    if !new_bin.exists() {
        bail!("the release archive did not contain a `pyq` binary");
    }
    set_executable(&new_bin)?;

    self_replace::self_replace(&new_bin)
        .context("replacing the running binary (is it on a read-only volume, or unwritable?)")?;
    Ok(())
}

/// Extract the short sha from a `pyq-sha: <sha>` line in the release body — the
/// canary comparison key, written there by the release workflow.
fn parse_sha_marker(body: &str) -> Option<String> {
    body.lines()
        .find_map(|l| l.trim().strip_prefix("pyq-sha:").map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
}

/// Whether `remote` is a strictly newer semver than `local`, comparing
/// `major.minor.patch` numerically (pre-release/build suffixes ignored — good
/// enough pre-1.0, where the workflow only ever publishes forward).
fn is_newer(remote: &str, local: &str) -> bool {
    parse_ver(remote) > parse_ver(local)
}

fn parse_ver(s: &str) -> [u64; 3] {
    let mut out = [0u64; 3];
    for (i, part) in s
        .trim()
        .trim_start_matches('v')
        .split(['.', '-', '+'])
        .take(3)
        .enumerate()
    {
        out[i] = part.parse().unwrap_or(0);
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// A GET against the GitHub API / asset host, with the User-Agent GitHub
/// requires and an optional token (lifts the unauthenticated rate limit).
fn agent_get(url: &str) -> Result<ureq::Response> {
    let mut req = ureq::get(url).set("User-Agent", concat!("pyq/", env!("CARGO_PKG_VERSION")));
    if let Some(tok) = std::env::var("GITHUB_TOKEN")
        .ok()
        .or_else(|| std::env::var("GH_TOKEN").ok())
        .filter(|t| !t.is_empty())
    {
        req = req.set("Authorization", &format!("Bearer {tok}"));
    }
    req.call().map_err(|e| anyhow!("{e}"))
}

fn http_json(url: &str) -> Result<Value> {
    agent_get(url)?.into_json::<Value>().context("decoding the JSON response")
}

fn http_bytes(url: &str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    agent_get(url)?
        .into_reader()
        .take(MAX_ASSET_BYTES)
        .read_to_end(&mut buf)
        .context("downloading the asset")?;
    Ok(buf)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).context("marking the new binary executable")
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_compares_numerically() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn sha_marker_extracted_from_body() {
        let body = "Rolling canary build.\n\npyq-sha: a1b2c3d\nbuilt 2026-06-07\n";
        assert_eq!(parse_sha_marker(body), Some("a1b2c3d".to_string()));
        assert_eq!(parse_sha_marker("no marker here"), None);
    }
}
