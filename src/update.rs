//! Updates: one request to GitHub at startup says whether a newer release is out and what's in it,
//! off the UI thread. Any failure there (offline, rate limited, GitHub changed) just means no notice.
//! An installed copy can then fetch that release's setup and let it replace the app in place.

use eframe::egui::{Context, ViewportCommand};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const LATEST_API: &str = "https://api.github.com/repos/MOmerTepe/openteslacam/releases/latest";
/// Where people without an installed copy are sent: the release with its installer.
pub const RELEASE_PAGE: &str = "https://github.com/MOmerTepe/openteslacam/releases/latest";
/// A setup is only ever fetched from this repo's releases.
const DOWNLOADS: &str = "https://github.com/MOmerTepe/openteslacam/releases/download/";
const USER_AGENT: &str = concat!("OpenTeslaCam/", env!("CARGO_PKG_VERSION"));
/// Setup puts this next to the app; a copy without it wasn't installed and isn't ours to replace.
const UNINSTALL_EXE: &str = "uninstall.exe";

#[derive(Debug, Clone, PartialEq)]
pub struct Setup {
    url: String,
    size: u64,
    /// Lowercase hex, as GitHub publishes it for the file.
    sha256: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    /// Without the tag's `v`.
    pub version: String,
    /// The release's description, as written (Markdown).
    pub notes: String,
    /// None when the release has no setup this app would trust.
    setup: Option<Setup>,
}

static NEWER: OnceLock<Release> = OnceLock::new();

/// The newer release, once the check found one.
pub fn available() -> Option<&'static Release> {
    NEWER.get()
}

/// Starts the check; `ctx` repaints when a newer version turns up.
pub fn check(ctx: Context) {
    std::thread::spawn(move || {
        clean_up();
        // Debug builds only: with OPENTESLACAM_FAKE_UPDATE set, the latest release counts as newer, to see the dialog.
        let fake = cfg!(debug_assertions) && std::env::var_os("OPENTESLACAM_FAKE_UPDATE").is_some();
        if let Some(r) = latest().filter(|r| fake || newer(&r.version, env!("CARGO_PKG_VERSION"))) {
            let _ = NEWER.set(r);
            ctx.request_repaint();
        }
    });
}

fn client() -> reqwest::Result<reqwest::Client> {
    // No total timeout: a setup is tens of MB. A stalled connection still ends.
    reqwest::Client::builder().user_agent(USER_AGENT).connect_timeout(Duration::from_secs(15)).read_timeout(Duration::from_secs(30)).build()
}

fn latest() -> Option<Release> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
    let body = rt.block_on(async { client()?.get(LATEST_API).header("Accept", "application/vnd.github+json").send().await?.error_for_status()?.text().await });
    parse(&serde_json::from_str(&body.ok()?).ok()?)
}

/// GitHub's answer for a release. The setup is the `.exe` it serves from this repo with a SHA-256.
fn parse(json: &serde_json::Value) -> Option<Release> {
    let setup = json["assets"].as_array().into_iter().flatten().find_map(|a| {
        let url = a["browser_download_url"].as_str().filter(|u| u.starts_with(DOWNLOADS) && u.ends_with(".exe"))?;
        let sha256 = a["digest"].as_str()?.strip_prefix("sha256:").filter(|h| h.len() == 64)?;
        Some(Setup { url: url.to_owned(), size: a["size"].as_u64()?, sha256: sha256.to_ascii_lowercase() })
    });
    Some(Release { version: json["tag_name"].as_str()?.trim_start_matches('v').to_owned(), notes: json["body"].as_str().unwrap_or_default().to_owned(), setup })
}

/// `tag` (`v2.0.1` or `2.0.1`) is a later release than `current`. Anything that isn't plain numbers,
/// like a pre-release, never is.
pub fn newer(tag: &str, current: &str) -> bool {
    let parse = |v: &str| v.trim_start_matches('v').split('.').map(|n| n.parse::<u64>().ok()).collect::<Option<Vec<u64>>>();
    matches!((parse(tag), parse(current)), (Some(t), Some(c)) if t > c)
}

