use crate::store::{ProcessChange, Store, DEFAULT_RAW_DAYS};
use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::core::{BOOL, BSTR, PWSTR};
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, GetWindowDC, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::Accessibility::{
    AutomationElementMode_Full, AutomationElementMode_None, TreeScope_Subtree, CUIAutomation, CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationCacheRequest, IUIAutomationElement, IUIAutomationTreeWalker, SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK, UIA_AutomationIdPropertyId, UIA_ClassNamePropertyId, UIA_ComboBoxControlTypeId, UIA_CONTROLTYPE_ID, UIA_ControlTypePropertyId,
    UIA_DocumentControlTypeId, UIA_E_TIMEOUT, UIA_EditControlTypeId, UIA_IsPasswordPropertyId, UIA_MenuBarControlTypeId, UIA_NamePropertyId, UIA_PaneControlTypeId, UIA_PROPERTY_ID, UIA_ProgressBarControlTypeId, UIA_ScrollBarControlTypeId, UIA_SeparatorControlTypeId, UIA_SliderControlTypeId, UIA_SpinnerControlTypeId, UIA_ThumbControlTypeId, UIA_TitleBarControlTypeId,
    UIA_ToolBarControlTypeId, UIA_ValueValuePropertyId, UIA_WindowControlTypeId,
};
use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EnumChildWindows, EnumWindows, GetClassNameW, GetForegroundWindow, GetWindow, GetWindowDisplayAffinity, GetWindowLongW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsHungAppWindow, IsWindowVisible, MsgWaitForMultipleObjectsEx, PeekMessageW, PostThreadMessageW, EVENT_SYSTEM_FOREGROUND, GWL_EXSTYLE, GW_OWNER, MSG, MWMO_INPUTAVAILABLE, PM_NOREMOVE, PM_REMOVE, QS_ALLINPUT, WINEVENT_OUTOFCONTEXT, WM_QUIT, WS_EX_TOOLWINDOW,
};

const MAX_TEXT_BYTES: usize = 64 * 1024;
const GAP: &str = "\n...\n";
const SHORT_WORD_CHARS: usize = 12;
const MIN_NEW_DISTANCE: u32 = 10;
const CONVERSATION_RECHECK_SECS: i64 = 60;
const OTHER_RECHECK_SECS: i64 = 120;
const PASSWORD_VAULT_TITLES: [&str; 6] = ["bitwarden", "1password", "lastpass", "dashlane", "keepass", "proton pass"];
// Text of these is left out completely, along with the app's name and what's saved while it's in front.
const BANKING_TITLES: [&str; 8] = ["bank", "nordea", "klarna", "paypal", "revolut", "avanza", "swish", "kivra"];
const SHORTEST_EXCLUDED_WORD: usize = 3;
const AVERAGE_OVER: usize = 8;
const WALK_DEPTH: usize = 35;
const WALK_NODES: usize = 5000;
const WALK_BUDGET: Duration = Duration::from_millis(250);
const SLOWEST_TIER_NODES: usize = 500;
const SWEEP_NODES: usize = 1000;
const SWEEP_TOTAL: Duration = Duration::from_secs(3);
const WAKE_FOR: Duration = Duration::from_secs(2);
const WAKE_RETRY: Duration = Duration::from_millis(250);
const WAKE_STEADY_READS: u32 = 2;
const UIA_TIMEOUT_MS: u32 = 1000;
const OCR_BELOW_CHARS: usize = 50;
const OCR_EVERY_MS: u64 = 10_000;
const LARGEST_CAPTURE: i32 = 8192;
const PRUNE_EVERY: Duration = Duration::from_secs(24 * 3600);
const REMEMBERED_WINDOWS: usize = 512;
const WDA_NONE: u32 = 0;
// PW_RENDERFULLCONTENT: without it Chromium and DirectX windows capture as black.
const FULL_CONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(2);
const CHROMIUM_CLASS: &str = "Chrome_WidgetWin_1";
const UWP_FRAME_CLASS: &str = "ApplicationFrameWindow";
const SHELL_CLASSES: [&str; 4] = ["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd"];

const DEFAULT_EXCLUDED: [&str; 8] = ["1password", "bitwarden", "keepass", "lastpass", "dashlane", "credentialuibroker", "logonui", "consent"];
const PRIVATE_TITLES: [&str; 15] = ["inprivate", "incognito", "inkognito", "incógnito", "private browsing", "privat surfning", "privat browsing", "privat nettlesing", "privates surfen", "privater modus", "navigation privée", "navegación privada", "navegação privada", "navigazione anonima", "prywatne przeglądanie"];
const SECRET_FIELDS: [&str; 7] = ["password", "passwd", "passcode", "lösenord", "losenord", "passwort", "secret"];
const NOISE_PROCESSES: [&str; 32] = [
    "[system process]", "system", "registry", "memory compression", "smss", "csrss", "wininit", "winlogon", "services", "lsass", "svchost", "conhost", "dllhost", "dwm", "sihost", "taskhostw", "ctfmon", "fontdrvhost", "runtimebroker", "searchhost", "searchindexer", "searchprotocolhost", "searchfilterhost", "wmiprvse", "audiodg", "spoolsv", "wudfhost", "backgroundtaskhost",
    "startmenuexperiencehost", "shellexperiencehost", "textinputhost", "applicationframehost",
];

const BROWSERS: [&str; 12] = ["msedge", "chrome", "firefox", "brave", "opera", "vivaldi", "arc", "zen", "librewolf", "waterfox", "chromium", "iexplore"];
const EDITORS: [&str; 18] = ["code", "code - insiders", "cursor", "windsurf", "zed", "devenv", "rider64", "idea64", "pycharm64", "webstorm64", "clion64", "goland64", "rustrover64", "sublime_text", "notepad++", "nvim-qt", "neovide", "antigravity"];
const TERMINALS: [&str; 10] = ["windowsterminal", "wt", "cmd", "powershell", "pwsh", "alacritty", "wezterm-gui", "mintty", "hyper", "tabby"];
const DOCUMENTS: [&str; 10] = ["winword", "excel", "powerpnt", "onenote", "notepad", "acrobat", "acrord32", "sumatrapdf", "obsidian", "notion"];
const NATIVE_APPS: [(&str, Kind, &str); 14] = [
    ("ms-teams", Kind::Chat, "Teams"),
    ("teams", Kind::Chat, "Teams"),
    ("slack", Kind::Chat, "Slack"),
    ("discord", Kind::Chat, "Discord"),
    ("whatsapp", Kind::Chat, "WhatsApp"),
    ("whatsapp.root", Kind::Chat, "WhatsApp"),
    ("telegram", Kind::Chat, "Telegram"),
    ("signal", Kind::Chat, "Signal"),
    ("outlook", Kind::Mail, "Outlook"),
    ("olk", Kind::Mail, "Outlook"),
    ("hxoutlook", Kind::Mail, "Windows Mail"),
    ("thunderbird", Kind::Mail, "Thunderbird"),
    ("hxcalendarappimm", Kind::Calendar, "Windows Calendar"),
    ("zoom", Kind::Meeting, "Zoom"),
];
// The first match wins, so a site's calendar path comes before the site itself.
const SITES: [(&str, &str, Kind, &str); 22] = [
    ("mail.google.com", "", Kind::Mail, "Gmail"),
    ("calendar.google.com", "", Kind::Calendar, "Google Calendar"),
    ("meet.google.com", "", Kind::Meeting, "Google Meet"),
    ("outlook.office.com", "/calendar", Kind::Calendar, "Outlook"),
    ("outlook.office365.com", "/calendar", Kind::Calendar, "Outlook"),
    ("outlook.live.com", "/calendar", Kind::Calendar, "Outlook"),
    ("outlook.cloud.microsoft", "/calendar", Kind::Calendar, "Outlook"),
    ("outlook.office.com", "", Kind::Mail, "Outlook"),
    ("outlook.office365.com", "", Kind::Mail, "Outlook"),
    ("outlook.live.com", "", Kind::Mail, "Outlook"),
    ("outlook.cloud.microsoft", "", Kind::Mail, "Outlook"),
    ("calendar.proton.me", "", Kind::Calendar, "Proton"),
    ("mail.proton.me", "", Kind::Mail, "Proton"),
    ("mail.yahoo.com", "", Kind::Mail, "Yahoo Mail"),
    ("teams.microsoft.com", "", Kind::Chat, "Teams"),
    ("teams.cloud.microsoft", "", Kind::Chat, "Teams"),
    ("teams.live.com", "", Kind::Chat, "Teams"),
    ("slack.com", "", Kind::Chat, "Slack"),
    ("discord.com", "", Kind::Chat, "Discord"),
    ("web.whatsapp.com", "", Kind::Chat, "WhatsApp"),
    ("web.telegram.org", "", Kind::Chat, "Telegram"),
    ("zoom.us", "/wc", Kind::Meeting, "Zoom"),
];
const SITE_TITLES: [(&str, Kind, &str); 11] = [
    ("gmail", Kind::Mail, "Gmail"),
    ("outlook", Kind::Mail, "Outlook"),
    ("proton mail", Kind::Mail, "Proton"),
    ("yahoo mail", Kind::Mail, "Yahoo Mail"),
    ("microsoft teams", Kind::Chat, "Teams"),
    ("slack", Kind::Chat, "Slack"),
    ("discord", Kind::Chat, "Discord"),
    ("whatsapp", Kind::Chat, "WhatsApp"),
    ("telegram web", Kind::Chat, "Telegram"),
    ("google calendar", Kind::Calendar, "Google Calendar"),
    ("google meet", Kind::Meeting, "Google Meet"),
];
const CALENDAR_WORDS: [&str; 3] = ["calendar", "kalender", "calendrier"];
const MEETING_WORDS: [&str; 2] = ["meeting", "möte"];

/// What sort of thing a window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Mail,
    Chat,
    Calendar,
    Browser,
    Code,
    Terminal,
    Document,
    Meeting,
    Other,
}

impl Kind {
    /// The name stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mail => "mail",
            Self::Chat => "chat",
            Self::Calendar => "calendar",
            Self::Browser => "browser",
            Self::Code => "code",
            Self::Terminal => "terminal",
            Self::Document => "document",
            Self::Meeting => "meeting",
            Self::Other => "other",
        }
    }

    /// The kind for a stored name; anything unknown is `Other`.
    pub fn parse(name: &str) -> Self {
        match name {
            "mail" => Self::Mail,
            "chat" => Self::Chat,
            "calendar" => Self::Calendar,
            "browser" => Self::Browser,
            "code" => Self::Code,
            "terminal" => Self::Terminal,
            "document" => Self::Document,
            "meeting" => Self::Meeting,
            _ => Self::Other,
        }
    }
}

/// How the text was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum How {
    Uia,
    Ocr,
}

impl How {
    /// The name stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uia => "uia",
            Self::Ocr => "ocr",
        }
    }
}

/// Text read from one window at one moment.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Observation {
    pub at: i64,
    pub exe: String,
    pub title: String,
    pub url: Option<String>,
    pub kind: Kind,
    pub source: Option<String>,
    pub text: String,
    pub how: How,
    pub foreground: bool,
}

