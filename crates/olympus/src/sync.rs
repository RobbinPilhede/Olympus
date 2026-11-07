//! Keeps chats the same on every PC through storage you connect. The store only ever sees ciphertext.
use crate::session::{Block, ChatState};
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use olympus_core::target::Provider;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CHECK_FILE: &str = "sync-check.oly";
const CHECK_TEXT: &[u8] = b"olympus-sync-1";
const FORMAT: &[u8] = b"OLY1";
const NONCE_LEN: usize = 24;
const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_LEN: usize = 32;
/// Larger transcripts stay behind and the other PC continues from a summary, to keep memory bounded.
const MAX_TRANSCRIPT: u64 = 256 * 1024 * 1024;
/// A PC that stops reporting for this long has crashed or lost its connection.
const RUNNING_GOES_STALE: Duration = Duration::from_secs(180);

/// The settings file in `~/.olympus`.
pub const SETTINGS_FILE: &str = "sync.json";
const STATE_FILE: &str = "sync-state.json";

/// One file in a store; the token changes whenever the file does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub token: String,
}

/// Somewhere chats can be kept. A new kind of storage only has to implement this.
pub trait Store: Send {
    fn list(&self) -> Result<Vec<Entry>, String>;
    /// None means the file isn't there.
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, String>;
    fn put(&self, name: &str, bytes: &[u8]) -> Result<(), String>;
    fn remove(&self, name: &str) -> Result<(), String>;
    /// Readies the store for first use.
    fn prepare(&self) -> Result<(), String>;
}

/// Any folder: one a cloud app keeps in sync, a network share, or a USB drive.
pub struct FolderStore(pub PathBuf);

impl Store for FolderStore {
    fn list(&self) -> Result<Vec<Entry>, String> {
        let entries = std::fs::read_dir(&self.0).map_err(|error| format!("{} isn't reachable: {error}", self.0.display()))?;
        let mut found = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".oly") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let time = meta.modified().ok().and_then(|time| time.duration_since(UNIX_EPOCH).ok()).map_or(0, |time| time.as_nanos());
            found.push(Entry { name, token: format!("{time}-{}", meta.len()) });
        }
        Ok(found)
    }

    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        match std::fs::read(self.0.join(name)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("Unable to read {name}: {error}")),
        }
    }

    // Cloud apps upload whatever appears in the folder, so a half-written file must never carry the real name.
    fn put(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let part = self.0.join(format!("{name}.part"));
        std::fs::write(&part, bytes).map_err(|error| format!("Unable to write {name}: {error}"))?;
        std::fs::rename(&part, self.0.join(name)).map_err(|error| format!("Unable to write {name}: {error}"))
    }

    fn remove(&self, name: &str) -> Result<(), String> {
        match std::fs::remove_file(self.0.join(name)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(format!("Unable to remove {name}: {error}")),
            _ => Ok(()),
        }
    }

    fn prepare(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.0).map_err(|error| format!("Unable to use {}: {error}", self.0.display()))
    }
}

/// Any of the many services rclone reaches, through a remote made with `rclone config`.
pub struct RcloneStore {
    pub program: PathBuf,
    pub remote: String,
}

impl RcloneStore {
    fn path(&self, name: &str) -> String {
        if self.remote.ends_with([':', '/']) { format!("{}{name}", self.remote) } else { format!("{}/{name}", self.remote) }
    }

    fn run(&self, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, String> {
        let mut command = Command::new(&self.program);
        command.args(args).args(["--contimeout=15s", "--timeout=60s", "--retries=2", "--low-level-retries=2"]);
        command.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut command, 0x0800_0000);
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "rclone isn't installed. Run winget install Rclone.Rclone, set up your storage with rclone config, then try again.".to_string()
            } else {
                format!("Unable to start rclone: {error}")
            }
        })?;
        // Written on its own thread, because rclone can fill its output pipes before it has read all of this.
        let writer = input.map(<[u8]>::to_vec).zip(child.stdin.take()).map(|(bytes, mut stdin)| std::thread::spawn(move || stdin.write_all(&bytes)));
        let output = child.wait_with_output().map_err(|error| format!("rclone stopped: {error}"))?;
        if let Some(writer) = writer {
            let _ = writer.join();
        }
        if output.status.success() {
            return Ok(output.stdout);
        }
        let problem = String::from_utf8_lossy(&output.stderr);
        Err(problem.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("rclone failed.").trim().to_string())
    }
}

impl Store for RcloneStore {
    fn list(&self) -> Result<Vec<Entry>, String> {
        let output = self.run(&["lsf", "--files-only", "--format=tsp", "--separator=|", &self.remote], None)?;
        let text = String::from_utf8_lossy(&output);
        Ok(text
            .lines()
            .filter_map(|line| {
                let (token, name) = line.rsplit_once('|')?;
                name.ends_with(".oly").then(|| Entry { name: name.to_string(), token: token.to_string() })
            })
            .collect())
    }

    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        match self.run(&["cat", &self.path(name)], None) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(problem) if problem.contains("not found") => Ok(None),
            Err(problem) => Err(problem),
        }
    }

    fn put(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        self.run(&["rcat", &self.path(name)], Some(bytes)).map(|_| ())
    }

    fn remove(&self, name: &str) -> Result<(), String> {
        match self.run(&["deletefile", &self.path(name)], None) {
            Err(problem) if !problem.contains("not found") => Err(problem),
            _ => Ok(()),
        }
    }

    fn prepare(&self) -> Result<(), String> {
        self.run(&["mkdir", &self.remote], None).map(|_| ())
    }
}

/// True for text meaning an rclone remote like `gdrive:Olympus` rather than a folder like `C:\Sync`.
pub fn is_remote(text: &str) -> bool {
    text.find(':').is_some_and(|at| at >= 2 && !text[..at].contains(['/', '\\']))
}

/// Opens the store a connection names.
pub fn open_store(place: &str) -> Box<dyn Store> {
    if is_remote(place) {
        Box::new(RcloneStore { program: PathBuf::from("rclone"), remote: place.to_string() })
    } else {
        Box::new(FolderStore(PathBuf::from(place)))
    }
}

/// `<where> [code]`, with the where optionally in quotes.
pub fn parse_connect(args: &str) -> Option<(String, Option<String>)> {
    let args = args.trim();
    if let Some(rest) = args.strip_prefix('"') {
        let (place, code) = rest.split_once('"')?;
        let code = code.trim();
        return (!place.is_empty()).then(|| (place.to_string(), (!code.is_empty()).then(|| code.to_string())));
    }
    if args.is_empty() {
        return None;
    }
    match args.rsplit_once(char::is_whitespace) {
        Some((place, last)) if is_code(last) => Some((place.trim().to_string(), Some(last.to_string()))),
        _ => Some((args.to_string(), None)),
    }
}

fn normal_code(code: &str) -> String {
    code.chars().filter(|c| *c != '-').map(|c| c.to_ascii_uppercase()).collect()
}

/// True for text in the shape of a sync code.
pub fn is_code(code: &str) -> bool {
    let code = normal_code(code);
    code.len() == CODE_LEN && code.bytes().all(|byte| CODE_ALPHABET.contains(&byte))
}

fn new_code() -> String {
    let random = XChaCha20Poly1305::generate_key(&mut OsRng);
    let text: Vec<u8> = random.iter().take(CODE_LEN).map(|byte| CODE_ALPHABET[usize::from(byte % 32)]).collect();
    text.chunks(4).map(|group| String::from_utf8_lossy(group).to_string()).collect::<Vec<_>>().join("-")
}

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Encrypts and decrypts what goes to the store.
pub struct Sealer {
    cipher: XChaCha20Poly1305,
    names: String,
}

