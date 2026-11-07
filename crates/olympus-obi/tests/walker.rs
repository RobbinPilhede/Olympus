//! Drives the real walker and observer over throwaway Edge windows. `only_pids` keeps every other window untouched.
use olympus_obi::capture::{observe_as_foreground, ocr_bgra, visible_windows, window_pid, window_pixels, Limits, Observer, Reader, Settings};
use olympus_obi::store::Store;
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use windows::Win32::Foundation::{CloseHandle, FILETIME, HWND};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
use windows::Win32::System::Threading::{GetCurrentThread, GetProcessTimes, GetThreadTimes, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW};

const TITLE: &str = "ObserverProbePage";
const MARKER: &str = "observerMarkerAlpha7f3a";
const SECRET: &str = "hunter2SecretValue9c1";
const OCR_WORD: &str = "submarine";
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
const MEASURED_WALKS: u32 = 20;

// One Edge at a time, because several at once make the timing tests meaningless.
static LIVE: Mutex<()> = Mutex::new(());

fn one_at_a_time() -> MutexGuard<'static, ()> {
    LIVE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn page() -> String {
    let filler: String = (0..40).map(|line| format!("<p>Line {line} of the probe page talks about walking trees and reading names.</p>")).collect();
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>{TITLE}</title></head><body>
        <h1 style="font-size:64px">PINEAPPLE SUBMARINE</h1>
        <p>{MARKER}</p>
        <label>Login <input type="password" value="{SECRET}"></label>
        <label>Town <input type="text" value="visibleFieldValue42"></label>
        {filler}</body></html>"#
    )
}

fn edge_path() -> Option<PathBuf> {
    ["ProgramFiles(x86)", "ProgramFiles"].iter().filter_map(std::env::var_os).map(|root| PathBuf::from(root).join(r"Microsoft\Edge\Application\msedge.exe")).find(|path| path.exists())
}

fn processes() -> Vec<(u32, u32, String)> {
    let mut found = Vec::new();
    // SAFETY: the snapshot is checked, used only here and closed, and the entry is sized as the API requires.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).expect("snapshot");
        let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            let length = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            found.push((entry.th32ProcessID, entry.th32ParentProcessID, String::from_utf16_lossy(&entry.szExeFile[..length]).to_lowercase()));
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    found
}

fn process_tree(root: u32, known: &mut HashSet<u32>) {
    known.insert(root);
    loop {
        let all = processes();
        let before = known.len();
        for (pid, parent, exe) in &all {
            if known.contains(parent) && exe == "msedge.exe" {
                known.insert(*pid);
            }
        }
        if known.len() == before {
            return;
        }
    }
}

fn filetime_ms(time: FILETIME) -> f64 {
    ((u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)) as f64 / 10_000.0
}

fn thread_cpu_ms() -> f64 {
    let (mut created, mut exited, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: the pointers outlive the call, and the pseudo handle needs no closing.
    unsafe { GetThreadTimes(GetCurrentThread(), &mut created, &mut exited, &mut kernel, &mut user) }.expect("thread times");
    filetime_ms(kernel) + filetime_ms(user)
}

// The whole browser, renderer included, because that's where most of a walk's cost lands.
fn edge_cpu_ms(pids: &HashSet<u32>) -> f64 {
    let mut total = 0.0;
    for pid in pids {
        let (mut created, mut exited, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        // SAFETY: the handle is checked and closed here, and the pointers outlive the call.
        unsafe {
            if let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, *pid) {
                if GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user).is_ok() {
                    total += filetime_ms(kernel) + filetime_ms(user);
                }
                let _ = CloseHandle(process);
            }
        }
    }
    total
}

fn title_of(window: HWND) -> String {
    let mut buffer = [0u16; 512];
    // SAFETY: the buffer is valid for its length.
    let length = unsafe { GetWindowTextW(window, &mut buffer) };
    String::from_utf16_lossy(&buffer[..length.max(0) as usize])
}

struct Edge {
    child: Child,
    pids: HashSet<u32>,
    _folder: TempDir,
}

impl Edge {
    fn launch(path: &PathBuf, extra: &[&str]) -> Self {
        Self::launch_page(path, extra, &page())
    }

    fn launch_page(path: &PathBuf, extra: &[&str], html: &str) -> Self {
        let folder = tempfile::tempdir().expect("temp dir");
        let page_path = folder.path().join("probe.html");
        std::fs::write(&page_path, html).expect("write page");
        let profile = folder.path().join("profile");
        let child = Command::new(path)
            .arg(format!("--user-data-dir={}", profile.display()))
            .args(["--no-first-run", "--no-default-browser-check", "--disable-sync", "--disable-extensions", "--new-window", "--window-size=900,700"])
            .args(extra)
            .arg(format!("file:///{}", page_path.display().to_string().replace('\\', "/")))
            .spawn()
            .expect("start Edge");
        let pids = HashSet::from([child.id()]);
        Self { child, pids, _folder: folder }
    }

