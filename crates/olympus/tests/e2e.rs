#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, HTCLOSE, WM_CHAR, WM_CLOSE, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP};

const EXE: &str = env!("CARGO_BIN_EXE_olympus");

struct Search {
    pid: u32,
    found: Option<HWND>,
}

unsafe extern "system" fn visit(window: HWND, data: LPARAM) -> BOOL {
    // SAFETY: `data` is the Search that `find_window` keeps alive for the whole enumeration.
    let search = unsafe { &mut *(data.0 as *mut Search) };
    let mut pid = 0;
    // SAFETY: `pid` outlives the call, and `window` came from EnumWindows.
    unsafe { GetWindowThreadProcessId(window, Some(&raw mut pid)) };
    // SAFETY: `window` came from EnumWindows.
    if pid == search.pid && unsafe { IsWindowVisible(window) }.as_bool() {
        search.found = Some(window);
        return false.into();
    }
    true.into()
}

fn find_window(pid: u32) -> HWND {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut search = Search { pid, found: None };
        // SAFETY: the callback only uses `search`, which outlives this call; ending early is reported as an error we can ignore.
        let _ = unsafe { EnumWindows(Some(visit), LPARAM(&raw mut search as isize)) };
        if let Some(window) = search.found {
            return window;
        }
        assert!(Instant::now() < deadline, "the window never appeared");
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct App {
    child: Child,
    window: HWND,
}

impl App {
    fn start(home: &Path, cwd: &Path) -> App {
        let child = Command::new(EXE).current_dir(cwd).env("USERPROFILE", home).env("HOME", home).spawn().expect("spawn");
        let window = find_window(child.id());
        std::thread::sleep(Duration::from_millis(800));
        App { child, window }
    }

    fn post(&self, message: u32, wparam: usize, lparam: isize) {
        // SAFETY: posting copies plain integers to a window handle that's valid while the child runs.
        unsafe { PostMessageW(Some(self.window), message, WPARAM(wparam), LPARAM(lparam)) }.expect("post");
    }

    fn line(&self, text: &str) {
        for unit in text.encode_utf16() {
            self.post(WM_CHAR, usize::from(unit), 1);
        }
        std::thread::sleep(Duration::from_millis(100));
        self.post(WM_KEYDOWN, 0x0D, 1);
        self.post(WM_KEYUP, 0x0D, 0xC000_0001u32 as isize);
        std::thread::sleep(Duration::from_millis(200));
    }

    fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().expect("wait") {
                return status;
            }
            assert!(Instant::now() < deadline, "the app didn't quit");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn saved_sessions(home: &Path) -> String {
    let dir: PathBuf = home.join(".olympus").join("sessions");
    let mut all = String::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        all.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
    }
    all
}

#[test]
fn typed_commands_reach_the_app_and_quit_saves_the_chat() {
    let (home, cwd) = (tempfile::tempdir().expect("home"), tempfile::tempdir().expect("cwd"));
    let mut app = App::start(home.path(), cwd.path());
    app.line("/help");
    app.line("/rename åäö-e2e-title");
    app.line("/quit");
    assert!(app.wait_for_exit().success());
    let saved = saved_sessions(home.path());
    assert!(saved.contains("åäö-e2e-title") && saved.contains("Commands"), "{saved}");
}

#[test]
fn closing_the_window_saves_and_exits_even_during_history_import() {
    let (home, cwd) = (tempfile::tempdir().expect("home"), tempfile::tempdir().expect("cwd"));
    let mut app = App::start(home.path(), cwd.path());
    app.line("/help");
    app.post(WM_CLOSE, 0, 0);
    assert!(app.wait_for_exit().success());
    assert!(saved_sessions(home.path()).contains("Commands"));
}

// A press the app swallows also stops Windows from dragging the window by its title bar.
#[test]
fn title_bar_presses_reach_windows_so_the_window_can_be_moved_and_closed() {
    let (home, cwd) = (tempfile::tempdir().expect("home"), tempfile::tempdir().expect("cwd"));
    let mut app = App::start(home.path(), cwd.path());
    let mut rect = windows::Win32::Foundation::RECT::default();
    // SAFETY: `rect` outlives the call, and the handle is valid while the child runs.
    unsafe { GetWindowRect(app.window, &raw mut rect) }.expect("rect");
    let position = (((rect.top + 18) as isize) << 16) | ((rect.right - 30) as isize & 0xFFFF);
    app.post(WM_NCLBUTTONDOWN, HTCLOSE as usize, position);
    std::thread::sleep(Duration::from_millis(100));
    app.post(WM_NCLBUTTONUP, HTCLOSE as usize, position);
    assert!(app.wait_for_exit().success());
}

#[test]
fn dragging_a_thread_up_the_list_reorders_it_and_is_remembered() {
    let (home, cwd) = (tempfile::tempdir().expect("home"), tempfile::tempdir().expect("cwd"));
    let mut app = App::start(home.path(), cwd.path());
    app.line("/help");
    app.line("/rename stays-put");
    app.line("/new");
    app.line("/help");
    app.line("/rename dragged-up");
    let at = |y: isize| (y << 16) | 100;
    app.post(WM_MOUSEMOVE, 0, at(166));
    std::thread::sleep(Duration::from_millis(300));
    app.post(WM_LBUTTONDOWN, 1, at(166));
    for y in [160, 140, 120, 100, 85] {
        std::thread::sleep(Duration::from_millis(100));
        app.post(WM_MOUSEMOVE, 1, at(y));
    }
    std::thread::sleep(Duration::from_millis(200));
    app.post(WM_LBUTTONUP, 0, at(85));
    std::thread::sleep(Duration::from_millis(300));
    app.line("/quit");
    assert!(app.wait_for_exit().success());
    let sessions = home.path().join(".olympus").join("sessions");
    let order = std::fs::read_to_string(sessions.join("order.txt")).expect("the order was saved");
    let top = order.lines().next().expect("a first thread");
    let chat = std::fs::read_to_string(sessions.join(format!("{top}.json"))).expect("its chat");
    assert!(chat.contains("dragged-up"), "{chat}");
}

#[test]
fn the_close_button_on_a_thread_closes_it_and_reopen_brings_it_back() {
    let (home, cwd) = (tempfile::tempdir().expect("home"), tempfile::tempdir().expect("cwd"));
    let mut app = App::start(home.path(), cwd.path());
    app.line("/help");
    app.line("/rename close-me");
    let position = (96isize << 16) | 226;
    app.post(WM_MOUSEMOVE, 0, position);
    std::thread::sleep(Duration::from_millis(300));
    app.post(WM_LBUTTONDOWN, 1, position);
    std::thread::sleep(Duration::from_millis(100));
    app.post(WM_LBUTTONUP, 0, position);
    std::thread::sleep(Duration::from_millis(500));
    let closed = home.path().join(".olympus").join("sessions").join("closed");
    let archived = std::fs::read_dir(&closed).expect("the chat was filed away").flatten().next().expect("one closed chat").path();
    assert!(std::fs::read_to_string(archived).expect("read").contains("close-me"));
    app.line("/reopen");
    app.line("/quit");
    assert!(app.wait_for_exit().success());
    assert!(saved_sessions(home.path()).contains("close-me"), "reopening restores it");
}

#[test]
fn fast_typing_never_loses_a_character_even_while_the_window_is_redrawing() {
    let (home, cwd) = (tempfile::tempdir().expect("home"), tempfile::tempdir().expect("cwd"));
    let mut app = App::start(home.path(), cwd.path());
    let mut expected = Vec::new();
    for round in 0..10 {
        let word = format!("round{round}-{}", "abcdefghij".repeat(30));
        app.line(&format!("/remember {word}"));
        app.line(&format!("/rename {word}"));
        app.line("/help");
        expected.push(word);
    }
    app.line("/quit");
    assert!(app.wait_for_exit().success());
    let saved = saved_sessions(home.path());
    assert!(saved.contains(expected.last().expect("word")), "the last rename arrived whole");
    let wrong = saved.matches("Unknown command").count();
    let evidence: Vec<&str> = saved.match_indices("Unknown command").map(|(at, _)| &saved[at..(at + 120).min(saved.len())]).collect();
    assert_eq!(wrong, 0, "a dropped character turned a command into an unknown one: {evidence:?}");
    let store = remembered_count(home.path(), &expected);
    assert_eq!(store, expected.len(), "every remembered word arrived whole");
}

fn remembered_count(home: &Path, words: &[String]) -> usize {
    let bytes = std::fs::read(home.join(".olympus").join("learn.db")).unwrap_or_default();
    let wal = std::fs::read(home.join(".olympus").join("learn.db-wal")).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes).to_string() + &String::from_utf8_lossy(&wal);
    words.iter().filter(|word| text.contains(word.as_str())).count()
}