/// Where an in-app update stands.
#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Idle,
    /// 0..=1 of the setup fetched.
    Downloading(f32),
    Installing,
    /// The new version is starting and this window is closing.
    Restarting,
    Failed(String),
}

static STATE: Mutex<State> = Mutex::new(State::Idle);

pub fn state() -> State {
    STATE.lock().map_or(State::Idle, |s| s.clone())
}

fn set(s: State, ctx: &Context) {
    if let Ok(mut state) = STATE.lock() {
        *state = s;
    }
    ctx.request_repaint();
}

/// This copy's exe, if setup installed it: only then can setup replace it.
fn installed_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.with_file_name(UNINSTALL_EXE).exists().then_some(exe)
}

/// Whether `release` can be installed from inside the app; otherwise the release page is the way.
pub fn can_install(release: &Release) -> bool {
    cfg!(windows) && release.setup.is_some() && installed_exe().is_some()
}

/// Fetches the release's setup and lets it replace this app, then starts the new version and closes
/// this one. Progress and failure show in `state`.
pub fn install(release: &'static Release, ctx: Context) {
    if matches!(state(), State::Downloading(_) | State::Installing | State::Restarting) {
        return;
    }
    set(State::Downloading(0.0), &ctx);
    std::thread::spawn(move || match run(release, &ctx) {
        Ok(()) => {
            set(State::Restarting, &ctx);
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        Err(e) => set(State::Failed(e), &ctx),
    });
}

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join("openteslacam-update")
}

/// The name this exe takes while setup writes the new one: Windows lets a running exe be renamed, not replaced.
fn old_name(exe: &Path) -> PathBuf {
    exe.with_added_extension("old")
}

/// What the last update left behind. The old exe can only go once that process has ended.
fn clean_up() {
    let _ = std::fs::remove_dir_all(temp_dir());
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(old_name(&exe));
    }
}

fn run(release: &Release, ctx: &Context) -> Result<(), String> {
    let (Some(setup), Some(exe)) = (&release.setup, installed_exe()) else { return Err("this copy wasn't installed by setup".to_owned()) };
    let file = temp_dir().join(format!("OpenTeslaCam-Setup-{}.exe", release.version));
    download(setup, &file, |f| set(State::Downloading(f), ctx))?;
    set(State::Installing, ctx);
    swap(&exe, &file)
}

/// Fetches `setup` to `to`; anything but the exact file GitHub describes is deleted again.
fn download(setup: &Setup, to: &Path, mut progress: impl FnMut(f32)) -> Result<(), String> {
    let err = |e: &dyn std::fmt::Display| format!("download: {e}");
    std::fs::create_dir_all(to.parent().unwrap_or(to)).map_err(|e| err(&e))?;
    let mut out = std::fs::File::create(to).map_err(|e| err(&e))?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| err(&e))?;
    let fetched = rt.block_on(async {
        let mut resp = client().map_err(|e| err(&e))?.get(&setup.url).send().await.and_then(|r| r.error_for_status()).map_err(|e| err(&e))?;
        let (mut hash, mut done) = (Sha256::new(), 0u64);
        while let Some(chunk) = resp.chunk().await.map_err(|e| err(&e))? {
            done += chunk.len() as u64;
            // A server sending more than the release lists isn't sending the release.
            if done > setup.size {
                return Err(err(&"the file is larger than the release says"));
            }
            hash.update(&chunk);
            out.write_all(&chunk).map_err(|e| err(&e))?;
            progress(done as f32 / setup.size.max(1) as f32);
        }
        let hex: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if done == setup.size && hex == setup.sha256 { Ok(()) } else { Err(err(&"the file doesn't match the release")) }
    });
    drop(out);
    if fetched.is_err() {
        let _ = std::fs::remove_file(to);
    }
    fetched
}

