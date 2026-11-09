//! What setup does, without any GUI: detection, the install steps and uninstall.

use std::fs;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

static UNINSTALLED: AtomicBool = AtomicBool::new(false);

pub const APP_EXE: &str = "openteslacam.exe";
pub const UNINSTALL_EXE: &str = "uninstall.exe";
pub const GIT_DOWNLOAD: &str = "https://git-scm.com/download/win";
const TESLA_REPO: &str = "https://github.com/teslamotors/dashcam";
const UNINSTALL_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\OpenTeslaCam";
const FONT_LICENSE: &[u8] = include_bytes!("../../assets/fonts/OFL.txt");
const DETECT_SCRIPT: &str = include_str!("../detect-deps.ps1");
/// Left next to Tesla's tools when setup cloned them, so uninstall never deletes a folder it didn't create.
const CLONED_MARKER: &str = ".cloned-by-openteslacam-setup";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub id: String,
    pub status: String,
    pub detail: String,
    pub path: String,
}

impl Check {
    pub fn ok(&self) -> bool {
        self.status == "ok"
    }
}

/// Parses the `-Machine` output of detect-deps.ps1, ignoring any stray line.
pub fn parse_checks(out: &str) -> Vec<Check> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.trim_end_matches('\r').split('\t');
            Some(Check { id: f.next()?.into(), status: f.next()?.into(), detail: f.next()?.into(), path: f.next()?.into() })
        })
        .collect()
}

pub fn find<'a>(checks: &'a [Check], id: &str) -> Option<&'a Check> {
    checks.iter().find(|c| c.id == id)
}

pub fn found_path(checks: &[Check], id: &str) -> Option<PathBuf> {
    find(checks, id).filter(|c| c.ok()).map(|c| PathBuf::from(&c.path))
}

/// Windows tools by full path: Windows would otherwise look in this exe's folder first, usually Downloads.
fn system_exe(relative: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    Path::new(&root).join("System32").join(relative)
}

fn powershell() -> Command {
    let mut c = Command::new(system_exe(r"WindowsPowerShell\v1.0\powershell.exe"));
    c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass"]).creation_flags(CREATE_NO_WINDOW);
    c
}

