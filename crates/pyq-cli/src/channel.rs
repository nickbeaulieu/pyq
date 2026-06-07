//! Release-channel state shared by `pyq channel` and `pyq upgrade`.
//!
//! The configured channel lives in `~/.pyq/channel` — seeded by the install
//! script, switched by `pyq channel <name>`, and read by `pyq upgrade` to know
//! which release line to pull from. `PYQ_CHANNEL` (baked in at build time)
//! records which channel *produced* this binary; the configured channel is what
//! the next upgrade will follow, and the two can legitimately differ (a `dev`
//! build whose owner has chosen to track `canary`, say).

use anyhow::{bail, Context, Result};
use pyq_output::Envelope;
use serde_json::json;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseChannel {
    /// Tracks tagged `v*` releases — the latest published semver.
    Stable,
    /// Tracks the rolling `canary` release that follows `main`, by commit sha.
    Canary,
}

impl ReleaseChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            ReleaseChannel::Stable => "stable",
            ReleaseChannel::Canary => "canary",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "stable" => Ok(ReleaseChannel::Stable),
            "canary" => Ok(ReleaseChannel::Canary),
            other => bail!("unknown channel {other:?} — expected `stable` or `canary`"),
        }
    }
}

/// `~/.pyq/channel`. `PYQ_CONFIG_DIR` overrides the `~/.pyq` base (tests,
/// sandboxes), mirroring `PYQ_CACHE_DIR` in `cache.rs`.
pub fn channel_file() -> Option<PathBuf> {
    let base = match std::env::var_os("PYQ_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => dirs::home_dir()?.join(".pyq"),
    };
    Some(base.join("channel"))
}

/// The configured channel: the `~/.pyq/channel` file if present and valid, else
/// the channel this binary was built on (so a canary build defaults to canary),
/// else `stable`.
pub fn configured() -> ReleaseChannel {
    if let Some(path) = channel_file() {
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Ok(ch) = ReleaseChannel::parse(&s) {
                return ch;
            }
        }
    }
    ReleaseChannel::parse(env!("PYQ_CHANNEL")).unwrap_or(ReleaseChannel::Stable)
}

/// Persist the configured channel to `~/.pyq/channel`, creating `~/.pyq` if
/// needed.
pub fn set(ch: ReleaseChannel) -> Result<()> {
    let path = channel_file().context("could not resolve the ~/.pyq config dir (no home?)")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, format!("{}\n", ch.as_str()))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// The `channel` verb. With no argument, report the configured channel and this
/// build's identity; with `stable`/`canary`, switch and confirm. Never touches
/// the network — `pyq upgrade --check` is the online "am I behind."
pub fn query(arg: Option<&str>) -> Result<Envelope> {
    match arg {
        Some(name) => {
            let ch = ReleaseChannel::parse(name)?;
            set(ch)?;
            let q = json!({ "kind": "channel", "set": ch.as_str() });
            Ok(Envelope::new(q, Vec::new()).with_summary(format!(
                "channel set to {0} — run `pyq upgrade` to move to the latest {0} build",
                ch.as_str()
            )))
        }
        None => {
            let ch = configured();
            let q = json!({
                "kind": "channel",
                "channel": ch.as_str(),
                "build": {
                    "version": env!("CARGO_PKG_VERSION"),
                    "channel": env!("PYQ_CHANNEL"),
                    "date": env!("PYQ_BUILD_DATE"),
                    "sha": env!("PYQ_GIT_SHA"),
                    "target": env!("PYQ_TARGET"),
                }
            });
            Ok(Envelope::new(q, Vec::new()).with_summary(format!(
                "channel: {}\nbuild:   pyq {} ({} {} {})",
                ch.as_str(),
                env!("CARGO_PKG_VERSION"),
                env!("PYQ_CHANNEL"),
                env!("PYQ_BUILD_DATE"),
                env!("PYQ_GIT_SHA"),
            )))
        }
    }
}