impl Sealer {
    /// The key comes from the sync code, which already holds 160 random bits, so it needs no stretching.
    pub fn new(code: &str) -> Self {
        let code = normal_code(code);
        let digest = Sha256::digest(format!("olympus-sync-1:{code}"));
        Self { cipher: XChaCha20Poly1305::new(&digest), names: sha_hex(format!("olympus-sync-names-1:{code}").as_bytes()) }
    }

    // The file name is authenticated, so the store can't pass one file off as another.
    fn seal(&self, name: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
        let mut packer = GzEncoder::new(Vec::new(), Compression::default());
        packer.write_all(bytes).map_err(|error| error.to_string())?;
        let packed = packer.finish().map_err(|error| error.to_string())?;
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let sealed = self.cipher.encrypt(&nonce, Payload { msg: &packed, aad: name.as_bytes() }).map_err(|_| "Unable to encrypt.".to_string())?;
        Ok([FORMAT, nonce.as_slice(), &sealed].concat())
    }

    fn open(&self, name: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
        let body = bytes.strip_prefix(FORMAT).filter(|body| body.len() > NONCE_LEN).ok_or("Not a sync file.")?;
        let (nonce, sealed) = body.split_at(NONCE_LEN);
        let packed = self.cipher.decrypt(XNonce::from_slice(nonce), Payload { msg: sealed, aad: name.as_bytes() }).map_err(|_| "It doesn't match this sync code.".to_string())?;
        let mut plain = Vec::new();
        GzDecoder::new(packed.as_slice()).read_to_end(&mut plain).map_err(|error| error.to_string())?;
        Ok(plain)
    }
}

/// Where this PC keeps things.
#[derive(Debug, Clone)]
pub struct Layout {
    pub user_home: PathBuf,
    pub olympus_home: PathBuf,
}

impl Layout {
    fn sessions(&self) -> PathBuf {
        self.olympus_home.join("sessions")
    }
}

/// What's saved in `sync.json`. The code is a secret: it opens everything in the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub place: String,
    pub code: String,
    pub device: String,
}

impl Settings {
    /// How often to look; online services cost a process and an API call each time.
    pub fn every(&self) -> Duration {
        Duration::from_secs(if is_remote(&self.place) { 30 } else { 5 })
    }

    /// Loads the settings; none means sync isn't set up.
    pub fn load(path: &Path) -> Option<Self> {
        std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok())
    }

    /// Saves the settings.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        write_atomic(path, text.as_bytes()).map_err(|error| error.to_string())
    }
}

/// Why connecting didn't work.
#[derive(Debug, PartialEq, Eq)]
pub enum ConnectError {
    /// This store already has chats in it, so this PC must prove it belongs.
    NeedsCode,
    Problem(String),
}

/// Joins a store, or starts one with a new code if it's empty.
pub fn connect(store: Box<dyn Store>, layout: Layout, place: &str, code: Option<String>, device: String) -> Result<(Syncer, Settings), ConnectError> {
    store.prepare().map_err(ConnectError::Problem)?;
    let existing = store.get(CHECK_FILE).map_err(ConnectError::Problem)?;
    let code = match (existing, code) {
        (Some(_), None) => return Err(ConnectError::NeedsCode),
        (Some(check), Some(code)) => {
            if !is_code(&code) || Sealer::new(&code).open(CHECK_FILE, &check).ok().as_deref() != Some(CHECK_TEXT) {
                return Err(ConnectError::Problem("That code doesn't match this storage. Run /sync code on your other PC and copy it again.".into()));
            }
            code
        }
        (None, given) => {
            if given.as_deref().is_some_and(|code| !is_code(code)) {
                return Err(ConnectError::Problem("That doesn't look like a sync code. Run /sync code on your other PC and copy it again.".into()));
            }
            let code = given.unwrap_or_else(new_code);
            let check = Sealer::new(&code).seal(CHECK_FILE, CHECK_TEXT).map_err(ConnectError::Problem)?;
            store.put(CHECK_FILE, &check).map_err(ConnectError::Problem)?;
            code
        }
    };
    let settings = Settings { place: place.to_string(), code, device };
    Ok((Syncer::new(store, layout, &settings), settings))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Kind {
    Claude,
    Codex,
    Hosted,
}

/// A chat's real conversation, which the provider needs in order to resume it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Transcript {
    kind: Kind,
    session: String,
    /// Where it sits below Codex's sessions folder, whose date folders Codex searches.
    rel: String,
    blob: String,
    sha: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    version: String,
    device: String,
    home: PathBuf,
    running: bool,
    closed: bool,
    state: Option<ChatState>,
    transcript: Option<Transcript>,
}

/// What this PC last knew about one thread.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Thread {
    token: Option<String>,
    version: String,
    local: String,
    transcript: Option<Transcript>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Memory {
    place: String,
    threads: BTreeMap<String, Thread>,
    closing: BTreeSet<String>,
    devices: BTreeSet<String>,
}

/// One open chat with content, already saved to disk.
#[derive(Debug, Clone)]
pub struct Open {
    pub id: String,
    pub running: bool,
}

/// A change for the interface to apply.
#[derive(Debug)]
pub enum Incoming {
    Apply {
        id: String,
        state: Box<ChatState>,
        /// The saved chat this replaces; anything else means it changed meanwhile, and both must be kept.
        expected: Option<String>,
        running_on: Option<String>,
        copy: Box<ChatState>,
        copy_id: String,
    },
    Copy { id: String, state: Box<ChatState> },
    Close { id: String, expected: Option<String> },
}

/// How one round went.
#[derive(Debug, Default)]
pub struct Outcome {
    pub incoming: Vec<Incoming>,
    pub error: Option<String>,
    pub pushed: usize,
    /// Files in the store that fail to decrypt, such as a cloud app's conflict copies.
    pub unreadable: usize,
    pub devices: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct Located {
    kind: Kind,
    rel: String,
    path: PathBuf,
}

struct Local {
    id: String,
    running: bool,
    state_sha: String,
    located: Option<Located>,
    file_sha: String,
    hash: String,
}

/// Does the syncing, off the interface thread.
pub struct Syncer {
    store: Box<dyn Store>,
    sealer: Sealer,
    layout: Layout,
    device: String,
    memory: Memory,
    hashes: HashMap<PathBuf, (SystemTime, u64, String)>,
    located: HashMap<String, (String, Option<Located>)>,
}

fn thread_file(id: &str) -> String {
    format!("thread-{id}.oly")
}

fn version_of(state_sha: &str, file_sha: &str, marker: &str) -> String {
    sha_hex(format!("{state_sha}|{file_sha}|{marker}").as_bytes())
}

fn is_safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn is_safe_rel(rel: &str) -> bool {
    let path = Path::new(rel);
    path.extension().is_some_and(|extension| extension == "jsonl") && path.components().all(|part| matches!(part, Component::Normal(_)))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let part = path.with_extension("part");
    std::fs::write(&part, bytes)?;
    std::fs::rename(part, path)
}

/// The folder name Claude Code gives a project.
pub fn claude_project_folder(cwd: &Path) -> String {
    cwd.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

fn find_rollout(dir: &Path, session: &str, depth: u32) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth > 0 {
                if let Some(found) = find_rollout(&path, session, depth - 1) {
                    return Some(found);
                }
            }
        } else if path.extension().is_some_and(|extension| extension == "jsonl") && path.file_stem().is_some_and(|stem| stem.to_string_lossy().ends_with(session)) {
            return Some(path);
        }
    }
    None
}

