//! Serves this Olympus to browsers on your network, once its switch is on.
use crate::app::{App, AppEvent};
use crate::commands;
use crate::gui::chrome::{self, short_path};
use crate::gui::parts::{block_parts, Part, PartKind};
use crate::markdown::{Rich, Tone};
use crate::session::{Activity, Block, Pane, ToolStatus};
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path as UrlPath, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::OsRng;
use olympus_core::config::home_dir;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

/// The port phones connect to.
pub const DEFAULT_PORT: u16 = 47480;
/// This file's existence turns the mirror on at launch.
pub const MIRROR_ON: &str = "mirror-on";
const DEVICES_FILE: &str = "mirror.json";
const PORT_VAR: &str = "OLYMPUS_MIRROR_PORT";
const MAX_UPLOAD: usize = 25 * 1024 * 1024;
const FIRST_LOAD: usize = 300;
const CODE_LIFETIME: Duration = Duration::from_secs(600);
/// A code dies after this many wrong tries, so nobody on the network can keep guessing.
const MAX_WRONG_CODES: u32 = 5;
const UPLOAD_TYPES: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "heic", "pdf", "txt", "md", "csv", "json", "log", "zip"];
const FIREWALL_RULE: &str = "Olympus Phone Access";

/// A paired browser; only a hash of its key is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub name: String,
    pub hash: String,
}

fn hashed(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn random_code() -> String {
    const LETTERS: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut bytes = [0u8; 8];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| char::from(LETTERS[usize::from(*byte) % LETTERS.len()])).collect()
}

/// Who may come in: paired browsers, and whoever holds the one-time code.
pub struct Door {
    path: PathBuf,
    pub devices: Vec<Device>,
    code: Option<(String, Instant)>,
    wrong: u32,
}

impl Door {
    /// Loads paired browsers from a file; a missing or broken file means none.
    pub fn load(path: PathBuf) -> Self {
        let devices = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
        Self { path, devices, code: None, wrong: 0 }
    }

    /// The code to show right now, replacing one that expired or was guessed at too often.
    pub fn code(&mut self) -> String {
        let alive = self.code.as_ref().filter(|(_, born)| born.elapsed() < CODE_LIFETIME).map(|(code, _)| code.clone());
        alive.unwrap_or_else(|| {
            let code = random_code();
            self.code = Some((code.clone(), Instant::now()));
            self.wrong = 0;
            code
        })
    }

    /// Trades the code for a browser's permanent key.
    pub fn pair(&mut self, code: &str, name: &str) -> Option<String> {
        let (expected, born) = self.code.as_ref()?;
        if born.elapsed() >= CODE_LIFETIME {
            self.code = None;
            return None;
        }
        if !code.trim().eq_ignore_ascii_case(expected) {
            self.wrong += 1;
            if self.wrong >= MAX_WRONG_CODES {
                self.code = None;
            }
            return None;
        }
        self.code = None;
        let token = random_token();
        self.devices.push(Device { name: name.chars().take(60).collect(), hash: hashed(&token) });
        self.save();
        Some(token)
    }

    /// True for a key from a paired browser.
    pub fn allows(&self, token: &str) -> bool {
        let hash = hashed(token);
        self.devices.iter().any(|device| device.hash == hash)
    }

    /// Forgets every paired browser.
    pub fn forget_all(&mut self) {
        self.devices.clear();
        self.code = None;
        self.save();
    }

    fn save(&self) {
        if let Ok(text) = serde_json::to_string_pretty(&self.devices) {
            // Failing only means pairing again after a restart.
            let _ = std::fs::write(&self.path, text);
        }
    }
}

/// True for addresses that can only be on your own network, or on a private overlay like Tailscale.
pub fn is_local_peer(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local() || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1])),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_local_peer(IpAddr::V4(v4)),
            None => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80,
        },
    }
}

/// What a browser can ask the app to do.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Act {
    Send { thread: u64, text: String, #[serde(default)] files: Vec<String> },
    Stop { thread: u64 },
    Close { thread: u64 },
    Seen { thread: u64 },
    Term { thread: u64, text: String },
    Key { thread: u64, key: String },
    Scroll { thread: u64, lines: isize },
    Pick { thread: u64, item: usize },
    PickClose,
    Palette { thread: u64 },
}

/// A question for the interface thread, which owns all the state.
pub enum Ask {
    Meta,
    Thread { id: u64, since: Option<u64>, before: Option<usize> },
    Term { id: u64 },
    DiffSource { id: u64 },
    Act(Act),
    Obi(crate::obiview::Request),
}

fn tone_name(tone: Tone) -> &'static str {
    match tone {
        Tone::Text => "text",
        Tone::Dim => "dim",
        Tone::Accent => "accent",
        Tone::Red => "red",
        Tone::Bright => "bright",
    }
}

