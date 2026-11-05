use std::collections::VecDeque;
use std::ffi::c_void;
use std::fs::{self, File};
use std::io::Write;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::download::{hex, sha256_file};

static INFR_EXE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/infr.exe"));

const LOG_RING_LINES: usize = 400;
const LOG_LINE_MAX_CHARS: usize = 2000;
const DEVICES_TIMEOUT: Duration = Duration::from_secs(30);

/// The extracted engine binary and the job object tying its children to this process.
pub struct Engine {
    exe: PathBuf,
    log_path: PathBuf,
    // Never closed: the OS closes it when this process dies, which kills the child.
    job: usize,
    log: Arc<LogRing>,
}

/// A started engine process.
pub struct Spawned {
    pub child: Child,
    pub port: u16,
}

/// Arguments for serving one model.
pub struct ServeArgs<'a> {
    pub model: &'a str,
    pub device: &'a str,
    pub ctx: Option<&'a str>,
    pub parallel: Option<u32>,
}

struct LogRing {
    lines: Mutex<VecDeque<String>>,
    file: Mutex<Option<File>>,
}

impl LogRing {
    fn lines(&self) -> MutexGuard<'_, VecDeque<String>> {
        self.lines.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn file(&self) -> MutexGuard<'_, Option<File>> {
        self.file.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, line: &str) {
        let line: String = line.trim_end().chars().take(LOG_LINE_MAX_CHARS).collect();
        if line.is_empty() {
            return;
        }
        if let Some(file) = self.file().as_mut() {
            writeln!(file, "{line}").ok();
        }
        let mut lines = self.lines();
        if lines.len() == LOG_RING_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }
}

impl Engine {
    /// Extracts the embedded binary once and creates the kill-on-close job.
    pub fn new() -> Result<Self> {
        let base = local_app_data()?.join("Olympus");
        let exe = extract(&base.join("engine"), INFR_EXE)?;
        let logs = base.join("logs");
        fs::create_dir_all(&logs).with_context(|| format!("create {}", logs.display()))?;
        Ok(Self {
            exe,
            log_path: logs.join("infr.log"),
            job: create_job()?,
            log: Arc::new(LogRing { lines: Mutex::default(), file: Mutex::new(None) }),
        })
    }

    /// The file engine logs are written to.
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// The last `count` engine log lines.
    pub fn log_tail(&self, count: usize) -> String {
        let lines = self.log.lines();
        let skip = lines.len().saturating_sub(count);
        lines.iter().skip(skip).map(String::as_str).collect::<Vec<_>>().join("\n")
    }

    /// Picks `VulkanN` from `infr devices`, or `cpu` if it lists no usable GPU.
    pub async fn pick_device(&self) -> String {
        let mut cmd = self.command();
        cmd.arg("devices").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        match tokio::time::timeout(DEVICES_TIMEOUT, cmd.output()).await {
            Ok(Ok(out)) => parse_device(&String::from_utf8_lossy(&out.stdout)).unwrap_or_else(|| "cpu".to_string()),
            _ => "cpu".to_string(),
        }
    }

    /// Starts `infr serve` on a free loopback port, bound to this process's lifetime.
    pub async fn serve(&self, args: &ServeArgs<'_>) -> Result<Spawned> {
        let port = free_port().await?;
        let log_file = File::create(&self.log_path).ok();
        *self.log.file() = log_file;
        self.log.lines().clear();

        let mut cmd = self.command();
        cmd.arg("serve")
            .arg(args.model)
            .arg(format!("--addr=127.0.0.1:{port}"))
            .arg(format!("--dev={}", args.device));
        if let Some(ctx) = args.ctx {
            cmd.arg(format!("--ctx={ctx}"));
        }
        if let Some(parallel) = args.parallel {
            cmd.arg(format!("--parallel={parallel}"));
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = cmd.spawn().context("start the engine process")?;

        let handle = child.raw_handle().ok_or_else(|| anyhow!("the engine exited immediately"))?;
        unsafe { AssignProcessToJobObject(HANDLE(self.job as *mut c_void), HANDLE(handle)) }
            .context("tie the engine to this process")?;

        if let Some(out) = child.stdout.take() {
            tokio::spawn(pump_log(out, self.log.clone()));
        }
        if let Some(err) = child.stderr.take() {
            tokio::spawn(pump_log(err, self.log.clone()));
        }
        Ok(Spawned { child, port })
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.exe);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().to_ascii_uppercase().starts_with("INFR_") {
                cmd.env_remove(name);
            }
        }
        cmd.env("NO_COLOR", "1").creation_flags(CREATE_NO_WINDOW.0);
        if let Some(dir) = self.exe.parent() {
            cmd.current_dir(dir);
        }
        cmd
    }
}

async fn pump_log(stream: impl AsyncRead + Unpin, log: Arc<LogRing>) {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => log.push(&String::from_utf8_lossy(&line)),
        }
    }
}

async fn free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await.context("find a free loopback port")?;
    Ok(listener.local_addr()?.port())
}

fn local_app_data() -> Result<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).ok_or_else(|| anyhow!("LOCALAPPDATA is not set"))
}

fn create_job() -> Result<usize> {
    let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.context("create the job object")?;
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .context("configure the job object")?;
    Ok(job.0 as usize)
}

fn extract(engine_dir: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let hash = hex(&Sha256::digest(bytes));
    let dir = engine_dir.join(&hash[..16]);
    let exe = dir.join("infr.exe");
    if matches_hash(&exe, &hash) {
        return Ok(exe);
    }
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = dir.join(format!("infr.{}.tmp", std::process::id()));
    fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    let renamed = fs::rename(&tmp, &exe);
    if let Err(e) = renamed {
        fs::remove_file(&tmp).ok();
        // Another node may have won the race, or be running this exe and holding it open.
        if !matches_hash(&exe, &hash) {
            return Err(e).with_context(|| format!("install {}", exe.display()));
        }
    }
    Ok(exe)
}

fn matches_hash(path: &Path, hash: &str) -> bool {
    sha256_file(path).is_ok_and(|actual| actual == hash)
}

fn parse_device(listing: &str) -> Option<String> {
    let mut first = None;
    for line in listing.lines().map(str::trim) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(index) = name.strip_prefix("Vulkan") else {
            continue;
        };
        if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) || rest.contains("[cpu") {
            continue;
        }
        if rest.contains("<- default") {
            return Some(name.to_string());
        }
        first.get_or_insert_with(|| name.to_string());
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_listing_prefers_the_default_mark() {
        let listing = "2 Vulkan device(s) (select with `--dev VulkanN` / `INFR_DEV=VulkanN`):\n\n  \
            Vulkan0: AMD [integrated, 1.0 GiB device-local]\n           external-memory: none\n  \
            Vulkan1: NVIDIA [discrete, 31.4 GiB device-local]  <- default\n";
        assert_eq!(parse_device(listing).as_deref(), Some("Vulkan1"));
    }

    #[test]
    fn empty_or_software_only_listing_means_cpu() {
        assert_eq!(parse_device("no Vulkan physical devices found\n"), None);
        assert_eq!(parse_device("  Vulkan0: llvmpipe [cpu, 8.0 GiB device-local]  <- default\n"), None);
    }

    #[test]
    fn extract_writes_once_and_repairs_a_corrupt_copy() {
        let dir = std::env::temp_dir().join(format!("olympus-extract-test-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();

        let exe = extract(&dir, b"engine").unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"engine");

        fs::write(&exe, b"corrupt").unwrap();
        assert_eq!(extract(&dir, b"engine").unwrap(), exe);
        assert_eq!(fs::read(&exe).unwrap(), b"engine");
    }
}
