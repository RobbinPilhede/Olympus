//! Installs Olympus for this user and keeps the installed copy up to date.
use anyhow::{bail, Context, Result};
use olympus_core::config::user_home;
use olympus_core::openai::client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::timeout;

const RELEASES: &str = "https://github.com/RobbinPilhede/Olympus/releases";
const EXE_NAME: &str = "olympus.exe";
const UNINSTALL_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Olympus";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Tells the next launch to skip the splash and show this message once.
pub const WELCOME_VAR: &str = "OLYMPUS_WELCOME";

/// What the splash window does while it animates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    Install,
    Update,
}

/// Where the splash has got to.
#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    Working(String),
    Downloading { version: String, fraction: f32 },
    OpenApp,
    Relaunch { exe: PathBuf, welcome: String },
    Failed(String),
}

/// Shared between the background job and the splash.
pub type Progress = Arc<Mutex<Stage>>;

#[derive(Debug, Deserialize)]
struct Manifest {
    version: String,
    sha256: String,
}

/// `%LOCALAPPDATA%\Programs\Olympus`, which needs no admin rights.
pub fn install_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map_or_else(|| user_home().join("AppData").join("Local"), PathBuf::from).join("Programs").join("Olympus")
}

// Dev builds and tests run elsewhere, so an update can never overwrite a build you just made.
/// Decides from where this exe sits whether it installs, updates or just opens.
pub fn job_for(exe: &Path, install_dir: &Path) -> Option<Job> {
    let name = exe.file_stem()?.to_string_lossy().to_lowercase();
    if name.contains("setup") {
        return Some(Job::Install);
    }
    let same = |a: &Path, b: &Path| a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase();
    same(exe.parent()?, install_dir).then_some(Job::Update)
}

fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.trim().trim_start_matches('v').split('.').map(|part| part.parse().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse_version(latest), parse_version(current)), (Some(latest), Some(current)) if latest > current)
}

// Renaming a running exe is allowed on Windows, so an open Olympus keeps working on the old copy.
fn swap_in(new: &Path, target: &Path) -> std::io::Result<()> {
    let old = target.with_extension(format!("{}.old", std::process::id()));
    let replacing = target.exists();
    if replacing {
        std::fs::rename(target, &old)?;
    }
    std::fs::rename(new, target).inspect_err(|_| {
        if replacing {
            let _ = std::fs::rename(&old, target);
        }
    })
}

// A copy that's still running stays locked, and goes on a later launch.
fn remove_old_copies(dir: &Path) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.path().extension().is_some_and(|extension| extension == "old") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    // SAFETY: the returned string is copied before Windows's allocation is freed once.
    unsafe {
        let path = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let text = path.to_string();
        CoTaskMemFree(Some(path.0.cast()));
        text.ok().map(PathBuf::from)
    }
}

// Test windows use a temporary profile, and must never touch the real registry or Desktop.
pub fn in_real_profile() -> bool {
    known_folder(&windows::Win32::UI::Shell::FOLDERID_Profile).is_some_and(|real| real == user_home())
}

fn shortcut_paths() -> Vec<PathBuf> {
    let appdata = std::env::var_os("APPDATA").map_or_else(|| user_home().join("AppData").join("Roaming"), PathBuf::from);
    let mut paths = vec![appdata.join(r"Microsoft\Windows\Start Menu\Programs\Olympus.lnk")];
    if in_real_profile() {
        paths.extend(known_folder(&windows::Win32::UI::Shell::FOLDERID_Desktop).map(|desktop| desktop.join("Olympus.lnk")));
    }
    paths
}

fn run_hidden(program: &str, args: &[OsString]) -> Result<()> {
    let status = Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(CREATE_NO_WINDOW).status().with_context(|| format!("running {program}"))?;
    if !status.success() {
        bail!("{program} failed with {status}");
    }
    Ok(())
}

fn powershell_quoted(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "''"))
}

