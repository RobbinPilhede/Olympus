use anyhow::{Context, Result};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::watch;

/// Cancels a running turn when set to true.
pub type Cancel = watch::Receiver<bool>;

/// Creates a linked cancel sender and receiver.
pub fn cancel_pair() -> (watch::Sender<bool>, Cancel) {
    watch::channel(false)
}

/// How a line-streaming child ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Idle,
    Cancelled,
}

/// Output of [`stream_lines`].
#[derive(Debug, Clone)]
pub struct Finished {
    pub exit: Exit,
    pub stderr_tail: String,
}

#[cfg(windows)]
mod job {
    use std::os::windows::io::RawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// Closing this kills the whole process tree.
    pub struct Job(HANDLE);
    // SAFETY: a job handle may be used and closed from any thread.
    unsafe impl Send for Job {}

    impl Job {
        pub fn assign(child: RawHandle) -> Option<Job> {
            // SAFETY: arguments are valid for the Win32 contract; failure is reported by return value.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let set = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of_val(&info) as u32,
                );
                if set == 0 || AssignProcessToJobObject(job, child as HANDLE) == 0 {
                    CloseHandle(job);
                    return None;
                }
                Some(Job(job))
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: self.0 is an open handle owned solely by this value.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

}

/// Blocks until this vendor may spawn again; concurrent CLI startups race on credential files.
pub async fn stagger(vendor: &str, gap: Duration) {
    static LAST: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    let wait = {
        let Ok(mut map) = LAST.get_or_init(Default::default).lock() else { return };
        let now = Instant::now();
        let slot = map.get(vendor).map_or(now, |last| (*last + gap).max(now));
        map.insert(vendor.to_string(), slot);
        slot - now
    };
    tokio::time::sleep(wait).await;
}

/// Removes inherited credentials and nested-session markers so children use their own login.
pub fn scrub_env(cmd: &mut Command) {
    for key in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "OPENAI_API_KEY", "CODEX_API_KEY", "CLAUDECODE", "CLAUDE_PID", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_SSE_PORT", "CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_SESSION_ID", "CLAUDE_CODE_BRIDGE_SESSION_ID", "CLAUDE_CODE_SESSION_ATTENDED", "CLAUDE_CODE_MESSAGING_SOCKET", "CLAUDE_CODE_MESSAGING_TOKEN"] {
        cmd.env_remove(key);
    }
}

/// Runs a command, feeding `stdin` and calling `on_line` per stdout line; `on_line` returns true once output is terminal.
pub async fn stream_lines(
    mut cmd: Command,
    stdin: String,
    idle: Duration,
    mut cancel: Cancel,
    mut on_line: impl FnMut(&str) -> bool,
) -> Result<Finished> {
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().context("starting agent process")?;
    #[cfg(windows)]
    let _job = child.raw_handle().and_then(job::Job::assign);

    let mut pipe = child.stdin.take().context("agent stdin unavailable")?;
    tokio::spawn(async move {
        // A child that exits early closes the pipe; its exit status reports the failure.
        let _ = pipe.write_all(stdin.as_bytes()).await;
        let _ = pipe.shutdown().await;
    });

    let tail = Arc::new(Mutex::new(String::new()));
    let mut stderr = child.stderr.take().context("agent stderr unavailable")?;
    let tail_writer = tail.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        while let Ok(n) = stderr.read(&mut buf).await {
            if n == 0 {
                break;
            }
            if let Ok(mut text) = tail_writer.lock() {
                text.push_str(&String::from_utf8_lossy(&buf[..n]));
                if text.len() > 16_384 {
                    let mut cut = text.len() - 8_192;
                    while !text.is_char_boundary(cut) {
                        cut += 1;
                    }
                    text.drain(..cut);
                }
            }
        }
    });

    let stdout = child.stdout.take().context("agent stdout unavailable")?;
    let mut lines = BufReader::with_capacity(1 << 16, stdout);
    let mut line = Vec::new();
    let mut terminal = false;
    let exit = loop {
        line.clear();
        let wait = if terminal { Duration::from_secs(3) } else { idle };
        tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    break Exit::Cancelled;
                }
            }
            read = tokio::time::timeout(wait, lines.read_until(b'\n', &mut line)) => match read {
                Err(_) if terminal => break Exit::Code(0),
                Err(_) => break Exit::Idle,
                Ok(Err(err)) => return Err(err).context("reading agent output"),
                Ok(Ok(0)) => {
                    let status = child.wait().await.context("waiting for agent")?;
                    break Exit::Code(status.code().unwrap_or(-1));
                }
                Ok(Ok(_)) => {
                    let text = String::from_utf8_lossy(&line);
                    let trimmed = text.trim();
                    if !trimmed.is_empty() && on_line(trimmed) {
                        terminal = true;
                    }
                }
            }
        }
    };
    if exit != Exit::Code(0) || terminal {
        let _ = child.start_kill();
    }
    let stderr_tail = tail.lock().map(|t| t.clone()).unwrap_or_default();
    Ok(Finished { exit, stderr_tail })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> Command {
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-Command", script]);
        cmd
    }

    #[tokio::test]
    async fn delivers_lines_and_exit_code() {
        let (_tx, cancel) = cancel_pair();
        let mut seen = Vec::new();
        let done = stream_lines(shell("Write-Output one; Write-Output two; exit 3"), String::new(), Duration::from_secs(30), cancel, |l| {
            seen.push(l.to_string());
            false
        })
        .await
        .expect("runs");
        assert_eq!(seen, ["one", "two"]);
        assert_eq!(done.exit, Exit::Code(3));
    }

    #[tokio::test]
    async fn idle_watchdog_stops_a_silent_child() {
        let (_tx, cancel) = cancel_pair();
        let started = Instant::now();
        let done = stream_lines(shell("Start-Sleep 60"), String::new(), Duration::from_secs(2), cancel, |_| false).await.expect("runs");
        assert_eq!(done.exit, Exit::Idle);
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[tokio::test]
    async fn terminal_output_ends_a_child_that_never_exits() {
        let (_tx, cancel) = cancel_pair();
        let started = Instant::now();
        let done = stream_lines(shell("Write-Output done; Start-Sleep 60"), String::new(), Duration::from_secs(50), cancel, |_| true).await.expect("runs");
        assert_eq!(done.exit, Exit::Code(0));
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[tokio::test]
    async fn cancel_stops_the_child() {
        let (tx, cancel) = cancel_pair();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let _ = tx.send(true);
        });
        let done = stream_lines(shell("Start-Sleep 60"), String::new(), Duration::from_secs(50), cancel, |_| false).await.expect("runs");
        assert_eq!(done.exit, Exit::Cancelled);
    }

    #[tokio::test]
    async fn job_kills_grandchildren_on_cancel() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("alive.txt");
        let script = format!(
            "$p = Start-Process powershell -PassThru -WindowStyle Hidden -ArgumentList '-NoProfile','-Command','Start-Sleep 4; Set-Content -LiteralPath ''{}'' x'; Write-Output started; Start-Sleep 60",
            marker.display()
        );
        let (tx, cancel) = cancel_pair();
        let sender = std::sync::Mutex::new(Some(tx));
        let done = stream_lines(shell(&script), String::new(), Duration::from_secs(50), cancel, |line| {
            if line == "started" {
                if let Ok(mut slot) = sender.lock() {
                    if let Some(tx) = slot.take() {
                        let _ = tx.send(true);
                    }
                }
            }
            false
        })
        .await
        .expect("runs");
        assert_eq!(done.exit, Exit::Cancelled);
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert!(!marker.exists(), "grandchild survived the job");
    }
}