    fn window(&mut self) -> HWND {
        let started = Instant::now();
        while started.elapsed() < OPEN_TIMEOUT {
            process_tree(self.child.id(), &mut self.pids);
            let only: Vec<u32> = self.pids.iter().copied().collect();
            if let Some(window) = visible_windows(&only).into_iter().find(|&window| title_of(window).contains(TITLE)) {
                return window;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!("Edge's probe window never appeared");
    }

    fn only_pids(&self) -> Vec<u32> {
        self.pids.iter().copied().collect()
    }
}

impl Drop for Edge {
    fn drop(&mut self) {
        process_tree(self.child.id(), &mut self.pids);
        let _ = self.child.kill();
        let _ = self.child.wait();
        let alive: HashSet<u32> = processes().into_iter().filter(|process| process.2 == "msedge.exe").map(|process| process.0).collect();
        for pid in self.pids.intersection(&alive) {
            // SAFETY: only ids from this test's own process tree that are still msedge.exe, and the handle is closed.
            unsafe {
                if let Ok(process) = OpenProcess(PROCESS_TERMINATE, false, *pid) {
                    let _ = TerminateProcess(process, 0);
                    let _ = CloseHandle(process);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < limit {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    done()
}

fn test_settings(only_pids: Vec<u32>) -> Settings {
    Settings { only_pids, sweep_every: Duration::from_secs(1), poll: Duration::from_millis(500), idle_after: Duration::from_secs(30 * 24 * 3600), ocr: false, ..Settings::default() }
}

#[test]
fn real_walker_reads_the_page_and_never_the_password() {
    let _turn = one_at_a_time();
    let Some(path) = edge_path() else {
        eprintln!("Edge isn't installed, so the live walker test did nothing");
        return;
    };
    let mut edge = Edge::launch(&path, &[]);
    let window = edge.window();
    assert!(edge.pids.contains(&window_pid(window)));
    let reader = Reader::new().expect("reader");

    let woken = Instant::now();
    let walk = reader.read_waking(window, Limits::default()).expect("walk");
    println!("first walk with waking: {}ms until a document, {} nodes", woken.elapsed().as_millis(), walk.nodes);
    let text = walk.text();
    assert!(walk.has_document, "Chromium's page never appeared in the tree");
    assert!(text.contains(MARKER), "marker missing");
    assert!(text.contains("visibleFieldValue42"), "ordinary field value missing");
    assert!(!text.contains(SECRET), "password value leaked");
    assert!(!text.contains('•') && !text.contains('●'), "masked password leaked its length");
    let found_url = wait_until(Duration::from_secs(5), || reader.read(window, Limits::default()).is_ok_and(|walk| walk.url.is_some_and(|url| url.contains("probe.html"))));
    assert!(found_url, "the page's URL was never read");

    process_tree(edge.child.id(), &mut edge.pids);
    let (client_before, edge_before) = (thread_cpu_ms(), edge_cpu_ms(&edge.pids));
    let (mut total, mut slowest, mut nodes) = (Duration::ZERO, Duration::ZERO, 0);
    for _ in 0..MEASURED_WALKS {
        let walk = reader.read(window, Limits::default()).expect("walk");
        total += walk.elapsed;
        slowest = slowest.max(walk.elapsed);
        nodes = walk.nodes;
        assert!(!walk.text().contains(SECRET));
    }
    let (client, edge_cpu) = (thread_cpu_ms() - client_before, edge_cpu_ms(&edge.pids) - edge_before);
    println!(
        "{MEASURED_WALKS} walks over {nodes} nodes: average {:.1}ms, slowest {:.1}ms; CPU per walk: {:.1}ms in the walker, {:.1}ms across all of Edge",
        total.as_secs_f64() * 1000.0 / f64::from(MEASURED_WALKS),
        slowest.as_secs_f64() * 1000.0,
        client / f64::from(MEASURED_WALKS),
        edge_cpu / f64::from(MEASURED_WALKS)
    );

    let stepwise = reader.read(window, Limits { batched: false, ..Limits::default() }).expect("stepwise walk");
    println!("stepwise walker on the same page: {:.1}ms", stepwise.elapsed.as_secs_f64() * 1000.0);
    assert!(stepwise.text().contains(MARKER), "stepwise walker missed the marker");
    assert!(!stepwise.text().contains(SECRET), "stepwise walker leaked the password value");

    let captured = Instant::now();
    let (width, height, pixels) = window_pixels(window).expect("pixels");
    let capture_ms = captured.elapsed().as_millis();
    let recognized = Instant::now();
    let read = ocr_bgra(width, height, &pixels).expect("ocr");
    println!("{width}x{height}: captured in {capture_ms}ms, OCR took {}ms", recognized.elapsed().as_millis());
    assert!(read.to_lowercase().contains(OCR_WORD), "OCR missed the heading");
    assert!(!read.contains(SECRET));
}

#[test]
fn real_walker_reaches_the_end_of_a_long_page_within_the_budget() {
    let _turn = one_at_a_time();
    let Some(path) = edge_path() else {
        eprintln!("Edge isn't installed, so the live long-page test did nothing");
        return;
    };
    let filler: String = (0..3000).map(|line| format!("<p>Message number {line} in a long thread</p>")).collect();
    let mut edge = Edge::launch_page(&path, &[], &format!("<!doctype html><title>{TITLE}</title><body>{filler}<p>{MARKER}</p></body>"));
    let window = edge.window();
    let reader = Reader::new().expect("reader");
    let reached = wait_until(Duration::from_secs(10), || reader.read_waking(window, Limits::default()).is_ok_and(|walk| walk.text().contains(MARKER)));
    assert!(reached, "the page's last line was never read");
    process_tree(edge.child.id(), &mut edge.pids);
    let (client_before, edge_before) = (thread_cpu_ms(), edge_cpu_ms(&edge.pids));
    let (mut slowest, mut nodes) = (Duration::ZERO, 0);
    for _ in 0..5 {
        let walk = reader.read(window, Limits::default()).expect("walk");
        slowest = slowest.max(walk.elapsed);
        nodes = walk.nodes;
    }
    println!("long page, {nodes} nodes: slowest of 5 {:.1}ms; CPU per walk: {:.1}ms in the walker, {:.1}ms across all of Edge", slowest.as_secs_f64() * 1000.0, (thread_cpu_ms() - client_before) / 5.0, (edge_cpu_ms(&edge.pids) - edge_before) / 5.0);
    assert!(nodes > 3000);
}

#[test]
fn a_chromium_window_read_only_now_and_then_still_gives_up_its_page() {
    let _turn = one_at_a_time();
    let Some(path) = edge_path() else {
        eprintln!("Edge isn't installed, so the live sweep test did nothing");
        return;
    };
    let mut edge = Edge::launch(&path, &[]);
    edge.window();
    let data = tempfile::tempdir().expect("temp dir");
    let database = data.path().join("obi.db");
    let settings = Settings { sweep_every: Duration::from_secs(25), poll: Duration::from_secs(3600), ..test_settings(edge.only_pids()) };
    let observer = Observer::start(settings, database.clone()).expect("start");
    let stored = wait_until(Duration::from_secs(60), || Store::open(&database).and_then(|store| store.search(MARKER, 1)).is_ok_and(|hits| !hits.is_empty()));
    assert!(stored, "the page was never read; status {:?}", observer.status());
    assert!(Store::open(&database).and_then(|store| store.search(SECRET, 1)).expect("search").is_empty());
}

#[test]
fn a_file_saved_while_a_window_is_in_front_is_credited_to_it() {
    let _turn = one_at_a_time();
    let Some(path) = edge_path() else {
        eprintln!("Edge isn't installed, so the live file test did nothing");
        return;
    };
    let mut edge = Edge::launch(&path, &[]);
    edge.window();
    let data = tempfile::tempdir().expect("temp dir");
    let watched = tempfile::tempdir().expect("watched");
    let database = data.path().join("obi.db");
    let settings = Settings { watch: vec![watched.path().to_path_buf()], ..test_settings(edge.only_pids()) };
    let _observer = Observer::start(settings, database.clone()).expect("start");
    std::fs::write(watched.path().join("first.txt"), "x").expect("write");
    let is_front = || edge.pids.contains(&window_pid(unsafe { GetForegroundWindow() }));
    let was_front = wait_until(Duration::from_secs(5), is_front);
    std::thread::sleep(Duration::from_secs(2));
    let in_front_at_save = was_front && is_front();
    std::fs::write(watched.path().join("second.txt"), "x").expect("write");
    let events = || Store::open(&database).and_then(|store| store.file_events(0, i64::MAX)).unwrap_or_default();
    assert!(wait_until(Duration::from_secs(10), || events().iter().any(|event| event.path.ends_with("second.txt"))), "the save was noticed");
    if in_front_at_save && is_front() {
        let saved = events().into_iter().find(|event| event.path.ends_with("second.txt")).expect("event");
        assert_eq!(saved.app.as_deref(), Some("msedge"));
    } else {
        eprintln!("something else took focus during the test, so only the save itself was checked");
    }
}

#[test]
fn real_observer_stores_once_dedupes_and_keeps_the_password_out() {
    let _turn = one_at_a_time();
    let Some(path) = edge_path() else {
        eprintln!("Edge isn't installed, so the live observer test did nothing");
        return;
    };
    let mut edge = Edge::launch(&path, &[]);
    edge.window();
    let data = tempfile::tempdir().expect("temp dir");
    let database = data.path().join("obi.db");
    let mut observer = Observer::start(test_settings(edge.only_pids()), database.clone()).expect("start");

    let stored = wait_until(Duration::from_secs(30), || Store::open(&database).and_then(|store| store.search(MARKER, 1)).is_ok_and(|hits| !hits.is_empty()));
    assert!(stored, "marker never stored; status {:?}", observer.status());

    let (walks_then, stored_then) = { let status = observer.status(); (status.walks, status.stored) };
    assert!(wait_until(Duration::from_secs(30), || observer.status().walks >= walks_then + 6), "walking stopped; status {:?}", observer.status());
    let store = Store::open(&database).expect("reopen");
    assert_eq!(observer.status().stored, stored_then, "the unchanged page was stored again; status {:?}", observer.status());
    assert_eq!(store.search(MARKER, 10).expect("search").len(), 1, "marker lost after deduping");

    observer.pause();
    assert!(observer.status().paused);
    std::thread::sleep(Duration::from_secs(1));
    let walks_paused = observer.status().walks;
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(observer.status().walks, walks_paused, "still walking while paused");
    observer.resume();
    assert!(wait_until(Duration::from_secs(15), || observer.status().walks > walks_paused), "didn't resume");

    observer.stop();
    let status = observer.status();
    assert!(!status.running);
    assert_eq!(status.last_app.as_deref(), Some("msedge"));
    observer.stop();

    let hit = store.search(MARKER, 1).expect("search").remove(0);
    assert_eq!(hit.exe, "msedge");
    assert!(hit.title.contains(TITLE));
    assert!(store.search(SECRET, 10).expect("search").is_empty(), "password value is searchable");
    assert!(store.running_apps().expect("running").iter().all(|exe| exe == "msedge"), "a process outside the test was recorded");
    assert!(store.digest(0, i64::MAX, 4000).expect("digest").contains(MARKER));

    for suffix in ["obi.db", "obi.db-wal"] {
        if let Ok(bytes) = std::fs::read(data.path().join(suffix)) {
            let found = bytes.windows(SECRET.len()).any(|window| window == SECRET.as_bytes());
            assert!(!found, "password value is on disk in {suffix}");
        }
    }
}

#[test]
fn real_observer_falls_back_to_ocr_for_text_only_in_pixels() {
    let _turn = one_at_a_time();
    let Some(path) = edge_path() else {
        eprintln!("Edge isn't installed, so the live OCR test did nothing");
        return;
    };
    let html = format!(
        r#"<!doctype html><title>{TITLE}</title><body><canvas id="c" width="800" height="200"></canvas><script>
        const context = document.getElementById("c").getContext("2d");
        context.font = "64px sans-serif";
        context.fillText("PINEAPPLE SUBMARINE", 10, 100);
        </script></body>"#
    );
    let mut edge = Edge::launch_page(&path, &[], &html);
    let window = edge.window();
    std::thread::sleep(Duration::from_secs(1));
    let data = tempfile::tempdir().expect("temp dir");
    let database = data.path().join("obi.db");
    let settings = Settings { ocr: true, ..test_settings(edge.only_pids()) };
    let stored = wait_until(Duration::from_secs(15), || observe_as_foreground(settings.clone(), &database, window).expect("observe").stored > 0);
    assert!(stored, "nothing was stored");
    let store = Store::open(&database).expect("reopen");
    assert_eq!(store.search(OCR_WORD, 1).expect("search").len(), 1, "OCR text wasn't stored");
    let session = store.now().expect("now").foreground.expect("foreground session");
    assert_eq!(session.exe, "msedge");
    assert!(session.url.as_deref().is_some_and(|url| url.contains("probe.html")), "session has no URL");
}

#[test]
fn real_inprivate_window_is_never_read() {
    let _turn = one_at_a_time();
    let Some(path) = edge_path() else {
        eprintln!("Edge isn't installed, so the live InPrivate test did nothing");
        return;
    };
    let mut edge = Edge::launch(&path, &["--inprivate"]);
    let window = edge.window();
    assert!(title_of(window).to_lowercase().contains("inprivate"), "Edge's title no longer says InPrivate, so the title rule can't work");
    let data = tempfile::tempdir().expect("temp dir");
    let database = data.path().join("obi.db");
    let mut observer = Observer::start(test_settings(edge.only_pids()), database.clone()).expect("start");
    std::thread::sleep(Duration::from_secs(5));
    observer.stop();
    let status = observer.status();
    assert_eq!((status.walks, status.stored), (0, 0));
    let stats = Store::open(&database).expect("reopen").stats().expect("stats");
    assert_eq!((stats.sessions, stats.captures), (0, 0));
}