/// Styled text for a browser: stretches cut wherever the style or a link changes.
pub fn rich_json(rich: &Rich) -> Vec<Value> {
    let mut out = Vec::new();
    let mut at = 0;
    for (len, style) in &rich.spans {
        let end = at + len;
        let mut from = at;
        while from < end {
            let link = rich.links.iter().find(|(range, _)| range.start <= from && from < range.end);
            let next = rich.links.iter().map(|(range, _)| range.start).filter(|start| *start > from).min().unwrap_or(end);
            let to = link.map_or(next, |(range, _)| range.end).min(end);
            let mut span = json!({"t": rich.text.get(from..to).unwrap_or(""), "tone": tone_name(style.tone)});
            for (key, on) in [("b", style.bold), ("i", style.italic), ("s", style.strike), ("c", style.code)] {
                if on {
                    span[key] = json!(true);
                }
            }
            if let Some((_, target)) = link {
                span["href"] = json!(target);
            }
            out.push(span);
            from = to;
        }
        at = end;
    }
    out
}

fn status_word(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Running => "running",
        ToolStatus::Ok => "ok",
        ToolStatus::Failed => "failed",
    }
}

/// A drawn piece of text for a browser.
pub fn part_json(part: &Part) -> Value {
    let spans = rich_json(&part.rich);
    match &part.kind {
        PartKind::User => json!({"k": "user", "spans": spans}),
        PartKind::Label => json!({"k": "label", "spans": spans}),
        PartKind::Para => json!({"k": "para", "spans": spans}),
        PartKind::Phase => json!({"k": "phase", "spans": spans}),
        PartKind::Heading(level) => json!({"k": "heading", "level": level, "spans": spans}),
        PartKind::Item { depth, marker } => json!({"k": "item", "depth": depth, "marker": marker, "spans": spans}),
        PartKind::Quote => json!({"k": "quote", "spans": spans}),
        PartKind::Code { lang } => json!({"k": "code", "lang": lang, "spans": spans}),
        PartKind::Table { rows, columns } => json!({"k": "table", "columns": columns, "rows": rows.iter().map(|row| row.iter().map(rich_json).collect::<Vec<_>>()).collect::<Vec<_>>()}),
        PartKind::Rule => json!({"k": "rule"}),
        PartKind::Tool { status, detail } => json!({"k": "tool", "status": status_word(*status), "detail": detail, "spans": spans}),
        PartKind::Note => json!({"k": "note", "spans": spans}),
        PartKind::Error => json!({"k": "error", "spans": spans}),
        PartKind::Task { state } => json!({"k": "task", "state": crate::gui::parts::state_word(*state), "spans": spans}),
        PartKind::Picture { path, width, height } => json!({"k": "picture", "path": path, "w": width, "h": height}),
    }
}

fn block_json(blocks: &[Block], index: usize) -> Value {
    let block = &blocks[index];
    let parts: Vec<Value> = block_parts(block).iter().map(part_json).collect();
    let after_tool = index > 0 && matches!(blocks[index - 1], Block::Tool { .. });
    json!({"i": index, "tool": matches!(block, Block::Tool { .. }), "after_tool": after_tool, "tasks": matches!(block, Block::Tasks { .. }), "assistant": matches!(block, Block::Assistant { .. }), "parts": parts})
}

fn thread_json(app: &App, id: u64, since: Option<u64>, before: Option<usize>) -> Value {
    let Some(window) = app.windows.iter().find(|w| w.id == id) else { return json!({"missing": true}) };
    let Pane::Chat(chat) = &window.pane else { return json!({"missing": true}) };
    let count = chat.state.blocks.len();
    let (from, to) = match (before, since) {
        (Some(before), _) => (before.saturating_sub(FIRST_LOAD), before.min(count)),
        (None, Some(since)) => (chat.changed_since(since), count),
        (None, None) => (count.saturating_sub(FIRST_LOAD), count),
    };
    let from = from.min(to);
    let blocks: Vec<Value> = (from..to).map(|index| block_json(&chat.state.blocks, index)).collect();
    let provider = chat.state.provider();
    let account = if provider == Some(&olympus_core::target::Provider::Codex) { &chat.state.codex_account_dir } else { &chat.state.account_dir };
    json!({
        "id": id, "rev": chat.revision, "count": count, "from": from, "blocks": blocks,
        "place": short_path(&chat.state.cwd),
        "model": chrome::model_label(&chat.state.choice, chat.state.last_target.as_ref()),
        "account": crate::session::account_name(account.as_deref()),
        "full_profile": chat.state.full_profile,
        "running": window.activity == Activity::Running,
    })
}

fn css_color(color: gpui::Hsla) -> String {
    let rgb: gpui::Rgba = color.into();
    let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(rgb.r), byte(rgb.g), byte(rgb.b))
}

fn term_json(app: &App, id: u64) -> Value {
    let Some(Pane::Term(term)) = app.windows.iter().find(|w| w.id == id).map(|w| &w.pane) else { return json!({"missing": true}) };
    let Ok(parser) = term.pty.parser.lock() else { return json!({"missing": true}) };
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let (cursor_row, cursor_col) = screen.cursor_position();
    let show_cursor = !screen.hide_cursor() && screen.scrollback() == 0;
    let lines: Vec<Value> = (0..rows)
        .map(|row| {
            let cursor = (show_cursor && row == cursor_row).then_some(cursor_col);
            let runs: Vec<Value> = crate::gui::terminal::row_runs(screen, row, cols, cursor)
                .iter()
                .map(|run| json!({"c": run.column, "w": run.width, "t": run.text, "fg": css_color(run.fg), "bg": run.bg.map(css_color), "b": run.bold, "i": run.italic, "u": run.underline}))
                .collect();
            json!(runs)
        })
        .collect();
    json!({"id": id, "title": term.title, "rows": rows, "cols": cols, "lines": lines, "exited": term.pty.exited.load(Ordering::Relaxed)})
}