impl Syncer {
    /// Builds a syncer for a known connection, remembering what it knew about this store.
    pub fn new(store: Box<dyn Store>, layout: Layout, settings: &Settings) -> Self {
        let saved = std::fs::read_to_string(layout.olympus_home.join(STATE_FILE)).ok().and_then(|text| serde_json::from_str::<Memory>(&text).ok());
        let memory = saved.filter(|memory| memory.place == settings.place).unwrap_or_else(|| Memory { place: settings.place.clone(), ..Memory::default() });
        Self { store, sealer: Sealer::new(&settings.code), layout, device: settings.device.clone(), memory, hashes: HashMap::new(), located: HashMap::new() }
    }

    // Keyed, so the store can't test whether you hold a file it already knows.
    fn blob_file(&self, sha: &str) -> String {
        format!("blob-{}.oly", sha_hex(format!("{}|{sha}", self.sealer.names).as_bytes()))
    }

    fn sha_of(&mut self, path: &Path) -> Option<String> {
        let meta = std::fs::metadata(path).ok()?;
        let (time, len) = (meta.modified().ok()?, meta.len());
        if let Some((known_time, known_len, sha)) = self.hashes.get(path) {
            if *known_time == time && *known_len == len {
                return Some(sha.clone());
            }
        }
        let mut hasher = Sha256::new();
        std::io::copy(&mut std::fs::File::open(path).ok()?, &mut hasher).ok()?;
        let sha: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
        self.hashes.insert(path.to_path_buf(), (time, len, sha.clone()));
        Some(sha)
    }

    fn account_root(&self, state: &ChatState, kind: Kind) -> PathBuf {
        let (chosen, default) = match kind {
            Kind::Codex => (state.codex_account_dir.as_ref(), ".codex"),
            _ => (state.account_dir.as_ref(), ".claude"),
        };
        chosen.cloned().unwrap_or_else(|| self.layout.user_home.join(default))
    }

    fn locate(&self, state: &ChatState) -> Option<Located> {
        let session = state.vendor_session.as_deref().filter(|session| is_safe_id(session))?;
        match state.provider()? {
            Provider::Claude => {
                let projects = self.account_root(state, Kind::Claude).join("projects");
                let name = format!("{session}.jsonl");
                let direct = projects.join(claude_project_folder(&state.cwd)).join(&name);
                let path = if direct.exists() {
                    direct
                } else {
                    std::fs::read_dir(&projects).ok()?.flatten().map(|entry| entry.path().join(&name)).find(|path| path.exists())?
                };
                Some(Located { kind: Kind::Claude, rel: name, path })
            }
            Provider::Codex => {
                let sessions = self.account_root(state, Kind::Codex).join("sessions");
                let path = find_rollout(&sessions, session, 3)?;
                let rel = path.strip_prefix(&sessions).ok()?.to_string_lossy().replace('\\', "/");
                Some(Located { kind: Kind::Codex, rel, path })
            }
            Provider::OpenRouter | Provider::Node(_) => {
                let name = format!("{session}.json");
                let path = self.layout.olympus_home.join("oa-sessions").join(&name);
                path.exists().then_some(Located { kind: Kind::Hosted, rel: name, path })
            }
        }
    }