/// What the observer reads and how often.
#[derive(Clone, Debug)]
pub struct Settings {
    pub excluded_exes: Vec<String>,
    /// Words you chose: a window with one in its title isn't read, which also covers web apps in a browser.
    pub excluded_titles: Vec<String>,
    /// Apps you turned off, by exact name, without catching apps that merely start the same.
    pub off_exes: Vec<String>,
    /// When set, only these apps are read at all.
    pub only_exes: Option<Vec<String>>,
    /// Apps that count for time and titles, with their text left unread.
    pub time_only_exes: Vec<String>,
    pub read_text: bool,
    pub read_processes: bool,
    /// Folders watched for saved files.
    pub watch: Vec<std::path::PathBuf>,
    pub sweep: bool,
    pub ocr: bool,
    pub poll: Duration,
    pub sweep_every: Duration,
    /// How often windows are still read while you're away, so what comes in meanwhile isn't missed.
    pub idle_sweep_every: Duration,
    pub processes_every: Duration,
    pub idle_after: Duration,
    pub raw_days: u32,
    /// When not empty, only windows and processes with these ids are ever touched.
    pub only_pids: Vec<u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            excluded_exes: DEFAULT_EXCLUDED.iter().map(ToString::to_string).collect(),
            excluded_titles: Vec::new(),
            off_exes: Vec::new(),
            only_exes: None,
            time_only_exes: Vec::new(),
            read_text: true,
            read_processes: true,
            watch: Vec::new(),
            sweep: true,
            ocr: true,
            poll: Duration::from_secs(3),
            sweep_every: Duration::from_secs(60),
            idle_sweep_every: Duration::from_secs(300),
            processes_every: Duration::from_secs(60),
            idle_after: Duration::from_secs(300),
            raw_days: DEFAULT_RAW_DAYS,
            only_pids: Vec::new(),
        }
    }
}

/// A snapshot of what the observer has been doing.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Status {
    pub running: bool,
    pub paused: bool,
    pub walks: u64,
    pub stored: u64,
    pub last_app: Option<String>,
    pub last_walk_ms: u64,
    pub last_error: Option<String>,
}

/// The app name from a path or file name: lower case, no folder, no `.exe`.
pub fn normalize_exe(exe: &str) -> String {
    let name = exe.rsplit(['/', '\\']).next().unwrap_or(exe).to_lowercase();
    name.strip_suffix(".exe").map_or_else(|| name.clone(), str::to_string)
}

/// What people call a program whose file name doesn't say.
pub fn plain_name(exe: &str) -> Option<&'static str> {
    const NAMES: [(&str, &str); 14] = [("msedge", "Microsoft Edge"), ("chrome", "Google Chrome"), ("firefox", "Firefox"), ("brave", "Brave"), ("opera", "Opera"), ("vivaldi", "Vivaldi"), ("windowsterminal", "Windows Terminal"), ("cmd", "Command Prompt"), ("powershell", "PowerShell"), ("pwsh", "PowerShell"), ("explorer", "File Explorer"), ("code", "VS Code"), ("devenv", "Visual Studio"), ("steamwebhelper", "Steam")];
    let exe = normalize_exe(exe);
    NAMES.iter().find(|(known, _)| *known == exe).map(|(_, name)| *name)
}

/// True for system processes that aren't apps you use.
pub fn is_noise_process(exe: &str) -> bool {
    NOISE_PROCESSES.contains(&exe)
}

/// True for password managers and the like, which can't be turned on.
pub fn never_read(exe: &str) -> bool {
    let exe = normalize_exe(exe);
    DEFAULT_EXCLUDED.iter().any(|name| exe.starts_with(name))
}

/// True for web browsers, whose windows hold many sites.
pub fn is_browser(exe: &str) -> bool {
    BROWSERS.contains(&normalize_exe(exe).as_str())
}

/// The apps with a window on screen now, as names and where their program is.
pub fn open_apps(only_pids: &[u32]) -> Vec<(String, String)> {
    let mut apps: Vec<(String, String)> = visible_windows(only_pids).into_iter().filter_map(|window| path_of_pid(app_pid(window, window_pid(window)))).map(|path| (normalize_exe(&path), path)).collect();
    apps.sort();
    apps.dedup_by(|a, b| a.0 == b.0);
    apps
}

/// Works out what a window is and which source it belongs to.
pub fn classify(exe: &str, title: &str, url: Option<&str>) -> (Kind, Option<String>) {
    let exe = normalize_exe(exe);
    let parts = title_parts(title);
    if let Some(&(_, kind, label)) = NATIVE_APPS.iter().find(|app| app.0 == exe) {
        return (refine_view(kind, &parts), Some(label.to_string()));
    }
    if BROWSERS.contains(&exe.as_str()) {
        return classify_browser(url, &parts);
    }
    let kind = if EDITORS.contains(&exe.as_str()) {
        Kind::Code
    } else if TERMINALS.contains(&exe.as_str()) {
        Kind::Terminal
    } else if DOCUMENTS.contains(&exe.as_str()) {
        Kind::Document
    } else {
        Kind::Other
    };
    (kind, None)
}

fn classify_browser(url: Option<&str>, parts: &[String]) -> (Kind, Option<String>) {
    if let Some((host, path)) = url.and_then(host_and_path) {
        let site = SITES.iter().find(|site| (host == site.0 || host.ends_with(&format!(".{}", site.0))) && path.starts_with(site.1));
        if let Some(&(_, _, kind, label)) = site {
            return (kind, Some(label.to_string()));
        }
    }
    let by_title = SITE_TITLES.iter().find(|site| parts.iter().any(|part| part_names(part, site.0)));
    match by_title {
        Some(&(_, kind, label)) => (refine_view(kind, parts), Some(label.to_string())),
        None => (Kind::Browser, None),
    }
}

// A random page about the "weather outlook" must not count, so only a whole title part names a site.
fn part_names(part: &str, label: &str) -> bool {
    part == label || part.starts_with(&format!("{label} and ")) || part.ends_with(&format!(" {label}"))
}

// Mail and chat apps also show a calendar or a meeting, which their titles lead with.
fn refine_view(kind: Kind, parts: &[String]) -> Kind {
    let Some(first) = parts.first() else { return kind };
    let has_calendar = parts.iter().any(|part| CALENDAR_WORDS.contains(&part.as_str()));
    if matches!(kind, Kind::Mail | Kind::Chat) && has_calendar {
        return Kind::Calendar;
    }
    if kind == Kind::Chat && MEETING_WORDS.iter().any(|word| first.starts_with(word)) {
        return Kind::Meeting;
    }
    kind
}

fn title_parts(title: &str) -> Vec<String> {
    [" – ", " — ", " | ", " • "].iter().fold(title.to_lowercase(), |text, mark| text.replace(mark, " - ")).split(" - ").map(|part| part.trim().to_string()).filter(|part| !part.is_empty()).collect()
}

fn host_and_path(url: &str) -> Option<(String, String)> {
    let rest = url.split_once("://").map_or(url, |split| split.1);
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest[..end].rsplit('@').next()?.split(':').next()?.to_lowercase();
    (!host.is_empty()).then(|| (host, rest[end..].to_lowercase()))
}

/// Drops the parts of a URL that can hold tokens: credentials, query and fragment.
pub fn clean_url(url: &str) -> String {
    let url = url.trim();
    let bare = url.find(['?', '#']).map_or(url, |at| &url[..at]);
    let (scheme, rest) = bare.split_once("://").map_or(("", bare), |(scheme, rest)| (scheme, rest));
    let end = rest.find('/').unwrap_or(rest.len());
    let host = rest[..end].rsplit('@').next().unwrap_or("");
    let path = &rest[end..];
    if scheme.is_empty() {
        format!("{host}{path}")
    } else {
        format!("{scheme}://{host}{path}")
    }
}

fn looks_like_url(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && !value.contains(char::is_whitespace) && (value.contains("://") || value.starts_with("edge:") || value.starts_with("chrome:") || value.split('/').next().is_some_and(|host| host.contains('.') && !host.starts_with('.') && !host.ends_with('.')))
}

/// Joins text in reading order without blanks, masked runs, repeats or repeated single short words, capped at 64 KB.
pub fn clean_text(lines: &[String]) -> String {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for line in lines {
        *counts.entry(line.trim()).or_default() += 1;
    }
    let mut seen = HashSet::new();
    let mut out = String::new();
    for line in lines.iter().map(|line| line.trim()) {
        let chrome = counts[line] > 1 && !line.contains(char::is_whitespace) && line.chars().count() <= SHORT_WORD_CHARS;
        if line.is_empty() || chrome || is_masked(line) || !seen.insert(line) {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    capped(out)
}

// A masked run shows a password's length, even in fields that don't report being passwords.
fn is_masked(line: &str) -> bool {
    line.chars().all(|c| matches!(c, '•' | '●' | '∙' | '·' | '*' | '⚫'))
}

// Long threads put their newest messages last, so most of what's kept comes from the end.
fn capped(text: String) -> String {
    if text.len() <= MAX_TEXT_BYTES {
        return text;
    }
    let mut head = (MAX_TEXT_BYTES - GAP.len()) / 4;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - (MAX_TEXT_BYTES - GAP.len() - head);
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}{GAP}{}", &text[..head], &text[tail..])
}

fn mix(hash: u64) -> u64 {
    let hash = (hash ^ (hash >> 33)).wrapping_mul(0xff51_afd7_ed55_8ccd);
    let hash = (hash ^ (hash >> 33)).wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^ (hash >> 33)
}

fn fnv(bytes: impl Iterator<Item = u8>, seed: u64) -> u64 {
    bytes.fold(seed, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3))
}

const FNV_SEED: u64 = 0xcbf2_9ce4_8422_2325;

/// A 64-bit SimHash over three-word shingles, stable across runs because it's stored.
pub fn simhash(text: &str) -> u64 {
    let words: Vec<String> = text.split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty()).map(str::to_lowercase).collect();
    if words.is_empty() {
        return 0;
    }
    let mut votes = [0i64; 64];
    for shingle in words.windows(words.len().min(3)) {
        let hash = mix(shingle.iter().fold(FNV_SEED, |hash, word| fnv(word.bytes().chain([0]), hash)));
        for (bit, vote) in votes.iter_mut().enumerate() {
            *vote += if (hash >> bit) & 1 == 1 { 1 } else { -1 };
        }
    }
    votes.iter().enumerate().fold(0u64, |hash, (bit, vote)| if *vote > 0 { hash | (1u64 << bit) } else { hash })
}

/// Whether text with this hash differs enough from what was last stored to keep.
pub fn is_new_text(last: Option<u64>, hash: u64) -> bool {
    last.is_none_or(|last| (last ^ hash).count_ones() > MIN_NEW_DISTANCE)
}