/// Moves this exe aside, has `setup` install over it in the same folder, and starts the new one. If
/// setup fails the old exe gets its name back, so the app still starts next time.
fn swap(exe: &Path, setup: &Path) -> Result<(), String> {
    let (old, dir) = (old_name(exe), exe.parent().ok_or("the app has no folder")?);
    std::fs::rename(exe, &old).map_err(|e| format!("{}: {e}", exe.display()))?;
    let mut cmd = std::process::Command::new(setup);
    cmd.args(["--silent", "--dir"]).arg(dir);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000); // CREATE_NO_WINDOW
    let result = match cmd.status() {
        Ok(s) if s.success() && exe.exists() => std::process::Command::new(exe).spawn().map(drop).map_err(|e| format!("{}: {e}", exe.display())),
        Ok(s) => Err(format!("setup failed ({s})")),
        Err(e) => Err(format!("setup: {e}")),
    };
    if result.is_err() && !exe.exists() {
        let _ = std::fs::rename(&old, exe);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Network: the real API names a release with a setup, and that setup downloads and matches its
    /// hash. `cargo test update -- --ignored`
    #[test]
    #[ignore]
    fn github_serves_the_latest_setup() {
        let r = latest().expect("an answer from GitHub");
        assert!(!newer("0.0.1", &r.version) && !r.notes.is_empty(), "{r:?}");
        let setup = r.setup.expect("a setup on the release");
        let to = std::env::temp_dir().join(format!("otc-update-test-{}.exe", std::process::id()));
        let mut last = 0.0;
        download(&setup, &to, |f| last = f).unwrap();
        assert_eq!((std::fs::metadata(&to).unwrap().len(), last), (setup.size, 1.0));
        // One wrong digit and nothing is kept.
        let wrong = Setup { sha256: setup.sha256.replacen(|c: char| c != '0', "0", 1), ..setup };
        assert!(download(&wrong, &to, |_| {}).is_err() && !to.exists());
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

    #[test]
    fn a_release_is_read_and_its_setup_only_trusted_from_this_repo() {
        let hash = "ab".repeat(32);
        let asset = |url: &str, digest: &str| json!({"browser_download_url": url, "size": 7, "digest": digest});
        let release = |assets: serde_json::Value| parse(&json!({"tag_name": "v2.2.0", "body": "## What's new\n- x", "assets": assets})).unwrap();
        let good = format!("{DOWNLOADS}v2.2.0/OpenTeslaCam-Setup-2.2.0.exe");
        let r = release(json!([asset("https://example.com/setup.exe", &format!("sha256:{hash}")), asset(&good, &format!("sha256:{}", hash.to_uppercase()))]));
        assert_eq!((r.version.as_str(), r.notes.as_str()), ("2.2.0", "## What's new\n- x"));
        assert_eq!(r.setup, Some(Setup { url: good.clone(), size: 7, sha256: hash.clone() }));
        // Elsewhere, not an exe, no hash, a short hash: the notice still shows, without a setup.
        for (url, digest) in [("https://example.com/a.exe", format!("sha256:{hash}")), (&format!("{DOWNLOADS}v2.2.0/notes.txt"), format!("sha256:{hash}")), (&good, String::new()), (&good, "sha256:abcd".to_owned())] {
            assert_eq!(release(json!([asset(url, &digest)])).setup, None, "{url} {digest}");
        }
        assert_eq!(release(json!(null)).setup, None);
        assert_eq!(parse(&json!({"body": "no tag"})), None);
    }

    #[test]
    fn a_failed_setup_gives_the_app_its_name_back() {
        let dir = std::env::temp_dir().join(format!("otc-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("openteslacam.exe");
        std::fs::write(&exe, b"the app").unwrap();
        assert!(swap(&exe, &dir.join("no-such-setup.exe")).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"the app");
        assert!(!old_name(&exe).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
