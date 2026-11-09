//! Tells the viewer when a newer release is out: one request to GitHub at startup, off the UI
//! thread. Any failure (offline, rate limited, GitHub changed) just means no notice.

use eframe::egui::Context;
use std::sync::OnceLock;
use std::time::Duration;

const LATEST_API: &str = "https://api.github.com/repos/MOmerTepe/openteslacam/releases/latest";
/// Where the notice sends people: the release with its installer.
pub const RELEASE_PAGE: &str = "https://github.com/MOmerTepe/openteslacam/releases/latest";
const USER_AGENT: &str = concat!("OpenTeslaCam/", env!("CARGO_PKG_VERSION"));

static NEWER: OnceLock<String> = OnceLock::new();

/// The newer version's number, once the check found one.
pub fn available() -> Option<&'static str> {
    NEWER.get().map(String::as_str)
}

/// Starts the check; `ctx` repaints when a newer version turns up.
pub fn check(ctx: Context) {
    std::thread::spawn(move || {
        if let Some(tag) = latest_tag().filter(|t| newer(t, env!("CARGO_PKG_VERSION"))) {
            let _ = NEWER.set(tag.trim_start_matches('v').to_owned());
            ctx.request_repaint();
        }
    });
}

fn latest_tag() -> Option<String> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
    let body = rt.block_on(async {
        let client = reqwest::Client::builder().user_agent(USER_AGENT).timeout(Duration::from_secs(15)).build()?;
        client.get(LATEST_API).header("Accept", "application/vnd.github+json").send().await?.error_for_status()?.text().await
    });
    let json: serde_json::Value = serde_json::from_str(&body.ok()?).ok()?;
    json["tag_name"].as_str().map(str::to_owned)
}

/// `tag` (`v2.0.1` or `2.0.1`) is a later release than `current`. Anything that isn't plain numbers,
/// like a pre-release, never is.
pub fn newer(tag: &str, current: &str) -> bool {
    let parse = |v: &str| v.trim_start_matches('v').split('.').map(|n| n.parse::<u64>().ok()).collect::<Option<Vec<u64>>>();
    matches!((parse(tag), parse(current)), (Some(t), Some(c)) if t > c)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Network: the real API answers with a release tag. `cargo test update -- --ignored`
    #[test]
    #[ignore]
    fn github_names_the_latest_release() {
        let tag = latest_tag().expect("an answer from GitHub");
        assert!(tag.starts_with('v') && !newer("v0.0.1", &tag), "{tag}");
    }

    #[test]
    fn only_a_later_plain_release_counts() {
        assert!(newer("v2.0.1", "2.0.0"));
        assert!(newer("2.1.0", "2.0.9"));
        assert!(newer("v2.0.10", "2.0.9"), "numbers, not text");
        assert!(newer("v3.0.0", "2.9.9"));
        assert!(!newer("v2.0.1", "2.0.1"));
        assert!(!newer("v2.0.0", "2.0.1"));
        assert!(!newer("v2.1.0-beta", "2.0.1"));
        assert!(!newer("", "2.0.1"));
        assert!(!newer("latest", "2.0.1"));
    }
}