fn exact_hash(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// How long a small change waits before being kept: SimHash barely moves when a long thread gains a message.
pub fn small_change_wait(kind: Kind) -> i64 {
    if matches!(kind, Kind::Chat | Kind::Mail) { CONVERSATION_RECHECK_SECS } else { OTHER_RECHECK_SECS }
}

#[derive(Debug, Clone, Copy)]
struct Stored {
    similar: u64,
    exact: u64,
    at: i64,
}

#[derive(Debug, Default)]
struct Deduper {
    last: HashMap<isize, Stored>,
}

impl Deduper {
    fn is_new(&self, window: isize, text: &str, kind: Kind, now: i64) -> bool {
        let Some(last) = self.last.get(&window) else { return true };
        is_new_text(Some(last.similar), simhash(text)) || (last.exact != exact_hash(text) && now - last.at >= small_change_wait(kind))
    }

    fn remember(&mut self, window: isize, text: &str, now: i64) {
        if self.last.len() >= REMEMBERED_WINDOWS {
            self.last.clear();
        }
        self.last.insert(window, Stored { similar: simhash(text), exact: exact_hash(text), at: now });
    }
}

/// How much a walk may do, and how soon after the last one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tier {
    pub min_interval: Duration,
    pub max_nodes: usize,
}

/// Slows walks down for apps whose trees are slow to read.
#[derive(Clone, Debug, Default)]
pub struct Budget {
    samples: VecDeque<u64>,
    last_walk_ms: Option<u64>,
}

impl Budget {
    /// The limits that fit the recent average walk time.
    pub fn tier(&self) -> Tier {
        let (interval, max_nodes) = match self.average_ms() {
            ms if ms < 50 => (200, WALK_NODES),
            ms if ms < 150 => (2_000, 2000),
            ms if ms <= 250 => (5_000, 1000),
            _ => (60_000, SLOWEST_TIER_NODES),
        };
        Tier { min_interval: Duration::from_millis(interval), max_nodes }
    }

    /// Whether a walk may start now; `now_ms` is any steady clock.
    pub fn allows(&self, now_ms: u64) -> bool {
        self.last_walk_ms.is_none_or(|last| now_ms.saturating_sub(last) >= self.tier().min_interval.as_millis() as u64)
    }

    /// Notes a finished walk.
    pub fn record(&mut self, now_ms: u64, walk_ms: u64) {
        if self.samples.len() == AVERAGE_OVER {
            self.samples.pop_front();
        }
        self.samples.push_back(walk_ms);
        self.last_walk_ms = Some(now_ms);
    }

    fn average_ms(&self) -> u64 {
        self.samples.iter().sum::<u64>() / (self.samples.len().max(1) as u64)
    }
}

/// What a session is about.
#[derive(Clone, Debug, PartialEq)]
pub struct Activity {
    pub exe: String,
    pub title: String,
    pub url: Option<String>,
    pub kind: Kind,
    pub source: Option<String>,
}

/// A foreground session so far; times are in seconds.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub start: i64,
    pub end: i64,
    pub activity: Activity,
}

/// The gap in seconds that still counts as the same session: the larger of 1.5 polls or one poll plus a second.
pub fn pulsetime(poll: Duration) -> i64 {
    let poll_ms = poll.as_millis() as i64;
    ((poll_ms * 3 / 2).max(poll_ms + 1000) + 999) / 1000
}

/// Turns foreground samples into sessions, merging identical ones close together.
#[derive(Clone, Debug)]
pub struct Sessionizer {
    pulse: i64,
    current: Option<Span>,
}

impl Sessionizer {
    /// A sessionizer that merges samples up to `pulse` seconds apart.
    pub fn new(pulse: i64) -> Self {
        Self { pulse, current: None }
    }

    /// Takes a sample and returns the spans to save: a closed one, if any, then the current one.
    pub fn beat(&mut self, at: i64, activity: Activity) -> Vec<Span> {
        let mut changed = Vec::new();
        if let Some(current) = &mut self.current {
            let close = at - current.end <= self.pulse;
            if close {
                current.end = current.end.max(at);
            }
            if close && current.activity.exe == activity.exe && current.activity.title == activity.title {
                return vec![current.clone()];
            }
            if close {
                changed.push(current.clone());
            }
        }
        let span = Span { start: at, end: at, activity };
        self.current = Some(span.clone());
        changed.push(span);
        changed
    }

    /// What's in front right now, if anything is.
    pub fn current(&self) -> Option<&Activity> {
        self.current.as_ref().map(|span| &span.activity)
    }

    /// Ends the session at its last sample, so time away isn't counted.
    pub fn end(&mut self) {
        self.current = None;
    }
}

/// Why a window is never read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    OwnWindow,
    ExcludedExe,
    NotChosen,
    PrivateBrowsing,
    CaptureProtected,
    Cloaked,
}

/// The facts about a window that decide whether it may be read.
#[derive(Clone, Copy, Debug)]
pub struct WindowFacts<'a> {
    pub pid: u32,
    pub exe: &'a str,
    pub title: &'a str,
    /// `None` when the affinity couldn't be read, which counts as protected.
    pub affinity: Option<u32>,
    pub cloaked: bool,
}

// A sidebar or sign-in panel counts as a document long before the page does, so the tree must stop growing too.
#[derive(Debug, Default)]
struct WakeWatch {
    nodes: usize,
    same: u32,
}

impl WakeWatch {
    fn is_done(&mut self, has_document: bool, nodes: usize) -> bool {
        self.same = if nodes == self.nodes { self.same + 1 } else { 0 };
        self.nodes = nodes;
        has_document && self.same >= WAKE_STEADY_READS
    }
}

/// Decides which windows stay unread.
#[derive(Clone, Debug)]
pub struct Redactor {
    excluded: Vec<String>,
    titles: Vec<String>,
    off: Vec<String>,
    only: Option<Vec<String>>,
    own_pid: u32,
}

impl Redactor {
    /// A redactor for this exclusion list, treating `own_pid` as Olympus itself.
    pub fn new(excluded: &[String], own_pid: u32) -> Self {
        Self { excluded: excluded.iter().map(|exe| normalize_exe(exe)).collect(), titles: Vec::new(), off: Vec::new(), only: None, own_pid }
    }

    /// Also leaves alone these exact apps, and with `only`, every app not on that list.
    pub fn choosing_apps(mut self, off: &[String], only: Option<&[String]>) -> Self {
        self.off = off.iter().map(|exe| normalize_exe(exe)).collect();
        self.only = only.map(|list| list.iter().map(|exe| normalize_exe(exe)).collect());
        self
    }

    /// Also leaves alone windows with one of these words in their title.
    pub fn hiding_titles(mut self, words: &[String]) -> Self {
        self.titles = words.iter().map(|word| word.trim().to_lowercase()).filter(|word| word.chars().count() >= SHORTEST_EXCLUDED_WORD).collect();
        self
    }

    /// The reason to leave a window alone, if there is one.
    pub fn decide(&self, window: &WindowFacts) -> Option<Skip> {
        let exe = normalize_exe(window.exe);
        let title = window.title.to_lowercase();
        if window.pid == self.own_pid || exe == "olympus" || exe.starts_with("olympus-") {
            Some(Skip::OwnWindow)
        } else if self.excludes(&exe) || self.titles.iter().any(|word| title.contains(word.as_str())) {
            Some(Skip::ExcludedExe)
        } else if self.not_chosen(&exe) {
            Some(Skip::NotChosen)
        } else if window.affinity.is_none_or(|affinity| affinity != WDA_NONE) {
            Some(Skip::CaptureProtected)
        } else if window.cloaked {
            Some(Skip::Cloaked)
        } else if PRIVATE_TITLES.iter().chain(PASSWORD_VAULT_TITLES.iter()).chain(BANKING_TITLES.iter()).any(|word| title.contains(word)) {
            Some(Skip::PrivateBrowsing)
        } else {
            None
        }
    }

    // A prefix also catches an app's relatives, such as keepassxc.
    fn excludes(&self, exe: &str) -> bool {
        self.excluded.iter().any(|name| !name.is_empty() && exe.starts_with(name.as_str())) || self.off.iter().any(|name| name == exe)
    }

    fn not_chosen(&self, exe: &str) -> bool {
        self.only.as_ref().is_some_and(|only| !only.iter().any(|name| name == exe))
    }
}

/// The largest walk allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub depth: usize,
    pub nodes: usize,
    pub budget: Duration,
    /// Fetch the whole tree in one round trip, about 8 times faster; only the UIA timeout can cut it short.
    pub batched: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Self { depth: WALK_DEPTH, nodes: WALK_NODES, budget: WALK_BUDGET, batched: true }
    }
}

/// What one walk over a window found.
#[derive(Clone, Debug, Default)]
pub struct Walk {
    pub lines: Vec<String>,
    pub url: Option<String>,
    pub has_document: bool,
    pub nodes: usize,
    pub elapsed: Duration,
    pub truncated: bool,
}

impl Walk {
    /// The walk's text, ready to store.
    pub fn text(&self) -> String {
        clean_text(&self.lines)
    }
}

#[derive(Clone, Debug, Default)]
struct Node {
    control: i32,
    name: String,
    value: Option<String>,
    class: String,
    automation_id: String,
    is_password: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct Scope {
    in_document: bool,
    in_toolbar: bool,
}

#[derive(Debug, Default)]
struct Collector {
    document: Vec<String>,
    chrome: Vec<String>,
    url: Option<String>,
    page_url: Option<String>,
    has_document: bool,
}

impl Collector {
    // Returns the scope for this node's children, or None to skip them.
    fn visit(&mut self, node: &Node, scope: Scope) -> Option<Scope> {
        if node.is_password {
            return None;
        }
        let control = UIA_CONTROLTYPE_ID(node.control);
        if [UIA_ScrollBarControlTypeId, UIA_ThumbControlTypeId, UIA_TitleBarControlTypeId, UIA_MenuBarControlTypeId, UIA_ProgressBarControlTypeId, UIA_SliderControlTypeId, UIA_SpinnerControlTypeId, UIA_SeparatorControlTypeId].contains(&control) {
            return None;
        }
        let is_document = control == UIA_DocumentControlTypeId;
        let next = Scope { in_document: scope.in_document || is_document, in_toolbar: scope.in_toolbar || control == UIA_ToolBarControlTypeId };
        self.has_document |= is_document;
        if is_document && self.page_url.is_none() {
            self.page_url = node.value.as_deref().filter(|value| looks_like_url(value)).map(clean_url);
        }
        if !next.in_document && self.take_address_bar(node, control, next) {
            return Some(next);
        }
        if next.in_toolbar && !next.in_document {
            return Some(next);
        }
        let lines = if next.in_document { &mut self.document } else { &mut self.chrome };
        if ![UIA_WindowControlTypeId, UIA_PaneControlTypeId, UIA_DocumentControlTypeId].contains(&control) {
            lines.push(node.name.clone());
        }
        let takes_value = [UIA_EditControlTypeId, UIA_ComboBoxControlTypeId, UIA_DocumentControlTypeId].contains(&control);
        if takes_value && !names_a_secret(node) {
            if let Some(value) = node.value.as_ref().filter(|value| !(is_document && looks_like_url(value))) {
                lines.push(value.clone());
            }
        }
        Some(next)
    }

    fn take_address_bar(&mut self, node: &Node, control: UIA_CONTROLTYPE_ID, scope: Scope) -> bool {
        if control != UIA_EditControlTypeId {
            return false;
        }
        let named = node.class.to_lowercase().contains("omnibox") || node.automation_id == "urlbar-input";
        let value = node.value.as_deref().filter(|value| looks_like_url(value));
        if let (true, None, Some(value)) = (named || scope.in_toolbar, &self.url, value) {
            self.url = Some(clean_url(value));
        }
        named
    }