#[test]
fn typing_the_moment_the_window_appears_loses_nothing() {
    let (home, cwd) = (tempfile::tempdir().expect("home"), tempfile::tempdir().expect("cwd"));
    let child = Command::new(EXE).current_dir(cwd.path()).env("USERPROFILE", home.path()).env("HOME", home.path()).spawn().expect("spawn");
    let window = find_window(child.id());
    let mut app = App { child, window };
    app.line("/help");
    app.line("/rename typed-at-launch");
    app.line("/quit");
    assert!(app.wait_for_exit().success());
    assert!(saved_sessions(home.path()).contains("typed-at-launch"), "the first keystrokes were lost");
}

#[test]
fn a_real_terminal_window_opens_and_keeps_the_app_responsive() {
    let (home, cwd) = (tempfile::tempdir().expect("home"), tempfile::tempdir().expect("cwd"));
    let mut app = App::start(home.path(), cwd.path());
    app.line("/term");
    std::thread::sleep(Duration::from_secs(2));
    app.line("exit");
    std::thread::sleep(Duration::from_secs(1));
    assert!(app.child.try_wait().expect("wait").is_none(), "a finished terminal doesn't end the app");
    app.post(WM_CLOSE, 0, 0);
    assert!(app.wait_for_exit().success());
}