/// Runs the bundled detect-deps.ps1.
pub fn detect(dir: &Path) -> Result<Vec<Check>, String> {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let tmp = std::env::temp_dir().join(format!("openteslacam-setup-{}-{nanos}", std::process::id()));
    fs::create_dir(&tmp).map_err(|e| format!("temp folder: {e}"))?;
    let script = tmp.join("detect-deps.ps1");
    let run = fs::write(&script, DETECT_SCRIPT)
        .map_err(|e| format!("temp script: {e}"))
        .and_then(|()| powershell().arg("-File").arg(&script).arg("-Machine").arg("-InstallDir").arg(dir).output().map_err(|e| format!("powershell: {e}")));
    let _ = fs::remove_dir_all(&tmp);
    let out = run?;
    let checks = parse_checks(&String::from_utf8_lossy(&out.stdout));
    if checks.is_empty() {
        return Err(format!("powershell printed no results: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(checks)
}

/// An install this setup can update: its folder (with the app still in it) and its version.
pub struct Installed {
    pub dir: PathBuf,
    pub version: String,
}

/// What Windows' installed-apps entry says is installed, if the app is still where it says.
pub fn installed() -> Option<Installed> {
    let out = Command::new(system_exe("reg.exe")).args(["query", UNINSTALL_KEY]).creation_flags(CREATE_NO_WINDOW).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let found = parse_installed(&text)?;
    found.dir.join(APP_EXE).exists().then_some(found)
}

/// Reads `InstallLocation` and `DisplayVersion` from `reg query` output.
fn parse_installed(text: &str) -> Option<Installed> {
    let value = |name: &str| {
        text.lines().find_map(|l| {
            let mut f = l.trim().splitn(3, "    ");
            (f.next()? == name && f.next()?.starts_with("REG_")).then(|| f.next().unwrap_or("").trim().to_owned())
        })
    };
    let dir = value("InstallLocation").filter(|d| !d.is_empty())?;
    Some(Installed { dir: PathBuf::from(dir), version: value("DisplayVersion").unwrap_or_default() })
}

pub fn default_dir() -> String {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default();
    base.join("Programs").join("OpenTeslaCam").display().to_string()
}

/// The install folder as typed: absolute, not a drive root, no trailing separator.
pub fn validate_dir(typed: &str) -> Result<PathBuf, String> {
    let t = typed.trim().trim_end_matches(['\\', '/']);
    if t.is_empty() {
        return Err("choose an install folder".into());
    }
    let p = PathBuf::from(t);
    if !p.is_absolute() {
        return Err("the install folder needs a full path, like C:\\Apps\\OpenTeslaCam".into());
    }
    if p.parent().is_none() || t.len() <= 2 {
        return Err("choose a folder, not a whole drive".into());
    }
    Ok(p)
}

pub fn tesla_dir(dir: &Path) -> PathBuf {
    dir.join("vendor-dev").join("dashcam")
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Task {
    Git,
    App,
    Integrate,
    Tesla,
    Remove,
}

impl Task {
    pub fn label(self) -> &'static str {
        match self {
            Task::Git => "install git",
            Task::App => "copy openteslacam",
            Task::Integrate => "add shortcuts and uninstaller",
            Task::Tesla => "fetch tesla dashcam tools",
            Task::Remove => "remove openteslacam",
        }
    }
}

/// Dependencies go first, before anything is installed.
pub fn plan(tesla: bool, git_found: bool) -> Vec<Task> {
    let mut tasks = Vec::new();
    if tesla && !git_found {
        tasks.push(Task::Git);
    }
    tasks.extend([Task::App, Task::Integrate]);
    if tesla {
        tasks.push(Task::Tesla);
    }
    tasks
}

#[derive(Debug)]
pub enum Event {
    Start(Task),
    Done(Task),
    Failed(Task, String),
    Skipped(Task, String),
    Log(String),
    Finished,
}

pub type Out = Arc<dyn Fn(Event) + Send + Sync>;

pub struct Options {
    pub dir: PathBuf,
    pub desktop: bool,
    pub tesla: bool,
    pub git: Option<PathBuf>,
    pub winget: Option<PathBuf>,
    pub app: Arc<Vec<u8>>,
    pub stub: Arc<Vec<u8>>,
}

pub fn run_install(o: Options, out: Out) {
    let mut git = o.git.clone();
    if o.tesla && git.is_none() {
        out(Event::Start(Task::Git));
        match install_git(&o, &out) {
            Ok(path) => {
                git = Some(path);
                out(Event::Done(Task::Git));
            }
            Err(e) => out(Event::Failed(Task::Git, e)),
        }
    }

    out(Event::Start(Task::App));
    if let Err(e) = install_app(&o) {
        out(Event::Failed(Task::App, e));
        out(Event::Skipped(Task::Integrate, "openteslacam wasn't copied".into()));
        if o.tesla {
            out(Event::Skipped(Task::Tesla, "openteslacam wasn't copied".into()));
        }
        out(Event::Finished);
        return;
    }
    out(Event::Done(Task::App));

    out(Event::Start(Task::Integrate));
    match integrate(&o, &out) {
        Ok(()) => out(Event::Done(Task::Integrate)),
        Err(e) => out(Event::Failed(Task::Integrate, e)),
    }

    if o.tesla {
        match git {
            Some(git) => {
                out(Event::Start(Task::Tesla));
                match clone_tesla(&o.dir, &git, &out) {
                    Ok(()) => out(Event::Done(Task::Tesla)),
                    Err(e) => out(Event::Failed(Task::Tesla, e)),
                }
            }
            None => out(Event::Skipped(Task::Tesla, "git isn't available".into())),
        }
    }
    out(Event::Finished);
}

fn install_git(o: &Options, out: &Out) -> Result<PathBuf, String> {
    let winget = o.winget.as_ref().ok_or("winget isn't available; install git yourself, then run setup again")?;
    let mut c = Command::new(winget);
    c.args(GIT_ARGS);
    let result = stream(c, out);
    // Trust detection over winget's exit code: it also fails when git appeared some other way.
    let checks = detect(&o.dir)?;
    found_path(&checks, "git").ok_or_else(|| result.err().unwrap_or_else(|| "winget finished, but git still can't be found".into()))
}

const GIT_ARGS: [&str; 8] =
    ["install", "--id", "Git.Git", "--exact", "--silent", "--accept-package-agreements", "--accept-source-agreements", "--disable-interactivity"];

/// Replaces `path` in one step, so a failed or interrupted copy never leaves half an exe.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let write = fs::write(&part, bytes).and_then(|()| fs::rename(&part, path));
    write.map_err(|e| {
        let _ = fs::remove_file(&part);
        // 5 and 32: another program has the file open.
        if matches!(e.raw_os_error(), Some(5) | Some(32)) {
            format!("{} is in use or not writable. Close OpenTeslaCam and try again ({e})", path.display())
        } else {
            format!("{}: {e}", path.display())
        }
    })
}

fn install_app(o: &Options) -> Result<(), String> {
    fs::create_dir_all(&o.dir).map_err(|e| format!("{}: {e}", o.dir.display()))?;
    write_atomic(&o.dir.join(APP_EXE), &o.app)?;
    write_atomic(&o.dir.join(UNINSTALL_EXE), &o.stub)?;
    write_atomic(&o.dir.join("IBM-Plex-OFL.txt"), FONT_LICENSE)
}

fn add_value(name: &str, kind: &str, value: &str) -> Result<(), String> {
    let status = Command::new(system_exe("reg.exe"))
        .args(["add", UNINSTALL_KEY, "/v", name, "/t", kind, "/d", value, "/f"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("reg: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("couldn't write {name} to the registry")) }
}

/// Start menu and desktop shortcuts, plus the entry in Windows' installed-apps list.
fn integrate(o: &Options, out: &Out) -> Result<(), String> {
    let exe = o.dir.join(APP_EXE);
    let uninstall = o.dir.join(UNINSTALL_EXE);
    let dir = o.dir.display().to_string();
    let values = [
        ("DisplayName", "REG_SZ", "OpenTeslaCam".to_string()),
        ("DisplayVersion", "REG_SZ", env!("CARGO_PKG_VERSION").to_string()),
        ("DisplayIcon", "REG_SZ", exe.display().to_string()),
        ("InstallLocation", "REG_SZ", dir.clone()),
        ("UninstallString", "REG_SZ", format!("\"{}\" --uninstall", uninstall.display())),
        ("EstimatedSize", "REG_DWORD", (o.app.len() / 1024).to_string()),
        ("NoModify", "REG_DWORD", "1".to_string()),
        ("NoRepair", "REG_DWORD", "1".to_string()),
    ];
    for (name, kind, value) in &values {
        add_value(name, kind, value)?;
    }
    // Paths travel in environment variables so nothing in them is parsed as PowerShell.
    let script = "$w=New-Object -ComObject WScript.Shell;\
        $f=@([Environment]::GetFolderPath('Programs'));\
        if($env:OTC_DESKTOP -eq '1'){$f+=[Environment]::GetFolderPath('Desktop')};\
        foreach($d in $f){$s=$w.CreateShortcut((Join-Path $d 'OpenTeslaCam.lnk'));\
        $s.TargetPath=$env:OTC_EXE;$s.WorkingDirectory=$env:OTC_DIR;$s.Description='Tesla dashcam viewer';$s.Save()}";
    let mut c = powershell();
    c.arg("-Command").arg(script).env("OTC_EXE", &exe).env("OTC_DIR", &o.dir).env("OTC_DESKTOP", if o.desktop { "1" } else { "0" });
    stream(c, out)
}

fn clone_tesla(dir: &Path, git: &Path, out: &Out) -> Result<(), String> {
    let dest = tesla_dir(dir);
    if dest.join("sei_extractor.py").exists() {
        out(Event::Log(format!("already at {}", dest.display())));
        return Ok(());
    }
    let vendor = dir.join("vendor-dev");
    fs::create_dir_all(&vendor).map_err(|e| format!("{}: {e}", vendor.display()))?;
    let mut c = Command::new(git);
    c.args(["clone", "--depth", "1", TESLA_REPO]).arg(&dest).env("GIT_TERMINAL_PROMPT", "0");
    stream(c, out)?;
    fs::write(vendor.join(CLONED_MARKER), "").map_err(|e| format!("{}: {e}", vendor.display()))
}

/// Runs `c` with no console window, passing its output on line by line.
fn stream(mut c: Command, out: &Out) -> Result<(), String> {
    let name = c.get_program().to_string_lossy().into_owned();
    let mut child = c
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("{name}: {e}"))?;
    let pump = |pipe: Box<dyn Read + Send>, out: Out| thread::spawn(move || pump_lines(pipe, &out));
    let a = pump(Box::new(child.stdout.take().expect("piped")), out.clone());
    let b = pump(Box::new(child.stderr.take().expect("piped")), out.clone());
    let status = child.wait().map_err(|e| format!("{name}: {e}"))?;
    let _ = (a.join(), b.join());
    if status.success() {
        Ok(())
    } else {
        let file = Path::new(&name).file_name().map_or(name.clone(), |f| f.to_string_lossy().into_owned());
        Err(format!("{file} failed ({status}); see the log"))
    }
}

fn pump_lines(mut pipe: Box<dyn Read + Send>, out: &Out) {
    let mut buf = [0u8; 4096];
    let mut line = Vec::new();
    loop {
        let n = match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            // Progress bars rewrite their line with \r.
            if b == b'\n' || b == b'\r' {
                emit_line(&mut line, out);
            } else {
                line.push(b);
            }
        }
    }
    emit_line(&mut line, out);
}

fn emit_line(line: &mut Vec<u8>, out: &Out) {
    let text = String::from_utf8_lossy(line).trim().to_string();
    line.clear();
    // winget's spinner is one character per line.
    if text.chars().count() > 1 {
        out(Event::Log(text));
    }
}

/// Removes only what setup put there, whatever folder it was installed into.
pub fn run_uninstall(dir: &Path, out: Out) {
    out(Event::Start(Task::Remove));
    match uninstall(dir, &out) {
        Ok(()) => out(Event::Done(Task::Remove)),
        Err(e) => out(Event::Failed(Task::Remove, e)),
    }
    out(Event::Finished);
}

fn uninstall(dir: &Path, out: &Out) -> Result<(), String> {
    // A renamed copy elsewhere, say in target/release, must not delete the files sitting next to it.
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    if me.file_name().and_then(|n| n.to_str()) != Some(UNINSTALL_EXE) {
        return Err(format!("only {UNINSTALL_EXE} in the install folder can uninstall"));
    }
    remove_file(&dir.join(APP_EXE)).map_err(|e| format!("{e}. Close OpenTeslaCam and try again"))?;
    remove_file(&dir.join("IBM-Plex-OFL.txt"))?;

    let vendor = dir.join("vendor-dev");
    if vendor.join(CLONED_MARKER).exists() {
        let dest = tesla_dir(dir);
        out(Event::Log(format!("removing {}", dest.display())));
        if dest.exists() {
            fs::remove_dir_all(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
        }
        remove_file(&vendor.join(CLONED_MARKER))?;
        let _ = fs::remove_dir(&vendor);
    }

    // Only shortcuts that point here: another install may own the same name.
    let script = "$w=New-Object -ComObject WScript.Shell;\
        foreach($d in [Environment]::GetFolderPath('Programs'),[Environment]::GetFolderPath('Desktop')){\
        $l=Join-Path $d 'OpenTeslaCam.lnk';\
        if((Test-Path -LiteralPath $l) -and ($w.CreateShortcut($l).TargetPath -eq $env:OTC_EXE)){Remove-Item -LiteralPath $l}}";
    let mut c = powershell();
    c.arg("-Command").arg(script).env("OTC_EXE", dir.join(APP_EXE));
    stream(c, out)?;

    let key_there = Command::new(system_exe("reg.exe")).args(["query", UNINSTALL_KEY]).creation_flags(CREATE_NO_WINDOW).output().is_ok_and(|o| o.status.success());
    if key_there {
        let status = Command::new(system_exe("reg.exe"))
            .args(["delete", UNINSTALL_KEY, "/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("reg: {e}"))?;
        if !status.success() {
            return Err("couldn't remove the installed-apps entry".into());
        }
    }

    UNINSTALLED.store(true, Ordering::Relaxed);
    Ok(())
}

/// Call last, on the way out: a running exe can't delete itself, so a detached cmd does it just after
/// this process exits. rmdir only removes an empty folder.
pub fn delete_self_if_uninstalled() {
    if !UNINSTALLED.load(Ordering::Relaxed) {
        return;
    }
    let Ok(me) = std::env::current_exe() else { return };
    let _ = Command::new(system_exe("cmd.exe"))
        .raw_arg("/c ping -n 3 127.0.0.1 >nul & del /f \"%OTC_EXE%\" & rmdir \"%OTC_DIR%\"")
        .env("OTC_EXE", &me)
        .env("OTC_DIR", me.parent().unwrap_or(&me))
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn remove_file(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_detection_output() {
        let c = parse_checks("windows\tok\tWindows 11 Pro, build 26300\t\r\ngit\tmissing\tnot found\t\r\nnoise line\ntesla\tmissing\tnot installed\tC:\\x\\vendor-dev\\dashcam\n");
        assert_eq!(c.len(), 3);
        assert!(c[0].ok());
        assert_eq!(c[0].detail, "Windows 11 Pro, build 26300");
        assert!(!find(&c, "git").unwrap().ok());
        assert_eq!(find(&c, "tesla").unwrap().path, r"C:\x\vendor-dev\dashcam");
        assert_eq!(found_path(&c, "git"), None);
    }

    #[test]
    fn reads_the_installed_folder_and_version() {
        let out = "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\OpenTeslaCam\r\n    DisplayName    REG_SZ    OpenTeslaCam\r\n    DisplayVersion    REG_SZ    2.0.0\r\n    InstallLocation    REG_SZ    D:\\My Apps\\OpenTeslaCam\r\n    NoModify    REG_DWORD    0x1\r\n\r\n";
        let i = parse_installed(out).unwrap();
        assert_eq!((i.dir, i.version.as_str()), (PathBuf::from(r"D:\My Apps\OpenTeslaCam"), "2.0.0"));
        assert!(parse_installed("ERROR: The system was unable to find the specified registry key or value.").is_none());
        assert!(parse_installed("    InstallLocation    REG_SZ    \r\n").is_none(), "an empty location isn't an install");
    }

    #[test]
    fn git_comes_first_and_only_when_needed() {
        assert_eq!(plan(false, false), [Task::App, Task::Integrate]);
        assert_eq!(plan(true, true), [Task::App, Task::Integrate, Task::Tesla]);
        assert_eq!(plan(true, false), [Task::Git, Task::App, Task::Integrate, Task::Tesla]);
    }

    #[test]
    fn install_folder_must_be_a_real_absolute_folder() {
        assert_eq!(validate_dir(r" C:\Apps\OpenTeslaCam\ ").unwrap(), PathBuf::from(r"C:\Apps\OpenTeslaCam"));
        assert_eq!(validate_dir(r"\\nas\share\otc").unwrap(), PathBuf::from(r"\\nas\share\otc"));
        for bad in ["", "   ", r"relative\path", r"C:\", "C:", r"\no-drive"] {
            assert!(validate_dir(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn tesla_tools_go_under_the_install_folder() {
        assert_eq!(tesla_dir(Path::new(r"C:\Apps\OTC")), PathBuf::from(r"C:\Apps\OTC\vendor-dev\dashcam"));
    }

    #[test]
    fn atomic_write_leaves_no_part_file() {
        let dir = std::env::temp_dir().join(format!("otc-atomic-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.bin");
        write_atomic(&f, b"one").unwrap();
        write_atomic(&f, b"two").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"two");
        assert!(!dir.join("a.bin.part").exists());
        let missing = dir.join("nope").join("b.bin");
        assert!(write_atomic(&missing, b"x").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn output_lines_split_on_progress_rewrites_and_drop_the_spinner() {
        let got = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = got.clone();
        let out: Out = Arc::new(move |e| {
            if let Event::Log(l) = e {
                sink.lock().unwrap().push(l);
            }
        });
        pump_lines(Box::new(&b"Found Git\r\n-\r\\\r|\rReceiving 10%\rReceiving 100%\nlast"[..]), &out);
        assert_eq!(*got.lock().unwrap(), ["Found Git", "Receiving 10%", "Receiving 100%", "last"]);
    }

    /// Network, read-only: the real winget finds the package id without a console. `cargo test -p openteslacam-setup -- --ignored`
    #[test]
    #[ignore]
    fn winget_runs_windowless_and_knows_the_package() {
        let checks = detect(Path::new(r"C:\does-not-exist")).unwrap();
        let winget = found_path(&checks, "winget").expect("winget");
        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = lines.clone();
        let out: Out = Arc::new(move |e| {
            if let Event::Log(l) = e {
                sink.lock().unwrap().push(l);
            }
        });
        let mut c = Command::new(winget);
        c.args(["show", "--id", "Git.Git", "--exact", "--accept-source-agreements", "--disable-interactivity"]);
        stream(c, &out).unwrap();
        assert!(lines.lock().unwrap().iter().any(|l| l.contains("Git.Git")));
    }
}