    fn finish(self) -> (Vec<String>, Option<String>, bool) {
        let lines = if self.document.is_empty() { self.chrome } else { self.document };
        (lines, self.page_url.or(self.url), self.has_document)
    }
}

fn names_a_secret(node: &Node) -> bool {
    let (name, id) = (node.name.to_lowercase(), node.automation_id.to_lowercase());
    SECRET_FIELDS.iter().any(|word| name.contains(word) || id.contains(word))
}

struct WalkState {
    collector: Collector,
    limits: Limits,
    started: Instant,
    nodes: usize,
    stopped: bool,
    truncated: bool,
}

impl WalkState {
    // Reads one node, returning the scope for its children unless they should be left out.
    fn enter(&mut self, element: &IUIAutomationElement, depth: usize, scope: Scope) -> Option<Scope> {
        if self.nodes >= self.limits.nodes || self.started.elapsed() >= self.limits.budget {
            self.truncated = true;
            self.stopped = true;
            return None;
        }
        self.nodes += 1;
        let scope = self.collector.visit(&read_node(element), scope)?;
        if depth + 1 >= self.limits.depth {
            self.truncated = true;
            return None;
        }
        Some(scope)
    }
}

struct ComApartment(bool);

impl ComApartment {
    fn enter() -> Self {
        // SAFETY: called on the thread that uses COM, and undone on that thread by Drop.
        Self(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok())
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
    }
}

/// Walks windows through UI Automation. Stays on the thread that made it, which it makes a single-threaded apartment.
pub struct Reader {
    automation: IUIAutomation,
    cache: IUIAutomationCacheRequest,
    subtree: IUIAutomationCacheRequest,
    walker: IUIAutomationTreeWalker,
    // Declared last so COM stays alive until the interfaces above are released.
    _apartment: ComApartment,
}

impl Reader {
    /// Sets up UI Automation on this thread.
    pub fn new() -> Result<Self> {
        let apartment = ComApartment::enter();
        // SAFETY: COM is initialised on this thread, and each call follows the interface's documented use.
        unsafe {
            let automation: IUIAutomation = match CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) {
                Ok(automation) => automation,
                Err(_) => CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).context("creating UI Automation")?,
            };
            // Without these a hung app blocks each call for 20 seconds or more.
            if let Ok(timeouts) = windows::core::Interface::cast::<IUIAutomation2>(&automation) {
                let _ = timeouts.SetConnectionTimeout(UIA_TIMEOUT_MS);
                let _ = timeouts.SetTransactionTimeout(UIA_TIMEOUT_MS);
            }
            let cache = automation.CreateCacheRequest()?;
            let subtree = automation.CreateCacheRequest()?;
            let properties: [UIA_PROPERTY_ID; 6] = [UIA_NamePropertyId, UIA_ControlTypePropertyId, UIA_IsPasswordPropertyId, UIA_ValueValuePropertyId, UIA_ClassNamePropertyId, UIA_AutomationIdPropertyId];
            for property in properties {
                cache.AddProperty(property)?;
                subtree.AddProperty(property)?;
            }
            cache.SetAutomationElementMode(AutomationElementMode_Full)?;
            subtree.SetAutomationElementMode(AutomationElementMode_None)?;
            subtree.SetTreeScope(TreeScope_Subtree)?;
            subtree.SetTreeFilter(&automation.ControlViewCondition()?)?;
            let walker = automation.ControlViewWalker()?;
            Ok(Self { automation, cache, subtree, walker, _apartment: apartment })
        }
    }

    /// One walk over a window, stopping at the limits.
    pub fn read(&self, window: HWND, limits: Limits) -> Result<Walk> {
        let started = Instant::now();
        let request = if limits.batched { &self.subtree } else { &self.cache };
        // SAFETY: a stale handle only makes this return an error.
        let root = unsafe { self.automation.ElementFromHandleBuildCache(window, request) }?;
        let mut state = WalkState { collector: Collector::default(), limits, started, nodes: 0, stopped: false, truncated: false };
        if limits.batched {
            visit_cached(&root, 0, Scope::default(), &mut state);
        } else {
            self.visit(&root, 0, Scope::default(), &mut state);
        }
        let (lines, url, has_document) = state.collector.finish();
        Ok(Walk { lines, url, has_document, nodes: state.nodes, elapsed: started.elapsed(), truncated: state.truncated })
    }

    /// Walks, and for a Chromium window keeps trying for up to two seconds while its page hasn't appeared yet.
    pub fn read_waking(&self, window: HWND, limits: Limits) -> Result<Walk> {
        let started = Instant::now();
        let mut settled = WakeWatch::default();
        loop {
            let walk = self.read(window, limits)?;
            if settled.is_done(walk.has_document, walk.nodes) || started.elapsed() >= WAKE_FOR {
                return Ok(walk);
            }
            std::thread::sleep(WAKE_RETRY);
        }
    }

    fn visit(&self, element: &IUIAutomationElement, depth: usize, scope: Scope, state: &mut WalkState) {
        let Some(scope) = state.enter(element, depth, scope) else { return };
        // SAFETY: the element and cache request are live COM objects used on their own thread.
        let mut child = self.advance(unsafe { self.walker.GetFirstChildElementBuildCache(element, &self.cache) }, state);
        while let Some(current) = child {
            self.visit(&current, depth + 1, scope, state);
            if state.stopped {
                return;
            }
            // SAFETY: as above.
            child = self.advance(unsafe { self.walker.GetNextSiblingElementBuildCache(&current, &self.cache) }, state);
        }
    }

    // The walker reports "no more elements" as an error whose code is success.
    fn advance(&self, step: windows::core::Result<IUIAutomationElement>, state: &mut WalkState) -> Option<IUIAutomationElement> {
        match step {
            Ok(element) => Some(element),
            Err(error) => {
                if error.code().0 as u32 == UIA_E_TIMEOUT {
                    state.truncated = true;
                    state.stopped = true;
                }
                None
            }
        }
    }
}

fn visit_cached(element: &IUIAutomationElement, depth: usize, scope: Scope, state: &mut WalkState) {
    let Some(scope) = state.enter(element, depth, scope) else { return };
    // SAFETY: these only read the tree already fetched; a node with no children reports an error.
    unsafe {
        let Ok(children) = element.GetCachedChildren() else { return };
        for index in 0..children.Length().unwrap_or(0) {
            if state.stopped {
                return;
            }
            if let Ok(child) = children.GetElement(index) {
                visit_cached(&child, depth + 1, scope, state);
            }
        }
    }
}

fn read_node(element: &IUIAutomationElement) -> Node {
    // SAFETY: only cached properties are read, which never call into the other app.
    unsafe {
        // Unknown counts as a password, so a failed read can't expose one.
        let is_password = element.CachedIsPassword().map_or(true, BOOL::as_bool);
        if is_password {
            return Node { is_password, ..Node::default() };
        }
        Node {
            control: element.CachedControlType().map_or(0, |control| control.0),
            name: element.CachedName().map_or_else(|_| String::new(), |name| name.to_string()),
            value: cached_text(element, UIA_ValueValuePropertyId),
            class: element.CachedClassName().map_or_else(|_| String::new(), |class| class.to_string()),
            automation_id: element.CachedAutomationId().map_or_else(|_| String::new(), |id| id.to_string()),
            is_password,
        }
    }
}

unsafe fn cached_text(element: &IUIAutomationElement, property: UIA_PROPERTY_ID) -> Option<String> {
    // SAFETY: the caller holds a live element, and the VARIANT owns its contents.
    let variant = unsafe { element.GetCachedPropertyValue(property) }.ok()?;
    let text = BSTR::try_from(&variant).ok()?.to_string();
    (!text.is_empty()).then_some(text)
}

/// Top-level windows that a person could be looking at; only those of `only_pids` unless it's empty.
pub fn visible_windows(only_pids: &[u32]) -> Vec<HWND> {
    let mut found: Vec<HWND> = Vec::new();
    // SAFETY: the callback only runs during this call, so the pointer to `found` stays valid.
    let _ = unsafe { EnumWindows(Some(collect_window), LPARAM(&raw mut found as isize)) };
    found.retain(|&window| {
        // SAFETY: a stale handle only makes these calls report nothing.
        let visible = unsafe { IsWindowVisible(window) }.as_bool() && unsafe { GetWindow(window, GW_OWNER) }.is_err() && unsafe { GetWindowLongW(window, GWL_EXSTYLE) } as u32 & WS_EX_TOOLWINDOW.0 == 0;
        visible && (only_pids.is_empty() || only_pids.contains(&window_pid(window))) && !SHELL_CLASSES.contains(&class_name(window).as_str())
    });
    found
}

unsafe extern "system" fn collect_window(window: HWND, list: LPARAM) -> BOOL {
    // SAFETY: `list` is the live Vec passed by `visible_windows`, on this same thread.
    unsafe { (*(list.0 as *mut Vec<HWND>)).push(window) };
    BOOL(1)
}

/// The process that owns a window.
pub fn window_pid(window: HWND) -> u32 {
    let mut pid = 0;
    // SAFETY: `pid` outlives the call, and a stale handle just leaves it at zero.
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    pid
}

fn class_name(window: HWND) -> String {
    let mut buffer = [0u16; 256];
    // SAFETY: the buffer is valid for its length.
    let length = unsafe { GetClassNameW(window, &mut buffer) };
    String::from_utf16_lossy(&buffer[..length.max(0) as usize])
}

fn window_title(window: HWND) -> String {
    let mut buffer = [0u16; 512];
    // SAFETY: the buffer is valid for its length.
    let length = unsafe { GetWindowTextW(window, &mut buffer) };
    String::from_utf16_lossy(&buffer[..length.max(0) as usize])
}

fn exe_of_pid(pid: u32) -> Option<String> {
    path_of_pid(pid).map(|path| normalize_exe(&path))
}

fn path_of_pid(pid: u32) -> Option<String> {
    // SAFETY: the handle is checked, used here only, and closed; the buffer is valid for `size` characters.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let named = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buffer.as_mut_ptr()), &mut size);
        let _ = CloseHandle(process);
        named.ok()?;
        Some(String::from_utf16_lossy(&buffer[..size as usize]))
    }
}

// A UWP window belongs to ApplicationFrameHost; the app itself owns a child window with another pid.
fn app_pid(window: HWND, pid: u32) -> u32 {
    if class_name(window) != UWP_FRAME_CLASS {
        return pid;
    }
    let mut search = (pid, pid);
    // SAFETY: the callback only runs during this call, so the pointer to `search` stays valid.
    let _ = unsafe { EnumChildWindows(Some(window), Some(find_app_child), LPARAM(&raw mut search as isize)) };
    search.1
}

unsafe extern "system" fn find_app_child(child: HWND, search: LPARAM) -> BOOL {
    // SAFETY: `search` is the live tuple passed by `app_pid`, on this same thread.
    let search = unsafe { &mut *(search.0 as *mut (u32, u32)) };
    let pid = window_pid(child);
    if pid != search.0 {
        search.1 = pid;
        return BOOL(0);
    }
    BOOL(1)
}

fn display_affinity(window: HWND) -> Option<u32> {
    let mut affinity = 0;
    // SAFETY: `affinity` outlives the call.
    unsafe { GetWindowDisplayAffinity(window, &mut affinity) }.ok()?;
    Some(affinity)
}

