use anyhow::{Context, Result};
use crate::input::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// A real terminal running in a pane.
pub struct PtyPane {
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub exited: Arc<AtomicBool>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    size: (u16, u16),
}

impl PtyPane {
    /// Spawns a command in a new pseudo-terminal; `wake` runs whenever output arrives.
    pub fn spawn(program: &str, args: &[String], cwd: &Path, env: Option<(&str, &Path)>, rows: u16, cols: u16, wake: impl Fn() + Send + Sync + 'static) -> Result<PtyPane> {
        let rows = rows.max(2);
        let cols = cols.max(2);
        let pair = native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).context("opening a terminal")?;
        let mut writer = pair.master.take_writer().context("opening terminal input")?;
        // ConPTY asks for the cursor position and blocks all output until it gets an answer.
        writer.write_all(b"\x1b[1;1R").context("answering the terminal's startup query")?;
        let mut command = CommandBuilder::new(program);
        command.args(args);
        command.cwd(cwd);
        for key in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "CODEX_API_KEY", "CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"] {
            command.env_remove(key);
        }
        if let Some((key, value)) = env {
            command.env(key, value);
        }
        let mut child = pair.slave.spawn_command(command).with_context(|| format!("starting {program}"))?;
        let killer = child.clone_killer();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().context("opening terminal output")?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 2000)));
        let exited = Arc::new(AtomicBool::new(false));
        let out_parser = parser.clone();
        let wake = Arc::new(wake);
        let (wait_exited, wait_wake) = (exited.clone(), wake.clone());
        // ConPTY keeps the output pipe open after the program ends, so only waiting on it reveals the exit.
        std::thread::spawn(move || {
            let _ = child.wait();
            wait_exited.store(true, Ordering::Relaxed);
            wait_wake();
        });
        std::thread::spawn(move || {
            let mut buffer = [0u8; 16 * 1024];
            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                if let Ok(mut parser) = out_parser.lock() {
                    parser.process(&buffer[..n]);
                }
                wake();
            }
        });
        Ok(PtyPane { parser, exited, master: pair.master, writer, killer, size: (rows, cols) })
    }

    /// Matches the pane to its on-screen size.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(2), cols.max(2));
        if self.size == (rows, cols) {
            return;
        }
        self.size = (rows, cols);
        // A failed resize leaves the old geometry, which still works.
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        if let Ok(mut parser) = self.parser.lock() {
            parser.screen_mut().set_size(rows, cols);
        }
    }

    /// Sends raw bytes to the program.
    pub fn send(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes).and_then(|()| self.writer.flush());
    }

    /// Sends text, using bracketed paste when the program asked for it.
    pub fn paste(&mut self, text: &str) {
        let bracketed = self.parser.lock().map(|p| p.screen().bracketed_paste()).unwrap_or(false);
        if bracketed {
            let payload = format!("\x1b[200~{text}\x1b[201~");
            self.send(payload.as_bytes());
        } else {
            self.send(text.as_bytes());
        }
    }

    /// Stops the program.
    pub fn kill(&mut self) {
        let _ = self.killer.kill();
    }
}

impl Drop for PtyPane {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Translates a key press to the bytes a terminal program expects.
pub fn key_bytes(key: KeyEvent) -> Vec<u8> {
    let KeyModifiers { ctrl, alt, shift } = key.modifiers;
    let mut out: Vec<u8> = Vec::new();
    if alt {
        out.push(0x1b);
    }
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let csi = |base: &str, tail: char| -> Vec<u8> {
        if modifier > 1 { format!("\x1b[{base};{modifier}{tail}").into_bytes() } else { format!("\x1b[{base}{tail}").into_bytes() }
    };
    let arrow = |letter: char| -> Vec<u8> {
        if modifier > 1 { format!("\x1b[1;{modifier}{letter}").into_bytes() } else { format!("\x1b[{letter}").into_bytes() }
    };
    match key.code {
        KeyCode::Char(c) if ctrl => {
            let upper = c.to_ascii_uppercase();
            if ('@'..='_').contains(&upper) {
                out.push(upper as u8 - b'@');
            } else if c == ' ' {
                out.push(0);
            }
        }
        KeyCode::Char(c) => out.extend(c.to_string().as_bytes()),
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend(b"\x1b[Z"),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Up => return arrow('A'),
        KeyCode::Down => return arrow('B'),
        KeyCode::Right => return arrow('C'),
        KeyCode::Left => return arrow('D'),
        KeyCode::Home => return arrow('H'),
        KeyCode::End => return arrow('F'),
        KeyCode::Delete => return csi("3", '~'),
        KeyCode::Insert => return csi("2", '~'),
        KeyCode::PageUp => return csi("5", '~'),
        KeyCode::PageDown => return csi("6", '~'),
        KeyCode::F(n @ 1..=4) => out.extend([0x1b, b'O', b'P' + n - 1]),
        KeyCode::F(n @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)];
            return csi(&code.to_string(), '~');
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
        key_bytes(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn translates_control_and_navigation_keys() {
        assert_eq!(key(KeyCode::Char('c'), KeyModifiers::CONTROL), [3]);
        assert_eq!(key(KeyCode::Char('x'), KeyModifiers::ALT), [0x1b, b'x']);
        assert_eq!(key(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(key(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert_eq!(key(KeyCode::Right, KeyModifiers::CONTROL), b"\x1b[1;5C");
        assert_eq!(key(KeyCode::PageDown, KeyModifiers::NONE), b"\x1b[6~");
        assert_eq!(key(KeyCode::F(5), KeyModifiers::NONE), b"\x1b[15~");
    }

    #[test]
    fn a_real_command_renders_output_and_reports_exit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pane = PtyPane::spawn("cmd", &["/c".to_string(), "echo olympus-pty-ok".to_string()], dir.path(), None, 24, 80, || {}).expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let contents = pane.parser.lock().expect("lock").screen().contents();
            if contents.contains("olympus-pty-ok") {
                break;
            }
            assert!(Instant::now() < deadline, "no output: {contents:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        while !pane.exited.load(Ordering::Relaxed) {
            assert!(Instant::now() < deadline, "the program's exit was never noticed");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