    fn local(&mut self, open: &Open) -> Option<Local> {
        let path = self.layout.sessions().join(format!("{}.json", open.id));
        let state_sha = self.sha_of(&path)?;
        let cached = self.located.get(&open.id).filter(|(sha, _)| *sha == state_sha).map(|(_, located)| located.clone());
        let located = match cached {
            Some(located) => located,
            None => {
                let state: ChatState = serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
                let located = self.locate(&state);
                self.located.insert(open.id.clone(), (state_sha.clone(), located.clone()));
                located
            }
        };
        let too_big = located.as_ref().is_some_and(|found| std::fs::metadata(&found.path).map_or(true, |meta| meta.len() > MAX_TRANSCRIPT));
        // A running turn is still writing its transcript, so that goes when the turn ends.
        let file_sha = if open.running || too_big { String::new() } else { located.as_ref().and_then(|found| self.sha_of(&found.path)).unwrap_or_default() };
        let marker = if open.running { format!("running-{}", SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |time| time.as_secs() / 60)) } else { String::new() };
        let hash = version_of(&state_sha, &file_sha, &marker);
        let located = if too_big { None } else { located };
        Some(Local { id: open.id.clone(), running: open.running, state_sha, located, file_sha, hash })
    }

    fn map_folder(&self, cwd: &Path, from_home: &Path) -> Result<PathBuf, ()> {
        if let Ok(rel) = cwd.strip_prefix(from_home) {
            let here = self.layout.user_home.join(rel);
            if here.is_dir() {
                return Ok(here);
            }
        }
        if cwd.is_dir() { Ok(cwd.to_path_buf()) } else { Err(()) }
    }

    fn map_account(&self, dir: Option<PathBuf>) -> Option<PathBuf> {
        let here = self.layout.user_home.join(dir?.file_name()?);
        here.is_dir().then_some(here)
    }

    // The same project usually sits at a different path on another PC, and sign-ins never travel.
    fn map_state(&self, mut state: ChatState, from_home: &Path) -> ChatState {
        match self.map_folder(&state.cwd, from_home) {
            Ok(cwd) => state.cwd = cwd,
            Err(()) => {
                let note = format!("{} isn't on this PC, so this chat works in your user folder here.", state.cwd.display());
                state.cwd.clone_from(&self.layout.user_home);
                if !state.blocks.iter().any(|block| matches!(block, Block::Note(text) if *text == note)) {
                    state.blocks.push(Block::Note(note));
                }
            }
        }
        state.account_dir = self.map_account(state.account_dir.take());
        state.codex_account_dir = self.map_account(state.codex_account_dir.take());
        state
    }

    fn transcript_path(&self, state: &ChatState, transcript: &Transcript) -> PathBuf {
        match transcript.kind {
            Kind::Claude => self.account_root(state, Kind::Claude).join("projects").join(claude_project_folder(&state.cwd)).join(format!("{}.jsonl", transcript.session)),
            Kind::Codex => self.account_root(state, Kind::Codex).join("sessions").join(&transcript.rel),
            Kind::Hosted => self.layout.olympus_home.join("oa-sessions").join(format!("{}.json", transcript.session)),
        }
    }

    // Returns the transcript's hash here, or None if its blob hasn't reached this PC yet.
    fn place_transcript(&mut self, state: &ChatState, transcript: &Transcript) -> Result<Option<String>, String> {
        let path = self.transcript_path(state, transcript);
        if self.sha_of(&path).as_deref() == Some(transcript.sha.as_str()) {
            return Ok(Some(transcript.sha.clone()));
        }
        let Some(sealed) = self.store.get(&transcript.blob)? else { return Ok(None) };
        let bytes = self.sealer.open(&transcript.blob, &sealed)?;
        // A longer file here means a terminal session added to it, and that work mustn't be overwritten.
        let local_len = std::fs::metadata(&path).map_or(0, |meta| meta.len());
        if local_len <= bytes.len() as u64 {
            write_atomic(&path, &bytes).map_err(|error| format!("Unable to save {}: {error}", path.display()))?;
        }
        Ok(self.sha_of(&path))
    }

    fn valid(record: &Record) -> bool {
        let good_session = record.state.as_ref().and_then(|state| state.vendor_session.as_deref()).is_none_or(is_safe_id);
        let good_file = record.transcript.as_ref().is_none_or(|file| {
            let rel_ok = file.kind != Kind::Codex || is_safe_rel(&file.rel);
            is_safe_id(&file.session) && rel_ok && file.blob.starts_with("blob-") && file.blob.ends_with(".oly") && !file.blob.contains(['/', '\\'])
        });
        good_session && good_file
    }

    fn as_copy(state: &ChatState, device: &str) -> ChatState {
        let mut copy = state.clone();
        copy.title = format!("{} (from {device})", state.title);
        copy.vendor_session = None;
        copy.carry_context = !copy.blocks.is_empty();
        copy
    }

    fn pull_one(&mut self, entry: &Entry, id: &str, locals: &HashMap<String, Local>, outcome: &mut Outcome, applied: &mut BTreeSet<String>) -> Result<(), String> {
        let known = self.memory.threads.get(id).cloned().unwrap_or_default();
        if known.token.as_deref() == Some(entry.token.as_str()) || self.memory.closing.contains(id) {
            return Ok(());
        }
        let Some(sealed) = self.store.get(&entry.name)? else { return Ok(()) };
        let parsed = self.sealer.open(&entry.name, &sealed).ok().and_then(|plain| serde_json::from_slice::<Record>(&plain).ok()).filter(Self::valid);
        let Some(record) = parsed else {
            outcome.unreadable += 1;
            return Ok(());
        };
        if record.device != self.device {
            self.memory.devices.insert(record.device.clone());
        }
        let local = locals.get(id);
        if record.version == known.version || local.is_some_and(|local| local.hash == record.version) {
            let thread = self.memory.threads.entry(id.to_string()).or_default();
            thread.token = Some(entry.token.clone());
            thread.version.clone_from(&record.version);
            if let Some(local) = local.filter(|local| local.hash == record.version) {
                thread.local.clone_from(&local.hash);
            }
            thread.transcript.clone_from(&record.transcript);
            return Ok(());
        }
        // A running chat has the only live copy, so whatever arrived waits until its turn ends.
        if local.is_some_and(|local| local.running) {
            return Ok(());
        }
        let changed_here = local.is_some_and(|local| local.hash != known.local);
        if record.closed {
            if !changed_here && local.is_some() {
                outcome.incoming.push(Incoming::Close { id: id.to_string(), expected: local.map(|local| local.state_sha.clone()) });
                applied.insert(id.to_string());
            }
            let thread = self.memory.threads.entry(id.to_string()).or_default();
            thread.token = Some(entry.token.clone());
            thread.version = record.version;
            if !changed_here {
                thread.local = String::new();
            }
            thread.transcript = None;
            return Ok(());
        }
        let Some(state) = record.state else {
            outcome.unreadable += 1;
            return Ok(());
        };
        let copy_id = format!("{id}-{}", &record.version[..8.min(record.version.len())]);
        if changed_here {
            let copy = self.map_state(Self::as_copy(&state, &record.device), &record.home);
            outcome.incoming.push(Incoming::Copy { id: copy_id, state: Box::new(copy) });
            let thread = self.memory.threads.entry(id.to_string()).or_default();
            thread.token = Some(entry.token.clone());
            thread.version = record.version;
            thread.transcript = record.transcript;
            return Ok(());
        }
        let mut mapped = self.map_state(state.clone(), &record.home);
        let mut file_sha = String::new();
        match &record.transcript {
            Some(transcript) => match self.place_transcript(&mapped, transcript)? {
                Some(sha) => file_sha = sha,
                // The record can arrive before its transcript does, so look again next round.
                None => return Ok(()),
            },
            None if mapped.vendor_session.is_some() => {
                mapped.vendor_session = None;
                mapped.carry_context = !mapped.blocks.is_empty();
            }
            None => {}
        }
        let text = serde_json::to_string(&mapped).map_err(|error| error.to_string())?;
        let state_sha = sha_hex(text.as_bytes());
        let copy = self.map_state(Self::as_copy(&state, &record.device), &record.home);
        let thread = self.memory.threads.entry(id.to_string()).or_default();
        thread.token = Some(entry.token.clone());
        thread.version.clone_from(&record.version);
        thread.local = version_of(&state_sha, &file_sha, "");
        thread.transcript.clone_from(&record.transcript);
        outcome.incoming.push(Incoming::Apply {
            id: id.to_string(),
            state: Box::new(mapped),
            expected: local.map(|local| local.state_sha.clone()),
            running_on: record.running.then_some(record.device),
            copy: Box::new(copy),
            copy_id,
        });
        applied.insert(id.to_string());
        Ok(())
    }

    fn drop_blob(&self, old: Option<&Transcript>, new: Option<&Transcript>) -> Result<(), String> {
        let Some(old) = old.filter(|old| new.is_none_or(|new| new.blob != old.blob)) else { return Ok(()) };
        let shared = self.memory.threads.values().any(|thread| thread.transcript.as_ref().is_some_and(|file| file.blob == old.blob));
        if shared { Ok(()) } else { self.store.remove(&old.blob) }
    }

    fn push_closed(&mut self) -> Result<(), String> {
        let home = self.layout.user_home.clone();
        for id in self.memory.closing.clone() {
            let Some(mut thread) = self.memory.threads.get(&id).cloned() else {
                self.memory.closing.remove(&id);
                continue;
            };
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |time| time.as_nanos());
            let version = version_of(&id, "closed", &stamp.to_string());
            let record = Record { version: version.clone(), device: self.device.clone(), home: home.clone(), running: false, closed: true, state: None, transcript: None };
            let name = thread_file(&id);
            let plain = serde_json::to_vec(&record).map_err(|error| error.to_string())?;
            self.store.put(&name, &self.sealer.seal(&name, &plain)?)?;
            let old = thread.transcript.take();
            thread.token = None;
            thread.version = version;
            thread.local = String::new();
            self.memory.threads.insert(id.clone(), thread);
            self.drop_blob(old.as_ref(), None)?;
            self.memory.closing.remove(&id);
        }
        Ok(())
    }

    fn push_one(&mut self, local: &Local, listed: &BTreeSet<String>) -> Result<(), String> {
        let path = self.layout.sessions().join(format!("{}.json", local.id));
        let text = std::fs::read_to_string(&path).map_err(|error| format!("Unable to read a saved chat: {error}"))?;
        let state: ChatState = serde_json::from_str(&text).map_err(|error| format!("Unable to read a saved chat: {error}"))?;
        let previous = self.memory.threads.get(&local.id).cloned().unwrap_or_default();
        let transcript = if local.running {
            previous.transcript.clone()
        } else {
            match local.located.as_ref().filter(|_| !local.file_sha.is_empty()) {
                Some(found) => {
                    let blob = self.blob_file(&local.file_sha);
                    if !listed.contains(&blob) {
                        let bytes = std::fs::read(&found.path).map_err(|error| format!("Unable to read a transcript: {error}"))?;
                        self.store.put(&blob, &self.sealer.seal(&blob, &bytes)?)?;
                    }
                    let session = state.vendor_session.clone().unwrap_or_default();
                    Some(Transcript { kind: found.kind, session, rel: found.rel.clone(), blob, sha: local.file_sha.clone() })
                }
                None => None,
            }
        };
        let record = Record { version: local.hash.clone(), device: self.device.clone(), home: self.layout.user_home.clone(), running: local.running, closed: false, state: Some(state), transcript: transcript.clone() };
        let name = thread_file(&local.id);
        let plain = serde_json::to_vec(&record).map_err(|error| error.to_string())?;
        // In this order, so nobody ever reads a record whose transcript hasn't been uploaded.
        self.store.put(&name, &self.sealer.seal(&name, &plain)?)?;
        let thread = self.memory.threads.entry(local.id.clone()).or_default();
        thread.token = None;
        thread.version.clone_from(&local.hash);
        thread.local.clone_from(&local.hash);
        thread.transcript = transcript.clone();
        self.drop_blob(previous.transcript.as_ref(), transcript.as_ref())
    }

    fn round(&mut self, open: &[Open], outcome: &mut Outcome) -> Result<(), String> {
        let entries = self.store.list()?;
        let locals: HashMap<String, Local> = open.iter().filter_map(|chat| self.local(chat)).map(|local| (local.id.clone(), local)).collect();
        let mut applied = BTreeSet::new();
        for entry in &entries {
            let Some(id) = entry.name.strip_prefix("thread-").and_then(|name| name.strip_suffix(".oly")).filter(|id| is_safe_id(id)) else { continue };
            self.pull_one(entry, id, &locals, outcome, &mut applied)?;
        }
        self.push_closed()?;
        let listed: BTreeSet<String> = entries.into_iter().map(|entry| entry.name).collect();
        for local in locals.values() {
            let known = self.memory.threads.get(&local.id).map(|thread| thread.local.as_str());
            if applied.contains(&local.id) || known == Some(local.hash.as_str()) {
                continue;
            }
            self.push_one(local, &listed)?;
            outcome.pushed += 1;
        }
        Ok(())
    }

    /// Exchanges changes with the store once.
    pub fn cycle(&mut self, open: &[Open], closed: Vec<String>) -> Outcome {
        let mut outcome = Outcome::default();
        self.memory.closing.extend(closed);
        if let Err(problem) = self.round(open, &mut outcome) {
            outcome.error = Some(problem);
        }
        outcome.devices.clone_from(&self.memory.devices);
        if let Ok(text) = serde_json::to_vec(&self.memory) {
            // If this can't be written, the next round repeats some work and changes nothing.
            let _ = write_atomic(&self.layout.olympus_home.join(STATE_FILE), &text);
        }
        outcome
    }
}