fn is_cloaked(window: HWND) -> bool {
    let mut cloaked = 0u32;
    // SAFETY: the pointer and size describe `cloaked`.
    let read = unsafe { DwmGetWindowAttribute(window, DWMWA_CLOAKED, (&raw mut cloaked).cast(), size_of::<u32>() as u32) };
    read.is_ok() && cloaked != 0
}

fn idle_for() -> Duration {
    let mut info = LASTINPUTINFO { cbSize: size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    // SAFETY: `info` is sized and outlives the call.
    if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
        return Duration::ZERO;
    }
    // SAFETY: takes no arguments. Both clocks wrap together after 49 days, so the difference holds.
    Duration::from_millis(u64::from(unsafe { GetTickCount() }.wrapping_sub(info.dwTime)))
}

/// A window's pixels as top-down BGRA, drawn to memory; never touches the cursor or the screen.
pub fn window_pixels(window: HWND) -> Result<(i32, i32, Vec<u8>)> {
    // SAFETY: every GDI object is created, used and released here; a hung window is refused first because PrintWindow would wait on it.
    unsafe {
        if IsHungAppWindow(window).as_bool() {
            return Err(anyhow!("window isn't responding"));
        }
        let mut rect = RECT::default();
        GetWindowRect(window, &mut rect)?;
        let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
        if width <= 0 || height <= 0 || width > LARGEST_CAPTURE || height > LARGEST_CAPTURE {
            return Err(anyhow!("window size {width}x{height} can't be captured"));
        }
        let screen = GetDC(None);
        let memory = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let previous = SelectObject(memory, bitmap.into());
        if !PrintWindow(window, memory, FULL_CONTENT).as_bool() {
            let source = GetWindowDC(Some(window));
            let _ = BitBlt(memory, 0, 0, width, height, Some(source), 0, 0, SRCCOPY);
            ReleaseDC(Some(window), source);
        }
        SelectObject(memory, previous);
        let header = BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: width, biHeight: -height, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default() };
        let mut info = BITMAPINFO { bmiHeader: header, ..Default::default() };
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        let copied = GetDIBits(memory, bitmap, 0, height as u32, Some(pixels.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
        if copied == 0 {
            return Err(anyhow!("couldn't copy the window's pixels"));
        }
        Ok((width, height, pixels))
    }
}

/// Reads text from top-down BGRA pixels with the Windows OCR engine.
pub fn ocr_bgra(width: i32, height: i32, pixels: &[u8]) -> Result<String> {
    // Windows draws with alpha 0 almost everywhere, and the OCR engine reads that as blank.
    let opaque: Vec<u8> = pixels.chunks_exact(4).flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255]).collect();
    let writer = DataWriter::new()?;
    writer.WriteBytes(&opaque)?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(&writer.DetachBuffer()?, BitmapPixelFormat::Bgra8, width, height)?;
    let engine = OcrEngine::TryCreateFromUserProfileLanguages().context("no OCR language installed")?;
    Ok(engine.RecognizeAsync(&bitmap)?.join()?.Text()?.to_string())
}

// One capture at most every 10s, and OCR only when the pixels really changed.
#[derive(Debug, Default)]
struct PixelGate {
    last_try_ms: Option<u64>,
    last_hash: Option<u64>,
}

impl PixelGate {
    fn ready(&self, now_ms: u64) -> bool {
        self.last_try_ms.is_none_or(|last| now_ms.saturating_sub(last) >= OCR_EVERY_MS)
    }

    fn changed(&mut self, now_ms: u64, pixels: &[u8]) -> bool {
        self.last_try_ms = Some(now_ms);
        let hash = fnv(pixels.iter().step_by(4).copied(), FNV_SEED);
        let changed = self.last_hash != Some(hash);
        self.last_hash = Some(hash);
        changed
    }
}

/// Tracks which apps are running and reports only starts and exits.
#[derive(Debug, Default)]
pub struct ProcessDiff {
    running: HashSet<String>,
}

impl ProcessDiff {
    /// Starts from what was already known to be running.
    pub fn new(running: impl IntoIterator<Item = String>) -> Self {
        Self { running: running.into_iter().collect() }
    }

    /// Takes the apps running now and returns what changed, sorted by name.
    pub fn update(&mut self, now: HashSet<String>) -> Vec<(String, ProcessChange)> {
        let mut changes: Vec<(String, ProcessChange)> = now.difference(&self.running).map(|exe| (exe.clone(), ProcessChange::Start)).chain(self.running.difference(&now).map(|exe| (exe.clone(), ProcessChange::Exit))).collect();
        changes.sort_by(|a, b| a.0.cmp(&b.0));
        self.running = now;
        changes
    }
}

fn running_processes() -> Vec<(u32, String)> {
    let mut found = Vec::new();
    // SAFETY: the snapshot handle is checked, used only here and closed, and the entry is sized as the API requires.
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return found };
        let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            let length = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            found.push((entry.th32ProcessID, normalize_exe(&String::from_utf16_lossy(&entry.szExeFile[..length]))));
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    found
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Due {
    poll: bool,
    sweep: bool,
    processes: bool,
    idle: bool,
}

// Decides what to do on each wake-up; times are milliseconds on a steady clock.
#[derive(Clone, Debug)]
struct Pacer {
    poll_ms: u64,
    sweep_ms: u64,
    idle_sweep_ms: u64,
    processes_ms: u64,
    idle_after_ms: u64,
    next_poll: u64,
    next_sweep: u64,
    next_processes: u64,
}

impl Pacer {
    fn new(settings: &Settings) -> Self {
        let sweep_ms = settings.sweep_every.as_millis() as u64;
        Self { poll_ms: settings.poll.as_millis() as u64, sweep_ms, idle_sweep_ms: settings.idle_sweep_every.as_millis() as u64, processes_ms: settings.processes_every.as_millis() as u64, idle_after_ms: settings.idle_after.as_millis() as u64, next_poll: 0, next_sweep: sweep_ms, next_processes: 0 }
    }

    fn due(&mut self, now: u64, idle_ms: u64, foreground_changed: bool) -> Due {
        let idle = idle_ms >= self.idle_after_ms;
        let poll_time = now >= self.next_poll;
        let sweep_time = now >= self.next_sweep;
        let processes = now >= self.next_processes;
        if poll_time {
            self.next_poll = now + self.poll_ms;
        }
        if sweep_time {
            self.next_sweep = now + if idle { self.idle_sweep_ms } else { self.sweep_ms };
        }
        if processes {
            self.next_processes = now + self.processes_ms;
        }
        Due { poll: !idle && (poll_time || foreground_changed), sweep: sweep_time, processes, idle }
    }

    fn wait_ms(&self, now: u64) -> u32 {
        self.next_poll.min(self.next_sweep).min(self.next_processes).saturating_sub(now).clamp(1, u64::from(u32::MAX)) as u32
    }
}

#[derive(Default)]
struct Shared {
    paused: AtomicBool,
    stop: AtomicBool,
    status: Mutex<Status>,
}

impl Shared {
    fn update(&self, change: impl FnOnce(&mut Status)) {
        change(&mut self.status.lock().unwrap_or_else(PoisonError::into_inner));
    }
}

thread_local! {
    static FOREGROUND_CHANGED: Cell<bool> = const { Cell::new(false) };
}

unsafe extern "system" fn on_foreground(_hook: HWINEVENTHOOK, _event: u32, _window: HWND, _object: i32, _child: i32, _thread: u32, _time: u32) {
    let _ = FOREGROUND_CHANGED.try_with(|changed| changed.set(true));
}

struct ForegroundHook(HWINEVENTHOOK);

impl ForegroundHook {
    fn install() -> Self {
        // SAFETY: the callback is out of context, so it runs on this thread while it pumps messages, and Drop removes it.
        Self(unsafe { SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(on_foreground), 0, 0, WINEVENT_OUTOFCONTEXT) })
    }
}

impl Drop for ForegroundHook {
    fn drop(&mut self) {
        // SAFETY: the hook came from SetWinEventHook on this thread, and a failed install holds a null handle that's refused harmlessly.
        let _ = unsafe { UnhookWinEvent(self.0) };
    }
}

struct WindowInfo {
    window: HWND,
    exe: String,
    title: String,
    class: String,
}

struct Runner {
    settings: Settings,
    shared: Arc<Shared>,
    store: Store,
    reader: Reader,
    redactor: Redactor,
    started: Instant,
    pacer: Pacer,
    budgets: HashMap<String, Budget>,
    deduper: Deduper,
    sessions: Sessionizer,
    gate: PixelGate,
    processes: ProcessDiff,
    woken: HashSet<isize>,
    sweep_from: usize,
    last_prune: Option<Instant>,
    files: Option<std::sync::mpsc::Receiver<crate::files::Change>>,
    // Saving while an excluded app or a bank is in front leaves no trace, not even a file name.
    front_is_private: bool,
}