#[test]
fn command_line_flags_print_and_bad_folders_explain_themselves() {
    let version = Command::new(EXE).arg("--version").output().expect("version");
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("olympus "));
    let bad = Command::new(EXE).arg("Z:/not/a/folder").output().expect("bad");
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("isn't a folder"));
}

#[test]
fn the_model_bridge_still_speaks_over_pipes_from_a_windowed_exe() {
    let home = tempfile::tempdir().expect("home");
    let mut child = Command::new(EXE).arg("mcp").env("USERPROFILE", home.path()).env("HOME", home.path()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"t","version":"1"}}}}}}"#).expect("write");
    stdin.flush().expect("flush");
    let mut reply = String::new();
    BufReader::new(child.stdout.take().expect("stdout")).read_line(&mut reply).expect("read");
    let _ = child.kill();
    let _ = child.wait();
    assert!(reply.contains("\"id\":1") && reply.contains("serverInfo"), "{reply}");
}

fn process_stat(pid: u32) -> (f64, f64) {
    let script = format!("$p = Get-Process -Id {pid}; Write-Output \"$($p.CPU) $($p.WorkingSet64)\"");
    let output = Command::new("powershell").args(["-NoProfile", "-Command", &script]).output().expect("powershell");
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.split_whitespace().map(|v| v.parse::<f64>().expect("number"));
    (parts.next().expect("cpu"), parts.next().expect("memory"))
}

#[test]
#[ignore]
fn an_idle_window_uses_almost_no_cpu() {
    let (home, cwd) = (tempfile::tempdir().expect("home"), tempfile::tempdir().expect("cwd"));
    let exe = std::env::var("OLYMPUS_TEST_EXE").unwrap_or_else(|_| EXE.to_string());
    let child = Command::new(exe).current_dir(cwd.path()).env("USERPROFILE", home.path()).env("HOME", home.path()).spawn().expect("spawn");
    let window = find_window(child.id());
    let mut app = App { child, window };
    std::thread::sleep(Duration::from_secs(5));
    let (before, _) = process_stat(app.child.id());
    std::thread::sleep(Duration::from_secs(20));
    let (after, memory) = process_stat(app.child.id());
    println!("cpu over 20s: {:.3}s, working set {:.1} MB", after - before, memory / 1_048_576.0);
    assert!(after - before < 0.6, "idle cpu {}", after - before);
    app.post(WM_CLOSE, 0, 0);
    app.wait_for_exit();
}