/// The hash a chat's saved file would have.
pub fn state_sha(state: &ChatState) -> Option<String> {
    serde_json::to_string(state).ok().map(|text| sha_hex(text.as_bytes()))
}

/// When a chat last heard from the PC running it; it counts as running only while that's recent.
pub fn is_fresh(heard: Instant) -> bool {
    heard.elapsed() < RUNNING_GOES_STALE
}

/// This PC's connection, as the interface holds it.
pub struct Link {
    pub settings: Settings,
    /// Empty while a round is running on another thread.
    pub syncer: Option<Box<Syncer>>,
    /// What each chat looked like when last saved for syncing.
    pub keys: HashMap<String, String>,
    pub last_round: Option<Instant>,
    pub last_ok: Option<Instant>,
    pub problem: Option<String>,
    pub unreadable: usize,
    pub devices: BTreeSet<String>,
}

impl Link {
    /// A link ready for its first round.
    pub fn new(syncer: Syncer, settings: Settings) -> Self {
        Self { settings, syncer: Some(Box::new(syncer)), keys: HashMap::new(), last_round: None, last_ok: None, problem: None, unreadable: 0, devices: BTreeSet::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Choice;
    use olympus_core::event::Usage;
    use olympus_core::target::Target;

    fn opens(ids: &[&str]) -> Vec<Open> {
        ids.iter().map(|id| Open { id: (*id).to_string(), running: false }).collect()
    }

    fn chat(title: &str, cwd: &Path, session: Option<&str>, spec: &str) -> ChatState {
        let target = Target::parse(spec).expect("target");
        ChatState {
            title: title.into(),
            cwd: cwd.to_path_buf(),
            choice: Choice::Fixed(target.clone()),
            last_target: Some(target),
            vendor_session: session.map(String::from),
            full_profile: false,
            usage: Usage::default(),
            last_branch: None,
            account_dir: None,
            codex_account_dir: None,
            carry_context: false,
            blocks: vec![Block::User("hello".into()), Block::Assistant { label: None, text: "the word is pineapple".into() }],
        }
    }

    /// One pretend PC with its own home folder, joined to a shared store.
    struct Pc {
        home: tempfile::TempDir,
        syncer: Syncer,
    }

    impl Pc {
        fn sessions(&self) -> PathBuf {
            self.home.path().join(".olympus").join("sessions")
        }

        fn save(&self, id: &str, state: &ChatState) {
            std::fs::create_dir_all(self.sessions()).expect("sessions");
            std::fs::write(self.sessions().join(format!("{id}.json")), serde_json::to_string(state).expect("json")).expect("save");
        }

        fn load(&self, id: &str) -> ChatState {
            serde_json::from_str(&std::fs::read_to_string(self.sessions().join(format!("{id}.json"))).expect("read")).expect("parse")
        }

        fn folder(&self, name: &str) -> PathBuf {
            let path = self.home.path().join(name);
            std::fs::create_dir_all(&path).expect("folder");
            path
        }

        fn claude_file(&self, cwd: &Path, session: &str) -> PathBuf {
            self.home.path().join(".claude").join("projects").join(claude_project_folder(cwd)).join(format!("{session}.jsonl"))
        }

        // Does what the interface does with a round's results.
        fn round(&mut self, open: &[Open], closed: Vec<String>) -> Outcome {
            let outcome = self.syncer.cycle(open, closed);
            assert_eq!(outcome.error, None);
            for incoming in &outcome.incoming {
                match incoming {
                    Incoming::Apply { id, state, .. } | Incoming::Copy { id, state } => self.save(id, state),
                    Incoming::Close { id, .. } => std::fs::remove_file(self.sessions().join(format!("{id}.json"))).expect("close"),
                }
            }
            outcome
        }
    }

    fn pc(store: &Path, code: Option<String>) -> (Pc, String) {
        let home = tempfile::tempdir().expect("home");
        let layout = Layout { user_home: home.path().to_path_buf(), olympus_home: home.path().join(".olympus") };
        std::fs::create_dir_all(&layout.olympus_home).expect("olympus");
        let (syncer, settings) = connect(Box::new(FolderStore(store.to_path_buf())), layout, &store.to_string_lossy(), code, "PC".into()).expect("connect");
        (Pc { home, syncer }, settings.code)
    }

    fn two_pcs(store: &tempfile::TempDir) -> (Pc, Pc) {
        let (first, code) = pc(store.path(), None);
        let (second, _) = pc(store.path(), Some(code));
        (first, second)
    }

    fn write_file(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, text).expect("write");
    }

    #[test]
    fn a_chat_and_its_memory_arrive_in_the_same_project_on_the_other_pc() {
        let store = tempfile::tempdir().expect("store");
        let (mut first, mut second) = two_pcs(&store);
        let (here, there) = (first.folder("code/app"), second.folder("code/app"));
        write_file(&first.claude_file(&here, "s1"), "{\"line\":1}\n");
        first.save("a", &chat("Plan", &here, Some("s1"), "claude:opus"));

        assert_eq!(first.round(&opens(&["a"]), vec![]).pushed, 1);
        let outcome = second.round(&[], vec![]);

        assert_eq!(outcome.incoming.len(), 1);
        let arrived = second.load("a");
        assert_eq!(arrived.cwd, there);
        assert_eq!(arrived.vendor_session.as_deref(), Some("s1"));
        assert_eq!(std::fs::read_to_string(second.claude_file(&there, "s1")).expect("transcript"), "{\"line\":1}\n");
    }

    #[test]
    fn nothing_moves_when_nothing_changed() {
        let store = tempfile::tempdir().expect("store");
        let (mut first, mut second) = two_pcs(&store);
        let (here, there) = (first.folder("code/app"), second.folder("code/app"));
        write_file(&first.claude_file(&here, "s1"), "one\n");
        first.save("a", &chat("Plan", &here, Some("s1"), "claude:opus"));
        first.round(&opens(&["a"]), vec![]);
        second.round(&[], vec![]);
        let _ = there;

        for _ in 0..2 {
            let (from_first, from_second) = (first.round(&opens(&["a"]), vec![]), second.round(&opens(&["a"]), vec![]));
            assert_eq!((from_first.pushed, from_first.incoming.len()), (0, 0));
            assert_eq!((from_second.pushed, from_second.incoming.len()), (0, 0));
        }
    }

    #[test]
    fn a_reply_on_the_second_pc_comes_back_to_the_first() {
        let store = tempfile::tempdir().expect("store");
        let (mut first, mut second) = two_pcs(&store);
        let (here, there) = (first.folder("code/app"), second.folder("code/app"));
        write_file(&first.claude_file(&here, "s1"), "one\n");
        first.save("a", &chat("Plan", &here, Some("s1"), "claude:opus"));
        first.round(&opens(&["a"]), vec![]);
        second.round(&[], vec![]);

        let mut changed = second.load("a");
        changed.blocks.push(Block::User("carry on".into()));
        second.save("a", &changed);
        write_file(&second.claude_file(&there, "s1"), "one\ntwo\n");
        assert_eq!(second.round(&opens(&["a"]), vec![]).pushed, 1);
        let outcome = first.round(&opens(&["a"]), vec![]);

        assert_eq!(outcome.incoming.len(), 1);
        assert_eq!(first.load("a").blocks.len(), 3);
        assert_eq!(first.load("a").cwd, here);
        assert_eq!(std::fs::read_to_string(first.claude_file(&here, "s1")).expect("transcript"), "one\ntwo\n");
    }

    #[test]
    fn changing_a_chat_on_both_pcs_keeps_both_versions() {
        let store = tempfile::tempdir().expect("store");
        let (mut first, mut second) = two_pcs(&store);
        let (here, _there) = (first.folder("code/app"), second.folder("code/app"));
        first.save("a", &chat("Plan", &here, None, "claude:opus"));
        first.round(&opens(&["a"]), vec![]);
        second.round(&[], vec![]);

        let mut mine = first.load("a");
        mine.blocks.push(Block::User("from the first PC".into()));
        first.save("a", &mine);
        let mut theirs = second.load("a");
        theirs.blocks.push(Block::User("from the second PC".into()));
        second.save("a", &theirs);
        second.round(&opens(&["a"]), vec![]);
        let outcome = first.round(&opens(&["a"]), vec![]);

        let Some(Incoming::Copy { id, state }) = outcome.incoming.first() else { panic!("expected a copy: {outcome:?}") };
        assert_ne!(id, "a");
        assert!(state.title.contains("(from PC)"));
        assert!(state.carry_context && state.vendor_session.is_none(), "a copy can't share the original's session");
        assert!(matches!(state.blocks.last(), Some(Block::User(text)) if text == "from the second PC"));
        assert!(matches!(first.load("a").blocks.last(), Some(Block::User(text)) if text == "from the first PC"));
    }

    #[test]
    fn a_running_chat_is_left_alone_until_its_turn_ends() {
        let store = tempfile::tempdir().expect("store");
        let (mut first, mut second) = two_pcs(&store);
        let (here, _there) = (first.folder("code/app"), second.folder("code/app"));
        first.save("a", &chat("Plan", &here, None, "claude:opus"));
        first.round(&opens(&["a"]), vec![]);
        second.round(&[], vec![]);
        let mut theirs = second.load("a");
        theirs.title = "Renamed elsewhere".into();
        second.save("a", &theirs);
        second.round(&opens(&["a"]), vec![]);

        let running = [Open { id: "a".into(), running: true }];
        let during = first.syncer.cycle(&running, vec![]);
        assert!(during.incoming.is_empty(), "{during:?}");
    }

    #[test]
    fn other_pcs_see_that_a_chat_is_still_running_and_when_it_stops() {
        let store = tempfile::tempdir().expect("store");
        let (mut first, mut second) = two_pcs(&store);
        let (here, _there) = (first.folder("code/app"), second.folder("code/app"));
        first.save("a", &chat("Plan", &here, None, "claude:opus"));
        first.round(&[Open { id: "a".into(), running: true }], vec![]);

        let during = second.round(&[], vec![]);
        assert!(matches!(&during.incoming[0], Incoming::Apply { running_on: Some(device), .. } if device == "PC"));

        first.round(&opens(&["a"]), vec![]);
        let after = second.round(&opens(&["a"]), vec![]);
        assert!(matches!(&after.incoming[0], Incoming::Apply { running_on: None, .. }), "{after:?}");
    }

    #[test]
    fn closing_a_chat_closes_it_elsewhere_and_reopening_brings_it_back() {
        let store = tempfile::tempdir().expect("store");
        let (mut first, mut second) = two_pcs(&store);
        let (here, _there) = (first.folder("code/app"), second.folder("code/app"));
        first.save("a", &chat("Plan", &here, None, "claude:opus"));
        first.round(&opens(&["a"]), vec![]);
        second.round(&[], vec![]);

        first.round(&[], vec!["a".into()]);
        let outcome = second.round(&opens(&["a"]), vec![]);
        assert!(matches!(outcome.incoming.as_slice(), [Incoming::Close { .. }]), "{outcome:?}");

        assert_eq!(first.round(&opens(&["a"]), vec![]).pushed, 1, "reopening must reach other PCs");
        assert!(matches!(second.round(&[], vec![]).incoming.as_slice(), [Incoming::Apply { .. }]));
    }

    #[test]
    fn the_store_only_holds_ciphertext() {
        let store = tempfile::tempdir().expect("store");
        let (mut first, _second) = two_pcs(&store);
        let here = first.folder("code/app");
        write_file(&first.claude_file(&here, "s1"), &"transcript-secret ".repeat(20));
        first.save("a", &chat("Top-secret-title", &here, Some("s1"), "claude:opus"));
        first.round(&opens(&["a"]), vec![]);

        let files: Vec<_> = std::fs::read_dir(store.path()).expect("dir").flatten().collect();
        assert!(files.len() >= 3, "{files:?}");
        for file in files {
            let name = file.file_name().to_string_lossy().to_string();
            let bytes = std::fs::read(file.path()).expect("read");
            let text = String::from_utf8_lossy(&bytes);
            for secret in ["Top-secret-title", "pineapple", "transcript-secret", "s1"] {
                assert!(!name.contains(secret) && !(secret.len() > 2 && text.contains(secret)), "{secret} in {name}");
            }
        }
    }

    #[test]
    fn a_tampered_or_renamed_file_is_refused() {
        let store = tempfile::tempdir().expect("store");
        let (mut first, mut second) = two_pcs(&store);
        let here = first.folder("code/app");
        second.folder("code/app");
        first.save("a", &chat("Plan", &here, None, "claude:opus"));
        first.round(&opens(&["a"]), vec![]);
        let original = store.path().join(thread_file("a"));

        std::fs::copy(&original, store.path().join(thread_file("b"))).expect("copy");
        let mut bytes = std::fs::read(&original).expect("read");
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&original, bytes).expect("tamper");

        let outcome = second.round(&[], vec![]);
        assert!(outcome.incoming.is_empty(), "{outcome:?}");
        assert_eq!(outcome.unreadable, 2);
    }