impl Runner {
    fn new(settings: Settings, store_path: &std::path::Path, shared: Arc<Shared>) -> Result<Self> {
        let store = Store::open(store_path)?;
        let reader = Reader::new()?;
        // SAFETY: takes no arguments.
        let own_pid = unsafe { GetCurrentProcessId() };
        let redactor = Redactor::new(&settings.excluded_exes, own_pid).hiding_titles(&settings.excluded_titles).choosing_apps(&settings.off_exes, settings.only_exes.as_deref());
        let (files_tx, files) = std::sync::mpsc::channel();
        for folder in &settings.watch {
            if let Err(error) = crate::files::watch(folder.clone(), files_tx.clone()) {
                shared.update(|status| status.last_error = Some(format!("watching files: {error}")));
            }
        }
        let processes = ProcessDiff::new(store.running_apps()?);
        let settings_watch_is_empty = settings.watch.is_empty();
        Ok(Self {
            pacer: Pacer::new(&settings),
            sessions: Sessionizer::new(pulsetime(settings.poll)),
            settings,
            shared,
            store,
            reader,
            redactor,
            started: Instant::now(),
            budgets: HashMap::new(),
            deduper: Deduper::default(),
            gate: PixelGate::default(),
            processes,
            woken: HashSet::new(),
            sweep_from: 0,
            last_prune: None,
            files: (!settings_watch_is_empty).then_some(files),
            front_is_private: false,
        })
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn fail(&self, context: &str, error: &anyhow::Error) {
        let message = format!("{context}: {error}");
        self.shared.update(|status| status.last_error = Some(message));
    }

    fn pump(&self) -> bool {
        let mut message = MSG::default();
        // SAFETY: `message` outlives each call, and only this thread's own queue is read.
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                if message.message == WM_QUIT {
                    return false;
                }
                DispatchMessageW(&message);
            }
        }
        !self.shared.stop.load(Ordering::SeqCst)
    }

    fn run(&mut self) {
        let _hook = ForegroundHook::install();
        loop {
            let wait = self.pacer.wait_ms(self.now_ms());
            // SAFETY: waits on no handles, only on this thread's queue, until the timeout.
            let _ = unsafe { MsgWaitForMultipleObjectsEx(None, wait, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
            if !self.pump() {
                return;
            }
            let changed = FOREGROUND_CHANGED.try_with(|changed| changed.replace(false)).unwrap_or(false);
            let now = self.now_ms();
            let due = self.pacer.due(now, idle_for().as_millis() as u64, changed);
            if self.shared.paused.load(Ordering::SeqCst) {
                self.sessions.end();
                if let Some(files) = &self.files {
                    files.try_iter().for_each(drop);
                }
                continue;
            }
            // A bug in one round must cost that round, not the host app.
            if catch_unwind(AssertUnwindSafe(|| self.tick(due))).is_err() {
                self.fail("observer", &anyhow!("a round panicked"));
            }
        }
    }

    fn tick(&mut self, due: Due) {
        if due.idle {
            self.sessions.end();
        }
        if due.poll {
            if let Err(error) = self.poll_foreground() {
                self.fail("foreground", &error);
            }
        }
        if due.sweep && self.settings.sweep {
            self.sweep();
        }
        if let Err(error) = self.record_files() {
            self.fail("files", &error);
        }
        if due.processes && self.settings.read_processes {
            if let Err(error) = self.track_processes() {
                self.fail("processes", &error);
            }
        }
        if self.last_prune.is_none_or(|last| last.elapsed() >= PRUNE_EVERY) {
            self.last_prune = Some(Instant::now());
            if let Err(error) = self.store.prune(self.settings.raw_days) {
                self.fail("prune", &error);
            }
        }
    }

    fn record_files(&mut self) -> Result<()> {
        let Some(files) = &self.files else { return Ok(()) };
        let at = chrono::Utc::now().timestamp();
        let app = self.sessions.current().map(|activity| activity.source.clone().unwrap_or_else(|| activity.exe.clone()));
        for change in files.try_iter() {
            if !self.front_is_private && !change.path.is_dir() {
                self.store.add_file_event(at, &change.path.to_string_lossy(), change.action.as_str(), app.as_deref())?;
            }
        }
        Ok(())
    }

    fn is_private(&self, window: HWND) -> bool {
        let host_pid = window_pid(window);
        if !self.pid_allowed(host_pid) {
            return false;
        }
        let pid = app_pid(window, host_pid);
        let Some(exe) = exe_of_pid(pid) else { return false };
        let title = window_title(window);
        let facts = WindowFacts { pid, exe: &exe, title: &title, affinity: display_affinity(window), cloaked: is_cloaked(window) };
        matches!(self.redactor.decide(&facts), Some(Skip::ExcludedExe | Skip::PrivateBrowsing))
    }

    // None for anything that may not be read, checked before the window's title is ever looked at.
    fn describe(&self, window: HWND) -> Option<WindowInfo> {
        let host_pid = window_pid(window);
        if !self.pid_allowed(host_pid) {
            return None;
        }
        let pid = app_pid(window, host_pid);
        let exe = exe_of_pid(pid)?;
        let title = window_title(window);
        let facts = WindowFacts { pid, exe: &exe, title: &title, affinity: display_affinity(window), cloaked: is_cloaked(window) };
        if self.redactor.decide(&facts).is_some() || title.trim().is_empty() {
            return None;
        }
        Some(WindowInfo { window, exe, title, class: class_name(window) })
    }

    fn pid_allowed(&self, pid: u32) -> bool {
        self.settings.only_pids.is_empty() || self.settings.only_pids.contains(&pid)
    }

    fn poll_foreground(&mut self) -> Result<()> {
        // SAFETY: takes no arguments; a null result is handled below.
        let window = unsafe { GetForegroundWindow() };
        self.observe_foreground(window)
    }

    fn observe_foreground(&mut self, window: HWND) -> Result<()> {
        let info = if window.is_invalid() { None } else { self.describe(window) };
        let Some(info) = info else {
            self.front_is_private = !window.is_invalid() && self.is_private(window);
            self.sessions.end();
            return Ok(());
        };
        self.front_is_private = false;
        let at = chrono::Utc::now().timestamp();
        let (kind, source) = classify(&info.exe, &info.title, None);
        let activity = Activity { exe: info.exe.clone(), title: info.title.clone(), url: None, kind, source };
        for span in self.sessions.beat(at, activity) {
            self.store.session_heartbeat(&span)?;
        }
        self.observe(&info, true, WALK_NODES)
    }

    fn sweep(&mut self) {
        // SAFETY: takes no arguments.
        let foreground = unsafe { GetForegroundWindow() };
        let windows = visible_windows(&self.settings.only_pids);
        let started = Instant::now();
        let count = windows.len();
        // Rotating the start gives windows at the end of the list a turn when time runs out.
        for offset in 0..count {
            let window = windows[(self.sweep_from + offset) % count];
            if started.elapsed() >= SWEEP_TOTAL || self.shared.stop.load(Ordering::SeqCst) {
                self.sweep_from = (self.sweep_from + offset) % count;
                return;
            }
            if window == foreground {
                continue;
            }
            if let Some(info) = self.describe(window) {
                if let Err(error) = self.observe(&info, false, SWEEP_NODES) {
                    self.fail(&info.exe, &error);
                }
            }
        }
    }

    fn observe(&mut self, info: &WindowInfo, foreground: bool, most_nodes: usize) -> Result<()> {
        if !self.settings.read_text || self.settings.time_only_exes.contains(&info.exe) {
            return Ok(());
        }
        let now = self.now_ms();
        let budget = self.budgets.entry(info.exe.clone()).or_default();
        if !budget.allows(now) {
            return Ok(());
        }
        let tier = budget.tier();
        // An app too slow for a batch gets the walker, which can stop itself at the time budget.
        let limits = Limits { nodes: tier.max_nodes.min(most_nodes), batched: tier.max_nodes > SLOWEST_TIER_NODES, ..Limits::default() };
        let key = info.window.0 as isize;
        let chromium = info.class == CHROMIUM_CLASS;
        let first_look = chromium && self.remember_woken(key);
        // Chromium drops its page tree between sweeps, and rebuilding takes longer than one read.
        let wake = chromium && (first_look || !foreground);
        let begun = Instant::now();
        let walk = if wake { self.reader.read_waking(info.window, limits) } else { self.reader.read(info.window, limits) };
        let walk_ms = walk.as_ref().map_or_else(|_| begun.elapsed(), |walk| walk.elapsed).as_millis() as u64;
        let finished = self.now_ms();
        self.budgets.entry(info.exe.clone()).or_default().record(finished, walk_ms);
        self.shared.update(|status| {
            status.walks += 1;
            status.last_walk_ms = walk_ms;
        });
        let walk = walk.with_context(|| format!("reading {}", info.exe))?;
        // Until a Chromium window's page has appeared there's only browser chrome to read.
        if chromium && !walk.has_document && (first_look || !foreground) {
            return Ok(());
        }
        let mut text = walk.text();
        let mut how = How::Uia;
        if foreground && self.settings.ocr && text.chars().count() < OCR_BELOW_CHARS {
            if let Some(read) = self.ocr_if_changed(info.window) {
                text = read;
                how = How::Ocr;
            }
        }
        if text.is_empty() {
            return Ok(());
        }
        let url = if BROWSERS.contains(&info.exe.as_str()) { walk.url } else { None };
        let (kind, source) = classify(&info.exe, &info.title, url.as_deref());
        let now = chrono::Utc::now().timestamp();
        if !self.deduper.is_new(key, &text, kind, now) {
            return Ok(());
        }
        let remembered = text.clone();
        let last_app = source.clone().unwrap_or_else(|| info.exe.clone());
        let observation = Observation { at: now, exe: info.exe.clone(), title: info.title.clone(), url, kind, source, text, how, foreground };
        self.store.add_observation(&observation)?;
        self.deduper.remember(key, &remembered, now);
        self.shared.update(|status| {
            status.stored += 1;
            status.last_app = Some(last_app);
        });
        Ok(())
    }

    fn remember_woken(&mut self, key: isize) -> bool {
        if self.woken.len() >= REMEMBERED_WINDOWS {
            self.woken.clear();
        }
        self.woken.insert(key)
    }

    fn ocr_if_changed(&mut self, window: HWND) -> Option<String> {
        let now = self.now_ms();
        if !self.gate.ready(now) {
            return None;
        }
        let (width, height, pixels) = match window_pixels(window) {
            Ok(captured) => captured,
            Err(error) => {
                self.fail("capture", &error);
                return None;
            }
        };
        if !self.gate.changed(now, &pixels) {
            return None;
        }
        match ocr_bgra(width, height, &pixels) {
            Ok(text) => {
                let lines: Vec<String> = text.lines().map(str::to_string).collect();
                Some(clean_text(&lines)).filter(|text| !text.is_empty())
            }
            Err(error) => {
                self.fail("ocr", &error);
                None
            }
        }
    }

    fn track_processes(&mut self) -> Result<()> {
        let mut now = HashSet::new();
        for (pid, exe) in running_processes() {
            let skipped = !self.pid_allowed(pid) || NOISE_PROCESSES.contains(&exe.as_str()) || self.redactor.excludes(&exe) || self.redactor.not_chosen(&exe);
            if !skipped {
                now.insert(exe);
            }
        }
        let at = chrono::Utc::now().timestamp();
        for (exe, change) in self.processes.update(now) {
            self.store.add_process_change(at, &exe, change)?;
        }
        Ok(())
    }
}

fn observer_thread(settings: Settings, store_path: &std::path::Path, shared: &Arc<Shared>, ready: &SyncSender<Result<u32, String>>) {
    let mut message = MSG::default();
    // SAFETY: looking at the queue creates it, so a WM_QUIT posted right after start can't be lost.
    let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };
    // SAFETY: only this thread's awareness changes, so window sizes match the real pixels.
    let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    match Runner::new(settings, store_path, Arc::clone(shared)) {
        Ok(mut runner) => {
            // SAFETY: takes no arguments.
            let thread_id = unsafe { GetCurrentThreadId() };
            shared.update(|status| status.running = true);
            let _ = ready.send(Ok(thread_id));
            runner.run();
        }
        Err(error) => {
            let _ = ready.send(Err(format!("{error:#}")));
        }
    }
}

/// Handles one window as if it were in the foreground, on this thread, so tests can prove that path without moving real focus.
#[doc(hidden)]
pub fn observe_as_foreground(settings: Settings, store_path: &std::path::Path, window: HWND) -> Result<Status> {
    let shared = Arc::new(Shared::default());
    let mut runner = Runner::new(settings, store_path, Arc::clone(&shared))?;
    runner.observe_foreground(window)?;
    let status = shared.status.lock().unwrap_or_else(PoisonError::into_inner).clone();
    Ok(status)
}

/// Watches the owner's windows on one background thread and stores their text locally.
pub struct Observer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
}