/// The state every open page keeps up to date with.
pub fn summary(app: &App) -> Value {
    let threads: Vec<Value> = (0..app.windows.len())
        .map(|index| {
            let window = &app.windows[index];
            let info = chrome::thread_info(app, index);
            let (kind, rev) = match &window.pane {
                Pane::Chat(chat) => (if crate::obiview::is_obiview(chat) { "obi" } else { "chat" }, chat.revision),
                Pane::Term(term) => ("term", term.pty.output.load(Ordering::Relaxed)),
            };
            let activity = match window.activity {
                Activity::Idle => "idle",
                Activity::Running => "running",
                Activity::Unseen => "unseen",
                Activity::Failed => "failed",
            };
            json!({"id": window.id, "kind": kind, "rev": rev, "title": window.title(), "who": info.who, "details": info.details, "used": info.used, "alert": info.alert, "mark": info.mark, "accounts": info.has_accounts, "activity": activity})
        })
        .collect();
    let picker = app.picker.as_ref().map(|picker| {
        let items: Vec<Value> = picker
            .items
            .iter()
            .enumerate()
            // A folder dialog would open on the PC, out of reach of whoever is on the phone.
            .filter(|(_, item)| !matches!(item.action, crate::app::Action::BrowseFolder(..) | crate::app::Action::BrowseSyncFolder))
            .map(|(index, item)| json!({"i": index, "label": item.label, "detail": item.detail}))
            .collect();
        json!({"title": picker.title, "items": items, "loading": picker.loading, "empty": picker.empty})
    });
    let limits: Vec<Value> = app.rt.limits.snapshot().into_iter().map(|limit| json!({"vendor": limit.vendor, "window": olympus_core::limits::window_word(&limit.window), "used": limit.used, "blocked": limit.blocked})).collect();
    json!({"threads": threads, "active": app.active_id(), "status": app.status, "picker": picker, "limits": limits, "reading": app.obi.observer.is_some()})
}

/// Sends the current state to every open page.
pub fn publish(app: &App) {
    let Some(mirror) = &app.mirror else { return };
    let text = summary(app).to_string();
    mirror.snapshots.send_if_modified(|current| {
        let changed = **current != text;
        if changed {
            *current = Arc::new(text);
        }
        changed
    });
}

fn attached(name: &str) -> Option<PathBuf> {
    let probe = Path::new(name);
    if probe.file_name().is_none_or(|file| file != name) {
        return None;
    }
    let folder = crate::capture::new_attachment_path("x").parent()?.to_path_buf();
    let path = folder.join(name);
    path.is_file().then_some(path)
}

// Runs something as if this thread were focused, then gives the PC's own focus back.
fn in_thread(app: &mut App, id: u64, work: impl FnOnce(&mut App)) -> Value {
    let Some(index) = app.windows.iter().position(|w| w.id == id) else { return json!({"flash": "That thread is gone."}) };
    let home = app.active_id();
    let draft = app.composer.text.clone();
    app.active = index;
    work(app);
    let now = app.active_id();
    let prefill = (app.composer.text != draft).then(|| app.composer.text.clone());
    if prefill.is_some() {
        app.composer.set(&draft);
    }
    if let Some(back) = app.windows.iter().position(|w| w.id == home) {
        app.active = back;
    }
    json!({"open": (now != id).then_some(now), "prefill": prefill, "flash": app.pending_flash})
}

fn key_bytes(name: &str) -> Option<Vec<u8>> {
    use crate::input::{KeyCode, KeyEvent, KeyModifiers};
    let plain = KeyModifiers::default();
    let (code, modifiers) = match name {
        "enter" => (KeyCode::Enter, plain),
        "esc" => (KeyCode::Esc, plain),
        "tab" => (KeyCode::Tab, plain),
        "backspace" => (KeyCode::Backspace, plain),
        "up" => (KeyCode::Up, plain),
        "down" => (KeyCode::Down, plain),
        "left" => (KeyCode::Left, plain),
        "right" => (KeyCode::Right, plain),
        "ctrl-c" => (KeyCode::Char('c'), KeyModifiers { ctrl: true, alt: false, shift: false }),
        "ctrl-d" => (KeyCode::Char('d'), KeyModifiers { ctrl: true, alt: false, shift: false }),
        _ => return None,
    };
    Some(crate::pty::key_bytes(KeyEvent::new(code, modifiers)))
}