    #[test]
    fn joining_needs_the_right_code() {
        let store = tempfile::tempdir().expect("store");
        let (_first, code) = pc(store.path(), None);
        let attempt = |code: Option<String>| {
            let home = tempfile::tempdir().expect("home");
            let layout = Layout { user_home: home.path().to_path_buf(), olympus_home: home.path().join(".olympus") };
            connect(Box::new(FolderStore(store.path().to_path_buf())), layout, "s", code, "PC".into()).map(|_| ())
        };
        assert_eq!(attempt(None), Err(ConnectError::NeedsCode));
        assert!(matches!(attempt(Some(new_code())), Err(ConnectError::Problem(_))));
        assert!(matches!(attempt(Some("short".into())), Err(ConnectError::Problem(_))));
        assert_eq!(attempt(Some(code.to_lowercase())), Ok(()), "codes are forgiving about case");
    }

    #[test]
    fn a_hostile_record_cant_write_outside_the_transcript_folders() {
        let store = tempfile::tempdir().expect("store");
        let (first, mut second) = two_pcs(&store);
        let here = second.folder("code/app");
        let evil_blob = first.syncer.blob_file("evil");
        for (session, rel, kind) in [("..\\..\\evil", "a.jsonl", Kind::Claude), ("ok", "..\\..\\evil.jsonl", Kind::Codex), ("ok", "C:\\evil.jsonl", Kind::Codex)] {
            let transcript = Transcript { kind, session: session.into(), rel: rel.into(), blob: evil_blob.clone(), sha: "x".into() };
            let record = Record { version: "v".into(), device: "PC".into(), home: here.clone(), running: false, closed: false, state: Some(chat("Bad", &here, Some("ok"), "claude:opus")), transcript: Some(transcript) };
            let plain = serde_json::to_vec(&record).expect("json");
            let name = thread_file("evil");
            std::fs::write(store.path().join(&name), first.syncer.sealer.seal(&name, &plain).expect("seal")).expect("write");
            let outcome = second.round(&[], vec![]);
            assert!(outcome.incoming.is_empty(), "{session} {rel}: {outcome:?}");
            assert_eq!(outcome.unreadable, 1);
        }
    }