impl Observer {
    /// Opens the store and starts watching; fails if the store or UI Automation can't be set up.
    pub fn start(settings: Settings, store_path: PathBuf) -> Result<Observer> {
        let shared = Arc::new(Shared::default());
        let (ready, started) = sync_channel(1);
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new().name("olympus-observer".to_string()).spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| observer_thread(settings, &store_path, &thread_shared, &ready)));
            thread_shared.update(|status| {
                status.running = false;
                if outcome.is_err() {
                    status.last_error = Some("the observer stopped after a panic".to_string());
                }
            });
        })?;
        let thread_id = started.recv().context("the observer thread stopped while starting")?.map_err(|error| anyhow!(error))?;
        Ok(Observer { shared, thread: Some(thread), thread_id })
    }

    /// Stops reading and recording until `resume`.
    pub fn pause(&self) {
        self.shared.paused.store(true, Ordering::SeqCst);
    }

    /// Carries on after `pause`.
    pub fn resume(&self) {
        self.shared.paused.store(false, Ordering::SeqCst);
    }

    /// Ends the thread and waits for it; calling it twice is harmless.
    pub fn stop(&mut self) {
        let Some(thread) = self.thread.take() else { return };
        self.shared.stop.store(true, Ordering::SeqCst);
        // SAFETY: posts to the thread this observer owns, whose queue exists since before start returned.
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        let _ = thread.join();
    }

    /// What the observer has done so far.
    pub fn status(&self) -> Status {
        let mut status = self.shared.status.lock().unwrap_or_else(PoisonError::into_inner).clone();
        status.paused = self.shared.paused.load(Ordering::SeqCst);
        status
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(ToString::to_string).collect()
    }

    fn label(kind: Kind, source: &str) -> (Kind, Option<String>) {
        (kind, Some(source.to_string()))
    }

    fn facts<'a>(exe: &'a str, title: &'a str) -> WindowFacts<'a> {
        WindowFacts { pid: 100, exe, title, affinity: Some(WDA_NONE), cloaked: false }
    }

    fn redactor() -> Redactor {
        Redactor::new(&Settings::default().excluded_exes, 1)
    }

    fn activity(exe: &str, title: &str) -> Activity {
        Activity { exe: exe.into(), title: title.into(), url: None, kind: Kind::Other, source: None }
    }

    fn edit(name: &str, value: &str) -> Node {
        Node { control: UIA_EditControlTypeId.0, name: name.into(), value: Some(value.into()), ..Node::default() }
    }

    fn text_node(name: &str) -> Node {
        Node { control: 50020, name: name.into(), ..Node::default() }
    }

    #[test]
    fn classify_recognizes_native_apps() {
        assert_eq!(classify("ms-teams.exe", "Chat | Microsoft Teams", None), label(Kind::Chat, "Teams"));
        assert_eq!(classify(r"C:\Program Files\Microsoft Office\OUTLOOK.EXE", "Inbox - me@example.com - Outlook", None), label(Kind::Mail, "Outlook"));
        assert_eq!(classify("olk.exe", "Mail - Outlook", None), label(Kind::Mail, "Outlook"));
        assert_eq!(classify("slack", "general", None), label(Kind::Chat, "Slack"));
        assert_eq!(classify("Discord.exe", "#general", None), label(Kind::Chat, "Discord"));
        assert_eq!(classify("WhatsApp.exe", "WhatsApp", None), label(Kind::Chat, "WhatsApp"));
        assert_eq!(classify("Telegram.exe", "Telegram", None), label(Kind::Chat, "Telegram"));
    }

    #[test]
    fn classify_sees_calendar_and_meeting_views() {
        assert_eq!(classify("olk.exe", "Calendar - me@example.com - Outlook", None), label(Kind::Calendar, "Outlook"));
        assert_eq!(classify("ms-teams", "Calendar | Calendar | Microsoft Teams", None), label(Kind::Calendar, "Teams"));
        assert_eq!(classify("ms-teams", "Meeting in General | Microsoft Teams", None), label(Kind::Meeting, "Teams"));
    }

    #[test]
    fn classify_recognizes_mail_sites_by_title() {
        assert_eq!(classify("msedge.exe", "Inbox (3) - me@gmail.com - Gmail and 2 more pages - Profile 1 - Microsoft Edge", None), label(Kind::Mail, "Gmail"));
        assert_eq!(classify("chrome", "Mail - Robin - Outlook - Google Chrome", None), label(Kind::Mail, "Outlook"));
        assert_eq!(classify("firefox", "Inbox | me@proton.me | Proton Mail — Mozilla Firefox", None), label(Kind::Mail, "Proton"));
        assert_eq!(classify("msedge", "(3) WhatsApp - Microsoft Edge", None), label(Kind::Chat, "WhatsApp"));
    }

    #[test]
    fn classify_prefers_the_url() {
        assert_eq!(classify("msedge", "Some page", Some("https://mail.google.com/mail/u/0/")), label(Kind::Mail, "Gmail"));
        assert_eq!(classify("chrome", "x", Some("outlook.office.com/calendar/view")), label(Kind::Calendar, "Outlook"));
        assert_eq!(classify("chrome", "x", Some("https://outlook.office.com/mail/")), label(Kind::Mail, "Outlook"));
        assert_eq!(classify("chrome", "x", Some("https://acme.slack.com/archives/1")), label(Kind::Chat, "Slack"));
    }

    #[test]
    fn classify_ignores_lookalikes() {
        assert_eq!(classify("msedge", "Weather outlook for the week - Microsoft Edge", None), (Kind::Browser, None));
        assert_eq!(classify("msedge", "x", Some("https://notmail.google.com.evil.example/")), (Kind::Browser, None));
        assert_eq!(classify("msedge", "x", Some("https://evilslack.com/")), (Kind::Browser, None));
    }

    #[test]
    fn classify_recognizes_tools_and_falls_back() {
        assert_eq!(classify("Code.exe", "main.rs - Olympus - Visual Studio Code", None), (Kind::Code, None));
        assert_eq!(classify("WindowsTerminal.exe", "pwsh", None), (Kind::Terminal, None));
        assert_eq!(classify("WINWORD.EXE", "Report", None), (Kind::Document, None));
        assert_eq!(classify("mystery.exe", "Gmail", None), (Kind::Other, None));
    }

    #[test]
    fn clean_url_removes_credentials_query_and_fragment() {
        assert_eq!(clean_url("https://user:hunter2@example.com/reset?token=abc#top"), "https://example.com/reset");
        assert_eq!(clean_url("example.com/path?x=1"), "example.com/path");
        assert_eq!(clean_url("file:///C:/temp/page.html"), "file:///C:/temp/page.html");
    }

    #[test]
    fn looks_like_url_tells_addresses_from_searches() {
        assert!(looks_like_url("https://example.com"));
        assert!(looks_like_url("example.com/page"));
        assert!(looks_like_url("edge://settings"));
        assert!(!looks_like_url("what is rust"));
        assert!(!looks_like_url("hello"));
        assert!(!looks_like_url(""));
    }

    #[test]
    fn clean_text_keeps_order_and_drops_repeats_and_chrome() {
        let cleaned = clean_text(&lines(&["Reply", "First message here", "  ", "Reply", "First message here", "Second message", "Reply", "Once"]));
        assert_eq!(cleaned, "First message here\nSecond message\nOnce");
    }

    #[test]
    fn clean_text_drops_masked_runs() {
        assert_eq!(clean_text(&lines(&["••••••••••••", "****", "visible"])), "visible");
    }

    #[test]
    fn clean_text_caps_on_a_character_boundary() {
        let cleaned = clean_text(&[format!("{} x", "ö".repeat(MAX_TEXT_BYTES))]);
        assert!(cleaned.len() <= MAX_TEXT_BYTES);
        assert!(cleaned.len() > MAX_TEXT_BYTES - 4);
    }

    #[test]
    fn clean_text_keeps_both_ends_of_a_long_thread() {
        let thread: Vec<String> = (0..5000).map(|line| format!("message number {line} in the thread")).collect();
        let cleaned = clean_text(&thread);
        assert!(cleaned.len() <= MAX_TEXT_BYTES);
        assert!(cleaned.starts_with("message number 0 "));
        assert!(cleaned.ends_with("message number 4999 in the thread"));
    }

    #[test]
    fn simhash_treats_a_small_edit_as_old_and_new_content_as_new() {
        let words: Vec<String> = (0..400).map(|word| format!("word{word}")).collect();
        let base = words.join(" ");
        let edited = base.replace("word200", "changed");
        let other: String = (0..400).map(|word| format!("other{word} ")).collect();
        assert!(!is_new_text(Some(simhash(&base)), simhash(&base)));
        assert!(!is_new_text(Some(simhash(&base)), simhash(&edited)));
        assert!(is_new_text(Some(simhash(&base)), simhash(&other)));
        assert!(is_new_text(None, simhash(&base)));
    }

    #[test]
    fn simhash_handles_short_and_empty_text() {
        assert_eq!(simhash(""), 0);
        assert_eq!(simhash("!!!"), 0);
        assert_ne!(simhash("hi"), 0);
        assert_eq!(simhash("Hello, there world"), simhash("hello there WORLD"));
    }

    #[test]
    fn simhash_stays_stable_because_it_is_stored() {
        assert_eq!(simhash("a fixed sentence of words"), 1_768_974_750_353_536_328);
    }

    #[test]
    fn waking_waits_for_a_document_and_for_the_tree_to_stop_growing() {
        let mut watch = WakeWatch::default();
        assert!(!watch.is_done(false, 50), "no page yet");
        assert!(!watch.is_done(true, 90), "a panel appeared");
        assert!(!watch.is_done(true, 90));
        assert!(!watch.is_done(true, 400), "the real page arrived late");
        assert!(!watch.is_done(true, 400));
        assert!(watch.is_done(true, 400));
        let mut empty = WakeWatch::default();
        for _ in 0..5 {
            assert!(!empty.is_done(false, 50), "steady but with no page is still not ready");
        }
    }

    #[test]
    fn deduper_compares_per_window() {
        let mut deduper = Deduper::default();
        let text = "the same text in two windows";
        assert!(deduper.is_new(1, text, Kind::Other, 0));
        deduper.remember(1, text, 0);
        assert!(!deduper.is_new(1, text, Kind::Other, 10_000), "identical text is never kept twice");
        assert!(deduper.is_new(2, text, Kind::Other, 0));
    }

    #[test]
    fn a_new_message_in_a_long_thread_is_kept_within_a_minute() {
        let thread: String = (0..1000).map(|line| format!("message {line} from someone
")).collect();
        let longer = format!("{thread}one new message
");
        assert!(!is_new_text(Some(simhash(&thread)), simhash(&longer)), "too small for SimHash alone");
        let mut deduper = Deduper::default();
        deduper.remember(1, &thread, 0);
        assert!(!deduper.is_new(1, &longer, Kind::Chat, 59));
        assert!(deduper.is_new(1, &longer, Kind::Chat, 60));
        assert!(!deduper.is_new(1, &longer, Kind::Document, 60), "other apps change constantly, so they wait longer");
        assert!(deduper.is_new(1, &longer, Kind::Document, 300));
    }

    #[test]
    fn a_password_managers_web_vault_stays_unread() {
        let redactor = Redactor::new(&[], 1);
        let window = WindowFacts { pid: 2, exe: "msedge", title: "My vault | Bitwarden Web", affinity: Some(WDA_NONE), cloaked: false };
        assert!(redactor.decide(&window).is_some());
    }

    #[test]
    fn budget_moves_through_the_tiers_by_average_time() {
        for (walk_ms, interval, nodes) in [(10, 200, 5000), (49, 200, 5000), (50, 2_000, 2000), (149, 2_000, 2000), (150, 5_000, 1000), (250, 5_000, 1000), (251, 60_000, 500)] {
            let mut budget = Budget::default();
            budget.record(0, walk_ms);
            assert_eq!(budget.tier(), Tier { min_interval: Duration::from_millis(interval), max_nodes: nodes }, "{walk_ms}ms");
        }
    }

    #[test]
    fn budget_recovers_as_old_samples_fall_out() {
        let mut budget = Budget::default();
        budget.record(0, 1000);
        assert_eq!(budget.tier().max_nodes, 500);
        for at in 0..AVERAGE_OVER as u64 {
            budget.record(at, 5);
        }
        assert_eq!(budget.tier().max_nodes, 5000);
    }

    #[test]
    fn budget_waits_out_the_interval() {
        let mut budget = Budget::default();
        assert!(budget.allows(0));
        budget.record(1000, 100);
        assert!(!budget.allows(2999));
        assert!(budget.allows(3000));
    }

    #[test]
    fn pulsetime_is_the_larger_rule_in_whole_seconds() {
        assert_eq!(pulsetime(Duration::from_secs(3)), 5);
        assert_eq!(pulsetime(Duration::from_secs(1)), 2);
        assert_eq!(pulsetime(Duration::from_secs(10)), 15);
    }

    #[test]
    fn sessionizer_merges_identical_samples_within_the_pulse() {
        let mut sessions = Sessionizer::new(5);
        sessions.beat(100, activity("code", "a"));
        let spans = sessions.beat(103, activity("code", "a"));
        assert_eq!(spans.len(), 1);
        assert_eq!((spans[0].start, spans[0].end), (100, 103));
    }

    #[test]
    fn sessionizer_starts_over_after_the_pulse() {
        let mut sessions = Sessionizer::new(5);
        sessions.beat(100, activity("code", "a"));
        let spans = sessions.beat(106, activity("code", "a"));
        assert_eq!(spans.len(), 1);
        assert_eq!((spans[0].start, spans[0].end), (106, 106));
    }

    #[test]
    fn sessionizer_closes_the_old_session_at_a_switch() {
        let mut sessions = Sessionizer::new(5);
        sessions.beat(100, activity("code", "a"));
        let spans = sessions.beat(102, activity("slack", "b"));
        assert_eq!(spans.len(), 2);
        assert_eq!((spans[0].activity.exe.as_str(), spans[0].end), ("code", 102));
        assert_eq!((spans[1].activity.exe.as_str(), spans[1].start), ("slack", 102));
    }

    #[test]
    fn sessionizer_does_not_count_time_away() {
        let mut sessions = Sessionizer::new(5);
        sessions.beat(100, activity("code", "a"));
        sessions.end();
        let spans = sessions.beat(101, activity("code", "a"));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].start, 101);
    }

    #[test]
    fn redactor_lets_ordinary_windows_through() {
        assert_eq!(redactor().decide(&facts("ms-teams", "Chat | Microsoft Teams")), None);
    }

    #[test]
    fn apps_you_turned_off_or_did_not_pick_are_left_alone_exactly() {
        let window = |exe| WindowFacts { pid: 2, exe, title: "x", affinity: Some(WDA_NONE), cloaked: false };
        let off = Redactor::new(&[], 1).choosing_apps(&["code".to_string()], None);
        assert_eq!(off.decide(&window("Code.exe")), Some(Skip::ExcludedExe));
        assert_eq!(off.decide(&window("codex")), None, "turning off one app leaves others with similar names");
        let only = Redactor::new(&[], 1).choosing_apps(&[], Some(&["outlook".to_string()]));
        assert_eq!(only.decide(&window("outlook")), None);
        assert_eq!(only.decide(&window("discord")), Some(Skip::NotChosen));
    }

    #[test]
    fn banks_and_words_you_excluded_are_never_read() {
        let redactor = Redactor::new(&[], 1).hiding_titles(&["Discord".to_string(), " ".to_string(), "ab".to_string()]);
        let window = |title| WindowFacts { pid: 2, exe: "chrome", title, affinity: Some(WDA_NONE), cloaked: false };
        assert_eq!(redactor.decide(&window("Logga in - Swedbank - Google Chrome")), Some(Skip::PrivateBrowsing));
        assert_eq!(redactor.decide(&window("#general | Friends - Discord")), Some(Skip::ExcludedExe));
        assert_eq!(redactor.decide(&window("Cabin plans")), None, "words too short to mean anything are ignored");
    }

    #[test]
    fn redactor_skips_excluded_apps_and_their_relatives() {
        for exe in ["1Password.exe", "bitwarden", "KeePassXC.exe", "LastPass", "dashlane", "CredentialUIBroker.exe", "LogonUI", "consent.exe"] {
            assert_eq!(redactor().decide(&facts(exe, "anything")), Some(Skip::ExcludedExe), "{exe}");
        }
    }

    #[test]
    fn redactor_skips_private_browsing_in_any_listed_language() {
        for title in ["Page - [InPrivate] - Microsoft Edge", "Page - Google Chrome (Incognito)", "Page — Mozilla Firefox Private Browsing", "Sida (Inkognito)", "Sida - Privat surfning"] {
            assert_eq!(redactor().decide(&facts("msedge", title)), Some(Skip::PrivateBrowsing), "{title}");
        }
    }

    #[test]
    fn redactor_skips_protected_cloaked_and_unknown_affinity() {
        assert_eq!(redactor().decide(&WindowFacts { affinity: Some(1), ..facts("x", "y") }), Some(Skip::CaptureProtected));
        assert_eq!(redactor().decide(&WindowFacts { affinity: Some(0x11), ..facts("x", "y") }), Some(Skip::CaptureProtected));
        assert_eq!(redactor().decide(&WindowFacts { affinity: None, ..facts("x", "y") }), Some(Skip::CaptureProtected));
        assert_eq!(redactor().decide(&WindowFacts { cloaked: true, ..facts("x", "y") }), Some(Skip::Cloaked));
    }

    #[test]
    fn redactor_skips_olympus_itself() {
        assert_eq!(redactor().decide(&WindowFacts { pid: 1, ..facts("anything", "y") }), Some(Skip::OwnWindow));
        assert_eq!(redactor().decide(&facts("olympus.exe", "y")), Some(Skip::OwnWindow));
        assert_eq!(redactor().decide(&facts("olympus-old-120000.exe", "y")), Some(Skip::OwnWindow));
    }

    #[test]
    fn collector_drops_a_password_field_and_everything_under_it() {
        let mut collector = Collector::default();
        let field = Node { is_password: true, ..edit("Password", "hunter2") };
        assert!(collector.visit(&field, Scope::default()).is_none());
        assert!(collector.chrome.is_empty());
    }

    #[test]
    fn collector_withholds_values_of_fields_named_like_secrets() {
        let mut collector = Collector::default();
        collector.visit(&edit("Lösenord", "hunter2"), Scope::default());
        assert_eq!(collector.chrome, ["Lösenord"]);
    }

    #[test]
    fn collector_keeps_names_and_values_of_ordinary_fields() {
        let mut collector = Collector::default();
        collector.visit(&edit("Subject", "Lunch"), Scope::default());
        assert_eq!(collector.chrome, ["Subject", "Lunch"]);
    }

    #[test]
    fn collector_prefers_the_document_over_browser_chrome() {
        let mut collector = Collector::default();
        collector.visit(&text_node("Back"), Scope::default());
        let document = Node { control: UIA_DocumentControlTypeId.0, name: "Page title".into(), value: Some("https://example.com/".into()), ..Node::default() };
        let inside = collector.visit(&document, Scope::default()).expect("descends");
        collector.visit(&text_node("Page words"), inside);
        let (lines, url, has_document) = collector.finish();
        assert!(has_document);
        assert_eq!(lines, ["Page words"]);
        assert_eq!(url.as_deref(), Some("https://example.com/"));
    }

    #[test]
    fn collector_takes_the_address_bar_as_the_url_not_as_text() {
        let mut collector = Collector::default();
        let omnibox = Node { class: "OmniboxViewViews".into(), ..edit("Address and search bar", "https://example.com/a?token=1") };
        collector.visit(&omnibox, Scope::default());
        let (lines, url, _) = collector.finish();
        assert!(lines.is_empty());
        assert_eq!(url.as_deref(), Some("https://example.com/a"));
    }

    #[test]
    fn collector_skips_scrollbars() {
        let mut collector = Collector::default();
        let scrollbar = Node { control: UIA_ScrollBarControlTypeId.0, name: "Vertical".into(), ..Node::default() };
        assert!(collector.visit(&scrollbar, Scope::default()).is_none());
    }

    #[test]
    fn process_diff_reports_only_changes() {
        let mut diff = ProcessDiff::new(["code".to_string(), "slack".to_string()]);
        let changes = diff.update(["code".to_string(), "outlook".to_string()].into_iter().collect());
        assert_eq!(changes, [("outlook".to_string(), ProcessChange::Start), ("slack".to_string(), ProcessChange::Exit)]);
        assert!(diff.update(["code".to_string(), "outlook".to_string()].into_iter().collect()).is_empty());
    }

    #[test]
    fn pixel_gate_limits_rate_and_needs_a_change() {
        let mut gate = PixelGate::default();
        assert!(gate.ready(0));
        assert!(gate.changed(0, &[1, 2, 3, 4]));
        assert!(!gate.ready(OCR_EVERY_MS - 1));
        assert!(gate.ready(OCR_EVERY_MS));
        assert!(!gate.changed(OCR_EVERY_MS, &[1, 2, 3, 4]));
        assert!(gate.changed(2 * OCR_EVERY_MS, &[9, 2, 3, 4]));
    }

    #[test]
    fn pacer_stops_polling_while_you_are_away_but_still_looks_now_and_then() {
        let settings = Settings::default();
        let mut pacer = Pacer::new(&settings);
        let idle = settings.idle_after.as_millis() as u64;
        let (sweep, every) = (settings.sweep_every.as_millis() as u64, settings.idle_sweep_every.as_millis() as u64);
        assert_eq!(pacer.due(0, idle, false), Due { poll: false, sweep: false, processes: true, idle: true });
        assert!(!pacer.due(1000, idle, true).poll, "nothing to follow while nobody is there");
        assert!(pacer.due(sweep, idle, false).sweep);
        assert!(!pacer.due(sweep + every - 1, idle, false).sweep);
        assert!(pacer.due(sweep + every, idle, false).sweep);
    }

    #[test]
    fn pacer_polls_on_schedule_or_when_the_foreground_changes() {
        let mut pacer = Pacer::new(&Settings::default());
        assert!(pacer.due(0, 0, false).poll);
        assert!(!pacer.due(1000, 0, false).poll);
        assert!(pacer.due(1500, 0, true).poll);
        assert!(pacer.due(3000, 0, false).poll);
    }

    #[test]
    fn pacer_sweeps_once_a_minute_while_active() {
        let mut pacer = Pacer::new(&Settings::default());
        assert!(!pacer.due(0, 0, false).sweep);
        assert!(pacer.due(60_000, 0, false).sweep);
        assert!(!pacer.due(61_000, 0, false).sweep);
    }

    #[test]
    fn pacer_never_waits_zero() {
        let pacer = Pacer::new(&Settings::default());
        assert_eq!(pacer.wait_ms(1_000_000), 1);
    }
}