fn act(app: &mut App, action: Act) -> Value {
    match action {
        Act::Send { thread, text, files } => {
            if text.trim() == "/quit" {
                return json!({"flash": "Quit from the PC. From here, you couldn't start it again."});
            }
            let files: Vec<PathBuf> = files.iter().filter_map(|name| attached(name)).collect();
            in_thread(app, thread, move |app| {
                let waiting = std::mem::replace(&mut app.attachments, files);
                app.deliver(text);
                app.attachments = waiting;
            })
        }
        Act::Stop { thread } => in_thread(app, thread, |app| {
            app.stop_active();
        }),
        Act::Close { thread } => match app.windows.iter().position(|w| w.id == thread) {
            Some(index) => {
                app.close_window(index);
                json!({})
            }
            None => json!({}),
        },
        Act::Seen { thread } => {
            if let Some(window) = app.window_mut(thread) {
                if window.activity == Activity::Unseen {
                    window.activity = Activity::Idle;
                }
            }
            json!({})
        }
        Act::Term { thread, text } => {
            if let Some(crate::session::Window { pane: Pane::Term(term), .. }) = app.window_mut(thread) {
                term.pty.paste(&text);
            }
            json!({})
        }
        Act::Key { thread, key } => {
            if let (Some(bytes), Some(crate::session::Window { pane: Pane::Term(term), .. })) = (key_bytes(&key), app.window_mut(thread)) {
                term.pty.send(&bytes);
            }
            json!({})
        }
        Act::Scroll { thread, lines } => in_thread(app, thread, move |app| app.scroll(lines)),
        Act::Pick { thread, item } => {
            let chosen = app.picker.as_ref().and_then(|picker| picker.items.get(item)).map(|row| row.action.clone());
            app.picker = None;
            match chosen {
                Some(action) => in_thread(app, thread, move |app| commands::pick(app, action)),
                None => json!({}),
            }
        }
        Act::PickClose => {
            app.picker = None;
            json!({})
        }
        Act::Palette { thread } => in_thread(app, thread, |app| app.picker = Some(commands::palette(app))),
    }
}

fn meta_json(app: &App) -> Value {
    let hints: Vec<Value> = commands::COMMANDS.iter().map(commands::Hint::from).chain(app.skill_hints.iter().cloned()).map(|hint| json!({"name": hint.name, "usage": hint.usage, "about": hint.about, "args": hint.takes_args})).collect();
    json!({"hints": hints, "providers": app.provider_lines, "version": env!("CARGO_PKG_VERSION")})
}

/// Answers a question on the interface thread.
pub fn answer(app: &mut App, ask: Ask) -> Value {
    match ask {
        Ask::Meta => meta_json(app),
        Ask::Thread { id, since, before } => thread_json(app, id, since, before),
        Ask::Term { id } => term_json(app, id),
        Ask::DiffSource { id } => {
            let Some(Pane::Chat(chat)) = app.windows.iter().find(|w| w.id == id).map(|w| &w.pane) else { return json!({"missing": true}) };
            let branch = match app.diffs.get(&id).map(|state| &state.source) {
                Some(crate::diff::DiffSource::Branch(branch)) => Some(branch.clone()),
                _ => None,
            };
            json!({"cwd": chat.state.cwd, "branch": branch})
        }
        Ask::Act(action) => act(app, action),
        Ask::Obi(request) => crate::obiview::answer(app, request),
    }
}

struct Shared {
    tx: UnboundedSender<AppEvent>,
    door: Arc<Mutex<Door>>,
    open: AtomicBool,
    events: watch::Receiver<Arc<String>>,
    boot: String,
}

/// A running server; dropping it locks the door and ends every open page's connection.
pub struct Mirror {
    shared: Arc<Shared>,
    snapshots: watch::Sender<Arc<String>>,
    task: JoinHandle<()>,
    pub port: u16,
    pub door: Arc<Mutex<Door>>,
}

impl Drop for Mirror {
    fn drop(&mut self) {
        self.shared.open.store(false, Ordering::Relaxed);
        self.task.abort();
    }
}

impl Mirror {
    /// Binds now, so a port in use is reported at once, then serves in the background.
    pub fn start(port: u16, devices_file: PathBuf, tx: UnboundedSender<AppEvent>) -> std::io::Result<Self> {
        let listener = std::net::TcpListener::bind(("0.0.0.0", port))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let (snapshots, events) = watch::channel(Arc::new(String::new()));
        let door = Arc::new(Mutex::new(Door::load(devices_file)));
        let shared = Arc::new(Shared { tx, door: door.clone(), open: AtomicBool::new(true), events, boot: random_code() });
        let app = router(shared.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await;
        });
        Ok(Self { shared, snapshots, task, port, door })
    }

    /// Where a phone opens this, with the one-time code for pairing.
    pub fn pairing_url(&self) -> String {
        let code = self.door.lock().map(|mut door| door.code()).unwrap_or_default();
        format!("{}/#code={code}", self.address())
    }

    /// Where a browser finds this.
    pub fn address(&self) -> String {
        format!("http://{}:{}", lan_address().map_or_else(|| "this-pc".to_string(), |ip| ip.to_string()), self.port)
    }
}

/// The address this PC uses on its network; connecting UDP sends nothing, it only picks a route.
pub fn lan_address() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    socket.local_addr().ok().map(|address| address.ip()).filter(|ip| !ip.is_unspecified())
}