    #[test]
    fn a_chat_from_a_missing_folder_says_so_and_a_missing_transcript_carries_context() {
        let store = tempfile::tempdir().expect("store");
        let (mut first, mut second) = two_pcs(&store);
        let here = first.folder("only/here");
        first.save("a", &chat("Plan", &here, Some("gone"), "claude:opus"));
        first.round(&opens(&["a"]), vec![]);
        // Both pretend PCs share this disk, so the folder has to go for it to count as missing.
        std::fs::remove_dir_all(&here).expect("remove");
        second.round(&[], vec![]);

        let arrived = second.load("a");
        assert_eq!(arrived.cwd, second.home.path());
        assert!(arrived.blocks.iter().any(|block| matches!(block, Block::Note(text) if text.contains("isn't on this PC"))));
        assert_eq!(arrived.vendor_session, None);
        assert!(arrived.carry_context);
    }

    #[test]
    fn codex_and_hosted_transcripts_go_where_those_providers_look() {
        let store = tempfile::tempdir().expect("store");
        let (mut first, mut second) = two_pcs(&store);
        let (here, _there) = (first.folder("code/app"), second.folder("code/app"));
        write_file(&first.home.path().join(".codex/sessions/2026/11/05/rollout-x-c1.jsonl"), "codex\n");
        write_file(&first.home.path().join(".olympus/oa-sessions/o1.json"), "[]");
        first.save("c", &chat("Codex", &here, Some("c1"), "codex:gpt-6-luna@low"));
        first.save("o", &chat("Router", &here, Some("o1"), "openrouter:some/model"));
        first.round(&opens(&["c", "o"]), vec![]);
        second.round(&[], vec![]);

        assert_eq!(std::fs::read_to_string(second.home.path().join(".codex/sessions/2026/11/05/rollout-x-c1.jsonl")).expect("codex"), "codex\n");
        assert_eq!(std::fs::read_to_string(second.home.path().join(".olympus/oa-sessions/o1.json")).expect("hosted"), "[]");
    }

    #[test]
    fn an_account_this_pc_doesnt_have_falls_back_to_the_default_one() {
        let store = tempfile::tempdir().expect("store");
        let (mut first, mut second) = two_pcs(&store);
        let here = first.folder("code/app");
        second.folder("code/app");
        let mut with_accounts = chat("Plan", &here, None, "claude:opus");
        with_accounts.account_dir = Some(first.folder(".claude-work"));
        with_accounts.codex_account_dir = Some(first.folder(".codex-work"));
        let also_there = second.folder(".codex-work");
        first.save("a", &with_accounts);
        first.round(&opens(&["a"]), vec![]);
        second.round(&[], vec![]);

        let arrived = second.load("a");
        assert_eq!(arrived.account_dir, None);
        assert_eq!(arrived.codex_account_dir, Some(also_there));
    }

    #[test]
    fn a_longer_transcript_here_is_never_overwritten() {
        let store = tempfile::tempdir().expect("store");
        let (mut first, mut second) = two_pcs(&store);
        let (here, there) = (first.folder("code/app"), second.folder("code/app"));
        write_file(&first.claude_file(&here, "s1"), "one\n");
        write_file(&second.claude_file(&there, "s1"), "one\nadded in a terminal\n");
        first.save("a", &chat("Plan", &here, Some("s1"), "claude:opus"));
        first.round(&opens(&["a"]), vec![]);
        second.round(&[], vec![]);

        assert_eq!(std::fs::read_to_string(second.claude_file(&there, "s1")).expect("read"), "one\nadded in a terminal\n");
    }

    #[test]
    fn it_reports_a_store_that_went_away_and_recovers() {
        let store = tempfile::tempdir().expect("store");
        let (mut first, _second) = two_pcs(&store);
        let here = first.folder("code/app");
        first.save("a", &chat("Plan", &here, None, "claude:opus"));
        let moved = store.path().with_extension("moved");
        std::fs::rename(store.path(), &moved).expect("unplug");

        assert!(first.syncer.cycle(&opens(&["a"]), vec![]).error.is_some());

        std::fs::rename(&moved, store.path()).expect("plug in");
        assert_eq!(first.round(&opens(&["a"]), vec![]).pushed, 1);
    }

    #[test]
    fn places_and_codes_are_told_apart() {
        for remote in ["gdrive:Olympus", "s3:bucket/path", "box:"] {
            assert!(is_remote(remote), "{remote}");
        }
        for folder in ["C:\\Users\\me\\OneDrive", "D:/sync", "\\\\nas\\share\\a:b", "relative\\folder"] {
            assert!(!is_remote(folder), "{folder}");
        }
        let code = new_code();
        assert!(is_code(&code) && code.contains('-'));
        assert_ne!(code, new_code());
        assert_eq!(parse_connect("C:\\My Drive\\Sync"), Some(("C:\\My Drive\\Sync".into(), None)));
        assert_eq!(parse_connect(&format!("C:\\My Drive\\Sync {code}")), Some(("C:\\My Drive\\Sync".into(), Some(code.clone()))));
        assert_eq!(parse_connect(&format!("\"C:\\My Drive\" {code}")), Some(("C:\\My Drive".into(), Some(code))));
        assert_eq!(parse_connect("gdrive:Olympus"), Some(("gdrive:Olympus".into(), None)));
        assert_eq!(parse_connect("  "), None);
    }