fn create_shortcuts(exe: &Path) -> Result<()> {
    let (target, home) = (powershell_quoted(exe), powershell_quoted(&user_home()));
    for link in shortcut_paths() {
        if let Some(folder) = link.parent() {
            std::fs::create_dir_all(folder)?;
        }
        let script = format!("$s=(New-Object -ComObject WScript.Shell).CreateShortcut({});$s.TargetPath={target};$s.WorkingDirectory={home};$s.IconLocation={target};$s.Save()", powershell_quoted(&link));
        run_hidden("powershell.exe", &["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), script.into()])?;
    }
    Ok(())
}

// Shows Olympus in Windows's installed apps, where its version and Uninstall button live.
fn register(exe: &Path, version: &str) -> Result<()> {
    if !in_real_profile() {
        return Ok(());
    }
    let exe_text = exe.display().to_string();
    let folder = exe.parent().map(|dir| dir.display().to_string()).unwrap_or_default();
    let values = [("DisplayName", "Olympus".to_string()), ("DisplayVersion", version.to_string()), ("Publisher", "Robbin Pilhede".to_string()), ("DisplayIcon", exe_text.clone()), ("InstallLocation", folder), ("UninstallString", format!("\"{exe_text}\" --uninstall"))];
    for (name, value) in values {
        run_hidden("reg.exe", &["add".into(), UNINSTALL_KEY.into(), "/v".into(), name.into(), "/t".into(), "REG_SZ".into(), "/d".into(), value.into(), "/f".into()])?;
    }
    for name in ["NoModify", "NoRepair"] {
        run_hidden("reg.exe", &["add".into(), UNINSTALL_KEY.into(), "/v".into(), name.into(), "/t".into(), "REG_DWORD".into(), "/d".into(), "1".into(), "/f".into()])?;
    }
    Ok(())
}

fn install(dir: &Path) -> Result<PathBuf> {
    let source = std::env::current_exe().context("finding this installer")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    remove_old_copies(dir);
    let (new, target) = (dir.join("olympus.exe.new"), dir.join(EXE_NAME));
    std::fs::copy(&source, &new).with_context(|| format!("copying to {}", new.display()))?;
    swap_in(&new, &target).with_context(|| format!("replacing {}", target.display()))?;
    create_shortcuts(&target)?;
    register(&target, env!("CARGO_PKG_VERSION"))?;
    Ok(target)
}

/// Removes the app, its shortcuts and its Windows entry, and leaves your data in `~/.olympus`.
pub fn uninstall() -> Result<()> {
    let dir = install_dir();
    let running = std::env::current_exe()?;
    if job_for(&running, &dir) != Some(Job::Update) {
        bail!("Only the installed copy in {} can uninstall itself.", dir.display());
    }
    for link in shortcut_paths() {
        let _ = std::fs::remove_file(link);
    }
    if in_real_profile() {
        let _ = run_hidden("reg.exe", &["delete".into(), UNINSTALL_KEY.into(), "/f".into()]);
    }
    // This exe is locked until it exits, so a detached shell deletes the folder a moment later.
    Command::new("cmd.exe").current_dir(std::env::temp_dir()).raw_arg(format!("/c ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", dir.display())).creation_flags(CREATE_NO_WINDOW).spawn().context("removing the app folder")?;
    Ok(())
}

async fn latest_manifest() -> Result<Option<Manifest>> {
    let response = client().get(format!("{RELEASES}/latest/download/latest.json")).send().await?;
    if response.status() == 404 {
        return Ok(None);
    }
    Ok(Some(response.error_for_status()?.json().await?))
}

// Returns None if you skipped it.
async fn download(manifest: &Manifest, to: &Path, progress: &Progress, cancel: &AtomicBool) -> Result<Option<()>> {
    let url = format!("{RELEASES}/download/v{}/{EXE_NAME}", manifest.version);
    let mut response = client().get(url).send().await?.error_for_status()?;
    let total = response.content_length().unwrap_or(0) as f32;
    let mut file = std::fs::File::create(to)?;
    let mut hash = Sha256::new();
    let mut received = 0.0;
    while let Some(chunk) = timeout(Duration::from_secs(30), response.chunk()).await.context("the download stalled")?? {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        file.write_all(&chunk)?;
        hash.update(&chunk);
        received += chunk.len() as f32;
        if total > 0.0 {
            set(progress, Stage::Downloading { version: manifest.version.clone(), fraction: received / total });
        }
    }
    file.flush()?;
    let actual: String = hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    if !actual.eq_ignore_ascii_case(manifest.sha256.trim()) {
        bail!("the download's checksum doesn't match");
    }
    Ok(Some(()))
}

fn reports_version(exe: &Path, version: &str) -> bool {
    Command::new(exe).arg("--version").creation_flags(CREATE_NO_WINDOW).output().is_ok_and(|output| output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == format!("olympus {version}"))
}

async fn update(dir: &Path, progress: &Progress, cancel: &AtomicBool) -> Result<Stage> {
    remove_old_copies(dir);
    let Some(manifest) = timeout(Duration::from_secs(3), latest_manifest()).await.context("the update check timed out")?? else { return Ok(Stage::OpenApp) };
    if !is_newer(&manifest.version, env!("CARGO_PKG_VERSION")) {
        return Ok(Stage::OpenApp);
    }
    set(progress, Stage::Downloading { version: manifest.version.clone(), fraction: 0.0 });
    let (new, target) = (dir.join("olympus.new.exe"), dir.join(EXE_NAME));
    let result = install_download(&manifest, &new, &target, progress, cancel).await;
    let _ = std::fs::remove_file(&new);
    result
}

async fn install_download(manifest: &Manifest, new: &Path, target: &Path, progress: &Progress, cancel: &AtomicBool) -> Result<Stage> {
    if download(manifest, new, progress, cancel).await?.is_none() {
        return Ok(Stage::OpenApp);
    }
    set(progress, Stage::Working(format!("Installing {}", manifest.version)));
    if !reports_version(new, &manifest.version) {
        bail!("the downloaded app didn't start");
    }
    swap_in(new, target)?;
    // The update already worked, so a missed version label in Windows's list shouldn't undo it.
    let _ = register(target, &manifest.version);
    Ok(Stage::Relaunch { exe: target.to_path_buf(), welcome: format!("Updated to Olympus {}", manifest.version) })
}

fn set(progress: &Progress, stage: Stage) {
    if let Ok(mut current) = progress.lock() {
        *current = stage;
    }
}

/// Runs a job in the background, reporting to `progress`. A failed update just opens the app.
pub fn start(job: Job, progress: Progress, cancel: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let dir = install_dir();
        let finished = match job {
            Job::Install => match tokio::task::spawn_blocking(move || install(&dir)).await {
                Ok(Ok(exe)) => Stage::Relaunch { exe, welcome: format!("Olympus {} is installed", env!("CARGO_PKG_VERSION")) },
                Ok(Err(error)) => Stage::Failed(format!("Unable to install: {error:#}")),
                Err(error) => Stage::Failed(format!("Unable to install: {error}")),
            },
            Job::Update => update(&dir, &progress, &cancel).await.unwrap_or(Stage::OpenApp),
        };
        set(&progress, finished);
    });
}