fn router(shared: Arc<Shared>) -> Router {
    let private = Router::new()
        .route("/api/meta", get(meta))
        .route("/api/events", get(events))
        .route("/api/thread/{id}", get(thread))
        .route("/api/term/{id}", get(term))
        .route("/api/changes/{id}", get(changes))
        .route("/api/image", get(image))
        .route("/api/do", post(perform))
        .route("/api/upload", post(upload).layer(DefaultBodyLimit::max(MAX_UPLOAD)))
        .route("/api/obi", post(obi))
        .route_layer(middleware::from_fn_with_state(shared.clone(), paired));
    Router::new()
        .merge(private)
        .route("/api/pair", post(pair))
        .route("/", get(|| asset("text/html; charset=utf-8", include_str!("../assets/mirror/index.html").as_bytes())))
        .route("/app.js", get(|| asset("text/javascript; charset=utf-8", include_str!("../assets/mirror/app.js").as_bytes())))
        .route("/obi.js", get(|| asset("text/javascript; charset=utf-8", include_str!("../assets/mirror/obi.js").as_bytes())))
        .route("/app.css", get(|| asset("text/css; charset=utf-8", include_str!("../assets/mirror/app.css").as_bytes())))
        .route("/theme.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], crate::theme::css()) }))
        .route("/manifest.webmanifest", get(|| asset("application/manifest+json", include_str!("../assets/mirror/manifest.webmanifest").as_bytes())))
        .route("/font/regular.ttf", get(|| asset("font/ttf", include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"))))
        .route("/font/medium.ttf", get(|| asset("font/ttf", include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf"))))
        .route("/font/bold.ttf", get(|| asset("font/ttf", include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf"))))
        .route("/font/italic.ttf", get(|| asset("font/ttf", include_bytes!("../assets/fonts/JetBrainsMono-Italic.ttf"))))
        .route("/icon/olympus.svg", get(|| asset("image/svg+xml", include_bytes!("../assets/icon/olympus.svg"))))
        .route("/icon/claude.svg", get(|| asset("image/svg+xml", include_bytes!("../assets/icon/claude.svg"))))
        .route("/icon/openai.svg", get(|| asset("image/svg+xml", include_bytes!("../assets/icon/openai.svg"))))
        .route("/icon/jarvis.svg", get(|| asset("image/svg+xml", include_bytes!("../assets/icon/jarvis.svg"))))
        .layer(middleware::from_fn_with_state(shared.clone(), gate))
        .with_state(shared)
}

async fn asset(kind: &'static str, bytes: &'static [u8]) -> Response {
    ([(header::CONTENT_TYPE, kind)], bytes).into_response()
}

// Every response, including static files, passes here, so the switch and the network limit always hold.
async fn gate(State(shared): State<Arc<Shared>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, request: Request, next: Next) -> Response {
    if !shared.open.load(Ordering::Relaxed) || !is_local_peer(peer.ip()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'self'; img-src 'self' blob:; style-src 'self' 'unsafe-inline'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")
}

async fn paired(State(shared): State<Arc<Shared>>, request: Request, next: Next) -> Response {
    let allowed = bearer(request.headers()).is_some_and(|token| shared.door.lock().is_ok_and(|door| door.allows(token)));
    if allowed {
        next.run(request).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

async fn ask(shared: &Shared, question: Ask) -> Response {
    let (reply, answered) = oneshot::channel();
    if shared.tx.send(AppEvent::Mirror(question, reply)).is_err() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    match tokio::time::timeout(Duration::from_secs(30), answered).await {
        Ok(Ok(value)) => Json(value).into_response(),
        _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

#[derive(Deserialize)]
struct PairBody {
    code: String,
    #[serde(default)]
    name: String,
}

async fn pair(State(shared): State<Arc<Shared>>, Json(body): Json<PairBody>) -> Response {
    let token = shared.door.lock().ok().and_then(|mut door| door.pair(&body.code, &body.name));
    match token {
        Some(token) => Json(json!({"token": token})).into_response(),
        None => StatusCode::FORBIDDEN.into_response(),
    }
}

async fn meta(State(shared): State<Arc<Shared>>) -> Response {
    ask(&shared, Ask::Meta).await
}

async fn events(State(shared): State<Arc<Shared>>) -> impl IntoResponse {
    let mut events = shared.events.clone();
    events.mark_changed();
    let boot = shared.boot.clone();
    let hello = Event::default().event("hello").data(boot);
    let updates = futures_util::stream::unfold((events, true), |(mut events, first)| async move {
        // Streaming text changes many times a second, and a phone only needs a few of them.
        if !first {
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        events.changed().await.ok()?;
        let text = events.borrow_and_update().clone();
        Some((Ok::<Event, Infallible>(Event::default().data(text.as_str())), (events, false)))
    });
    let stream = futures_util::StreamExt::chain(futures_util::stream::once(async move { Ok::<Event, Infallible>(hello) }), updates);
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct ThreadQuery {
    since: Option<u64>,
    before: Option<usize>,
}

async fn thread(State(shared): State<Arc<Shared>>, UrlPath(id): UrlPath<u64>, Query(query): Query<ThreadQuery>) -> Response {
    ask(&shared, Ask::Thread { id, since: query.since, before: query.before }).await
}

async fn term(State(shared): State<Arc<Shared>>, UrlPath(id): UrlPath<u64>) -> Response {
    ask(&shared, Ask::Term { id }).await
}

async fn perform(State(shared): State<Arc<Shared>>, Json(action): Json<Act>) -> Response {
    ask(&shared, Ask::Act(action)).await
}

async fn obi(State(shared): State<Arc<Shared>>, Json(request): Json<crate::obiview::Request>) -> Response {
    ask(&shared, Ask::Obi(request)).await
}

fn diff_json(files: &[crate::diff::FileDiff]) -> Value {
    use crate::diff::LineKind;
    let files: Vec<Value> = files
        .iter()
        .map(|file| {
            let lines: Vec<Value> = file
                .lines
                .iter()
                .map(|line| {
                    let kind = match line.kind {
                        LineKind::Hunk => "hunk",
                        LineKind::Context => "context",
                        LineKind::Add => "add",
                        LineKind::Remove => "remove",
                    };
                    json!({"k": kind, "old": line.old, "new": line.new, "t": line.text})
                })
                .collect();
            json!({"path": file.path, "added": file.added, "removed": file.removed, "binary": file.binary, "lines": lines})
        })
        .collect();
    json!({"files": files})
}

async fn changes(State(shared): State<Arc<Shared>>, UrlPath(id): UrlPath<u64>) -> Response {
    let (reply, answered) = oneshot::channel();
    if shared.tx.send(AppEvent::Mirror(Ask::DiffSource { id }, reply)).is_err() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Ok(Ok(place)) = tokio::time::timeout(Duration::from_secs(30), answered).await else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    let Some(cwd) = place["cwd"].as_str().map(PathBuf::from) else { return StatusCode::NOT_FOUND.into_response() };
    let source = place["branch"].as_str().map_or(crate::diff::DiffSource::Working, |branch| crate::diff::DiffSource::Branch(branch.to_string()));
    match crate::diff::load(&cwd, &source).await {
        Ok(files) => Json(diff_json(&files)).into_response(),
        Err(problem) => Json(json!({"error": problem})).into_response(),
    }
}

#[derive(Deserialize)]
struct ImageQuery {
    path: PathBuf,
}

async fn image(Query(query): Query<ImageQuery>) -> Response {
    if !query.path.is_absolute() || !olympus_core::agent::is_image(&query.path) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let kind = match query.path.extension().map(|ext| ext.to_string_lossy().to_ascii_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/jpeg",
    };
    match tokio::fs::read(&query.path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, kind)], Body::from(bytes)).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
struct UploadQuery {
    ext: String,
}

async fn upload(Query(query): Query<UploadQuery>, body: Bytes) -> Response {
    let ext = query.ext.to_ascii_lowercase();
    if !UPLOAD_TYPES.contains(&ext.as_str()) {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "Olympus can't take that kind of file.").into_response();
    }
    let path = crate::capture::new_attachment_path(&ext);
    let saved = async {
        if let Some(folder) = path.parent() {
            tokio::fs::create_dir_all(folder).await?;
        }
        tokio::fs::write(&path, &body).await
    }
    .await;
    match (saved, path.file_name()) {
        (Ok(()), Some(name)) => Json(json!({"file": name.to_string_lossy()})).into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "Olympus couldn't save that file.").into_response(),
    }
}

/// Asks Windows to let your network reach the port; only ever from clicking the switch.
fn allow_through_firewall(port: u16) {
    use windows::core::{w, HSTRING};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    if !crate::update::in_real_profile() {
        return;
    }
    let known = std::process::Command::new("netsh").args(["advfirewall", "firewall", "show", "rule"]).arg(format!("name={FIREWALL_RULE}")).output();
    if known.is_ok_and(|output| output.status.success()) {
        return;
    }
    let args = format!("advfirewall firewall add rule name=\"{FIREWALL_RULE}\" dir=in action=allow protocol=TCP localport={port} remoteip=localsubnet profile=any");
    // SAFETY: both strings outlive the call, and no owner window is needed.
    unsafe { ShellExecuteW(None, w!("runas"), w!("netsh.exe"), &HSTRING::from(args), None, SW_HIDE) };
}

fn chosen_port() -> u16 {
    std::env::var(PORT_VAR).ok().and_then(|port| port.parse().ok()).unwrap_or(DEFAULT_PORT)
}

fn start(app: &mut App) -> std::io::Result<()> {
    let mirror = Mirror::start(chosen_port(), home_dir().join(DEVICES_FILE), app.tx.clone())?;
    app.mirror = Some(mirror);
    publish(app);
    Ok(())
}

fn explain(problem: &std::io::Error) -> String {
    format!("Phone access couldn't start on port {}: {problem}. Another Olympus may already have it.", chosen_port())
}

/// Starts the mirror at launch if it was left on.
pub fn resume(app: &mut App) {
    if home_dir().join(MIRROR_ON).exists() {
        if let Err(problem) = start(app) {
            app.status = explain(&problem);
        }
    }
}

/// What clicking the switch does.
pub fn toggle(app: &mut App) {
    let marker = home_dir().join(MIRROR_ON);
    if app.mirror.take().is_some() {
        let _ = std::fs::remove_file(marker);
        app.mirror_panel = false;
        app.pending_flash = Some("Phone access is off.".to_string());
        return;
    }
    match start(app) {
        Ok(()) => {
            // Saying so is enough: without the marker it only lasts until the next launch.
            let _ = std::fs::write(marker, "");
            let port = app.mirror.as_ref().map_or(DEFAULT_PORT, |mirror| mirror.port);
            std::thread::spawn(move || allow_through_firewall(port));
            app.mirror_panel = true;
        }
        Err(problem) => app.pending_flash = Some(explain(&problem)),
    }
}

/// `/mirror`: opens the pairing panel, or `on`, `off` or `forget`.
pub fn command(app: &mut App, args: &str) {
    match args {
        "" if app.mirror.is_some() => app.mirror_panel = true,
        "" | "on" if app.mirror.is_none() => toggle(app),
        "on" => app.mirror_panel = true,
        "off" if app.mirror.is_some() => toggle(app),
        "off" => app.note_active("Phone access is already off."),
        "forget" => forget(app),
        _ => app.fail_active("Use /mirror, /mirror on, /mirror off or /mirror forget."),
    }
}

/// Unpairs every browser, including while the mirror is off.
pub fn forget(app: &mut App) {
    match &app.mirror {
        Some(mirror) => {
            if let Ok(mut door) = mirror.door.lock() {
                door.forget_all();
            }
        }
        None => {
            let _ = std::fs::remove_file(home_dir().join(DEVICES_FILE));
        }
    }
    app.pending_flash = Some("Every phone is unpaired. Each one needs the code again.".to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testing::app;
    use crate::markdown::Inline;
    use olympus_core::openai::client;

    fn door() -> (Door, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("dir");
        (Door::load(dir.path().join("devices.json")), dir)
    }

    #[test]
    fn a_code_pairs_once_and_the_key_survives_a_restart_as_a_hash() {
        let (mut door, dir) = door();
        let code = door.code();
        let token = door.pair(&code.to_lowercase(), "phone").expect("paired");
        assert!(door.allows(&token));
        assert!(!door.allows("guess"));
        assert!(door.pair(&code, "second").is_none(), "a code works once");
        let saved = std::fs::read_to_string(dir.path().join("devices.json")).expect("saved");
        assert!(!saved.contains(&token), "only a hash reaches the disk");
        assert!(Door::load(dir.path().join("devices.json")).allows(&token));
    }

    #[test]
    fn wrong_guesses_kill_the_code() {
        let (mut door, _dir) = door();
        let code = door.code();
        for _ in 0..MAX_WRONG_CODES {
            assert!(door.pair("WRONG", "x").is_none());
        }
        assert!(door.pair(&code, "x").is_none(), "even the right code is dead now");
        assert_ne!(door.code(), code, "the panel gets a fresh one");
    }

    #[test]
    fn forgetting_locks_everyone_out() {
        let (mut door, _dir) = door();
        let code = door.code();
        let token = door.pair(&code, "phone").expect("paired");
        door.forget_all();
        assert!(!door.allows(&token));
    }

    #[test]
    fn only_addresses_on_your_own_network_count_as_local() {
        for good in ["192.168.1.9", "10.0.0.2", "172.16.4.4", "127.0.0.1", "100.100.1.1", "169.254.1.1", "::1", "fe80::1", "fd00::1", "::ffff:192.168.1.9"] {
            assert!(is_local_peer(good.parse().expect("ip")), "{good}");
        }
        for bad in ["8.8.8.8", "100.128.0.1", "172.32.0.1", "2001:4860::1", "::ffff:8.8.8.8"] {
            assert!(!is_local_peer(bad.parse().expect("ip")), "{bad}");
        }
    }

    #[test]
    fn styled_text_keeps_its_links_and_styles_across_multibyte_text() {
        let mut rich = Rich::default();
        rich.push("åäö ", Inline::default());
        rich.push("link", Inline { link: true, ..Inline::default() });
        rich.push(" bold", Inline { bold: true, ..Inline::default() });
        rich.links.push((7..11, "https://example.com".into()));
        let spans = rich_json(&rich);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0]["t"], "åäö ");
        assert_eq!(spans[1]["href"], "https://example.com");
        assert_eq!(spans[2]["b"], true);
    }

    #[test]
    fn a_page_only_gets_the_blocks_that_changed() {
        let (mut app, _rx, _dir) = app(true);
        let id = app.active_id();
        let chat = app.chat().expect("chat");
        chat.push(Block::Note("one".into()));
        chat.push(Block::Note("two".into()));
        let rev = chat.revision;
        chat.push(Block::Note("three".into()));
        let fresh = thread_json(&app, id, None, None);
        assert_eq!(fresh["blocks"].as_array().expect("blocks").len(), 3);
        let change = thread_json(&app, id, Some(rev), None);
        assert_eq!(change["from"], 2);
        assert_eq!(change["blocks"].as_array().expect("blocks").len(), 1);
        let none = thread_json(&app, id, Some(rev + 1), None);
        assert_eq!(none["blocks"].as_array().expect("blocks").len(), 0);
    }

    #[test]
    fn an_upload_name_cannot_walk_out_of_the_attachments_folder() {
        for name in ["..\\secret.txt", "../secret.txt", "C:\\Windows\\win.ini", "a/b.png", ""] {
            assert!(attached(name).is_none(), "{name}");
        }
    }

    #[tokio::test]
    async fn sending_from_a_phone_reaches_that_thread_and_leaves_the_pc_where_it_was() {
        let (mut app, _rx, dir) = app(true);
        let first = app.active_id();
        let second = app.new_chat(dir.path().to_path_buf());
        app.composer.set("my draft");
        let reply = answer(&mut app, Ask::Act(Act::Send { thread: first, text: "/rename from-phone".into(), files: Vec::new() }));
        assert!(reply["open"].is_null());
        assert_eq!(app.active_id(), second, "the PC stays on its own thread");
        assert_eq!(app.composer.text, "my draft", "and keeps its draft");
        assert_eq!(app.windows[0].title(), "from-phone");
    }

    #[tokio::test]
    async fn the_phone_cannot_quit_what_it_could_not_restart() {
        let (mut app, _rx, _dir) = app(true);
        let id = app.active_id();
        let reply = answer(&mut app, Ask::Act(Act::Send { thread: id, text: "/quit".into(), files: Vec::new() }));
        assert!(!app.quit);
        assert!(reply["flash"].as_str().is_some());
    }

    #[tokio::test]
    async fn a_new_thread_from_the_phone_opens_there_but_not_on_the_pc() {
        let (mut app, _rx, _dir) = app(true);
        let home = app.active_id();
        let reply = answer(&mut app, Ask::Act(Act::Send { thread: home, text: "/new".into(), files: Vec::new() }));
        let opened = reply["open"].as_u64().expect("opened");
        assert_ne!(opened, home);
        assert_eq!(app.active_id(), home);
    }

    #[tokio::test]
    async fn the_real_server_refuses_strangers_and_serves_the_paired() {
        let (mut app, mut rx, dir) = app(true);
        let mirror = Mirror::start(0, dir.path().join("devices.json"), app.tx.clone()).expect("bound");
        let base = format!("http://127.0.0.1:{}", mirror.port);
        let code = mirror.door.lock().expect("door").code();
        app.mirror = Some(mirror);
        publish(&app);
        let serve = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if let AppEvent::Mirror(ask, reply) = event {
                    let _ = reply.send(answer(&mut app, ask));
                }
            }
            app
        });
        let http = client();
        let page = http.get(format!("{base}/")).send().await.expect("page");
        assert!(page.status().is_success());
        assert!(page.headers().get("content-security-policy").is_some());
        assert_eq!(http.get(format!("{base}/api/meta")).send().await.expect("meta").status(), 401);
        assert_eq!(http.post(format!("{base}/api/pair")).json(&json!({"code": "NOPE"})).send().await.expect("pair").status(), 403);
        let paired: Value = http.post(format!("{base}/api/pair")).json(&json!({"code": code, "name": "test"})).send().await.expect("pair").json().await.expect("json");
        let token = paired["token"].as_str().expect("token").to_string();
        let meta: Value = http.get(format!("{base}/api/meta")).bearer_auth(&token).send().await.expect("meta").json().await.expect("json");
        assert!(meta["hints"].as_array().expect("hints").iter().any(|hint| hint["name"] == "/mirror"));
        let mut stream = http.get(format!("{base}/api/events")).bearer_auth(&token).send().await.expect("events");
        let mut seen = String::new();
        while !seen.contains("threads") {
            let chunk = tokio::time::timeout(Duration::from_secs(5), stream.chunk()).await.expect("in time").expect("chunk").expect("more");
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        serve.abort();
    }

    #[tokio::test]
    async fn turning_the_switch_off_cuts_every_open_page_and_stops_listening() {
        let (app, _rx, dir) = app(true);
        let mirror = Mirror::start(0, dir.path().join("devices.json"), app.tx.clone()).expect("bound");
        let base = format!("http://127.0.0.1:{}", mirror.port);
        let code = mirror.door.lock().expect("door").code();
        let token = mirror.door.lock().expect("door").pair(&code, "t").expect("paired");
        let mut stream = client().get(format!("{base}/api/events")).bearer_auth(&token).send().await.expect("events");
        let hello = stream.chunk().await.expect("chunk").expect("hello");
        assert!(String::from_utf8_lossy(&hello).contains("hello"));
        drop(mirror);
        let ended = tokio::time::timeout(Duration::from_secs(5), async {
            while let Ok(Some(_)) = stream.chunk().await {}
        })
        .await;
        assert!(ended.is_ok(), "an open page's stream ends");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let after = client().get(format!("{base}/")).send().await;
        assert!(after.is_err() || after.is_ok_and(|reply| reply.status() == 403), "nothing is served once it's off");
    }
}