    #[test]
    fn a_missing_rclone_says_how_to_get_it() {
        let store = RcloneStore { program: PathBuf::from("olympus-no-such-program"), remote: "gdrive:Olympus".into() };
        assert!(store.prepare().expect_err("missing").contains("winget install Rclone.Rclone"));
    }

    #[test]
    fn a_chat_stops_counting_as_running_when_its_pc_goes_quiet() {
        assert!(is_fresh(Instant::now()));
        if let Some(long_ago) = Instant::now().checked_sub(RUNNING_GOES_STALE + Duration::from_secs(1)) {
            assert!(!is_fresh(long_ago));
        }
    }

    // Set OLYMPUS_RCLONE_EXE to a real rclone. Uses its plain-folder backend, so it needs no account.
    #[test]
    #[ignore]
    fn the_rclone_connector_passes_chats_between_pcs() {
        let program = PathBuf::from(std::env::var_os("OLYMPUS_RCLONE_EXE").expect("OLYMPUS_RCLONE_EXE"));
        let store = tempfile::tempdir().expect("store");
        let remote = format!(":local:{}", store.path().join("remote").display()).replace('\\', "/");
        let open = |code: Option<String>| {
            let home = tempfile::tempdir().expect("home");
            let layout = Layout { user_home: home.path().to_path_buf(), olympus_home: home.path().join(".olympus") };
            std::fs::create_dir_all(&layout.olympus_home).expect("olympus");
            let rclone = RcloneStore { program: program.clone(), remote: remote.clone() };
            let (syncer, settings) = connect(Box::new(rclone), layout, "rclone-test:", code, "PC".into()).expect("connect");
            (Pc { home, syncer }, settings.code)
        };
        let (mut first, code) = open(None);
        let (mut second, _) = open(Some(code));
        let (here, there) = (first.folder("code/app"), second.folder("code/app"));
        write_file(&first.claude_file(&here, "s1"), "real rclone transcript\n");
        first.save("a", &chat("Plan", &here, Some("s1"), "claude:opus"));

        assert_eq!(first.round(&opens(&["a"]), vec![]).pushed, 1);
        assert_eq!(second.round(&[], vec![]).incoming.len(), 1);

        assert_eq!(std::fs::read_to_string(second.claude_file(&there, "s1")).expect("transcript"), "real rclone transcript\n");
        assert_eq!(first.round(&opens(&["a"]), vec![]).pushed, 0);
    }

    fn real_runtime() -> std::sync::Arc<olympus_core::runtime::Runtime> {
        let mut rt = olympus_core::runtime::Runtime::load().expect("runtime");
        rt.learn = None;
        rt.rules = String::new();
        rt.self_exe = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/olympus.exe");
        assert!(rt.self_exe.exists(), "build the olympus binary first");
        std::sync::Arc::new(rt)
    }

    // A second signed-in account stands in for the other PC: nothing there has seen this chat before.
    async fn real_chat_continues_elsewhere(spec: &str, other_account: &str, source_folder: &str) {
        let rt = real_runtime();
        let user = olympus_core::config::user_home();
        let other_account = user.join(other_account);
        assert!(other_account.is_dir(), "needs {}", other_account.display());
        let store = tempfile::tempdir().expect("store");
        let scratch = tempfile::tempdir_in(&user).expect("scratch");
        let here = scratch.path().join("code").join("project");
        std::fs::create_dir_all(&here).expect("project");

        let mut first = olympus_core::agent::TurnRequest::new(Target::parse(spec).expect("target"), "Remember the code word tangerine. Reply with only: ok", here.clone());
        first.use_memory = false;
        let (_keep, cancel) = olympus_core::proc::cancel_pair();
        let result = olympus_core::agent::run_turn(&rt, &first, cancel, &mut |_| {}).await;
        assert!(result.ok(), "{:?}", result.error);
        let session = result.session.clone().expect("session");

        let layout = Layout { user_home: user.clone(), olympus_home: scratch.path().join("first-olympus") };
        std::fs::create_dir_all(layout.olympus_home.join("sessions")).expect("sessions");
        let (mut from, settings) = connect(Box::new(FolderStore(store.path().to_path_buf())), layout.clone(), "s", None, "first".into()).expect("connect");
        let mut state = chat("Real", &here, Some(&session), spec);
        state.blocks.clear();
        state.blocks.push(Block::User("code word".into()));
        std::fs::write(layout.sessions().join("real.json"), serde_json::to_string(&state).expect("json")).expect("save");
        let pushed = from.cycle(&opens(&["real"]), vec![]);
        assert_eq!((pushed.pushed, pushed.error), (1, None));

        let other_home = tempfile::tempdir().expect("other home");
        let other_here = other_home.path().join(here.strip_prefix(&user).expect("under home"));
        std::fs::create_dir_all(&other_here).expect("project there");
        let other = Layout { user_home: other_home.path().to_path_buf(), olympus_home: other_home.path().join(".olympus") };
        std::fs::create_dir_all(&other.olympus_home).expect("olympus");
        let (mut to, _) = connect(Box::new(FolderStore(store.path().to_path_buf())), other, "s", Some(settings.code), "second".into()).expect("join");
        let outcome = to.cycle(&[], vec![]);
        let Some(Incoming::Apply { state: arrived, .. }) = outcome.incoming.first() else { panic!("nothing arrived: {outcome:?}") };
        assert_eq!(arrived.cwd, other_here);

        // Everything landed in the pretend home; the signed-in account takes it from there.
        let rel = other_home.path().join(source_folder);
        let mut placed = Vec::new();
        let mut stack = vec![rel.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("dir").flatten() {
                if entry.path().is_dir() { stack.push(entry.path()) } else { placed.push(entry.path()) }
            }
        }
        assert_eq!(placed.len(), 1, "{placed:?}");
        let inside = placed[0].strip_prefix(other_home.path().join(source_folder)).expect("inside").to_path_buf();
        let destination = other_account.join(Path::new(source_folder).file_name().expect("name")).join(inside);
        std::fs::create_dir_all(destination.parent().expect("parent")).expect("dirs");
        std::fs::copy(&placed[0], &destination).expect("copy");

        let mut next = olympus_core::agent::TurnRequest::new(Target::parse(spec).expect("target"), "What code word did I ask you to remember? Reply with only that word.", other_here);
        next.use_memory = false;
        next.resume = Some(session.clone());
        next.account_dir = Some(other_account);
        let (_keep, cancel) = olympus_core::proc::cancel_pair();
        let answer = olympus_core::agent::run_turn(&rt, &next, cancel, &mut |_| {}).await;
        let _ = std::fs::remove_file(&destination);
        assert!(answer.ok(), "{:?}", answer.error);
        assert!(answer.text.to_lowercase().contains("tangerine"), "lost the memory: {}", answer.text);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn a_real_claude_chat_continues_on_another_account_in_another_folder() {
        real_chat_continues_elsewhere("claude:haiku", ".claude-account2", ".claude/projects").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn a_real_codex_chat_continues_on_another_account_in_another_folder() {
        real_chat_continues_elsewhere("codex:gpt-6-luna@low", ".codex-account2", ".codex/sessions").await;
    }
}