/// Opens the finished copy with the same folder and arguments, then ends this one.
pub fn relaunch(exe: &Path, welcome: &str) -> ! {
    let launched = Command::new(exe).args(std::env::args_os().skip(1)).env(WELCOME_VAR, welcome).spawn();
    std::process::exit(i32::from(launched.is_err()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_setup_name_and_the_install_folder_trigger_a_job() {
        let dir = Path::new(r"C:\Users\me\AppData\Local\Programs\Olympus");
        assert_eq!(job_for(Path::new(r"C:\Downloads\OlympusSetup.exe"), dir), Some(Job::Install));
        assert_eq!(job_for(&dir.join("olympus.exe"), dir), Some(Job::Update));
        assert_eq!(job_for(Path::new(r"c:\users\ME\appdata\local\programs\olympus\olympus.exe"), dir), Some(Job::Update));
        assert_eq!(job_for(Path::new(r"C:\code\Olympus\target\release\olympus.exe"), dir), None);
        assert_eq!(job_for(Path::new(r"C:\Downloads\olympus.exe"), dir), None);
    }

    #[test]
    fn versions_compare_as_numbers_and_bad_ones_never_update() {
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        for bad in ["", "1.2", "1.2.3.4", "latest", "1.2.x", "../../evil"] {
            assert!(!is_newer(bad, "0.1.0"), "{bad}");
        }
    }

    #[test]
    fn swapping_replaces_the_exe_and_keeps_the_old_one_aside() {
        let dir = tempfile::tempdir().expect("dir");
        let (new, target) = (dir.path().join("new"), dir.path().join("olympus.exe"));
        std::fs::write(&new, "new").expect("new");
        std::fs::write(&target, "old").expect("old");
        swap_in(&new, &target).expect("swap");
        assert_eq!(std::fs::read_to_string(&target).expect("target"), "new");
        remove_old_copies(dir.path());
        assert_eq!(std::fs::read_dir(dir.path()).expect("dir").count(), 1);
    }

    #[test]
    fn a_failed_swap_puts_the_old_exe_back() {
        let dir = tempfile::tempdir().expect("dir");
        let target = dir.path().join("olympus.exe");
        std::fs::write(&target, "old").expect("old");
        assert!(swap_in(&dir.path().join("missing"), &target).is_err());
        assert_eq!(std::fs::read_to_string(&target).expect("target"), "old");
    }

    #[test]
    fn the_manifest_must_name_a_version_and_checksum() {
        let manifest: Manifest = serde_json::from_str(r#"{"version":"0.2.0","sha256":"ab"}"#).expect("manifest");
        assert_eq!((manifest.version.as_str(), manifest.sha256.as_str()), ("0.2.0", "ab"));
        assert!(serde_json::from_str::<Manifest>(r#"{"version":"0.2.0"}"#).is_err());
    }
}
