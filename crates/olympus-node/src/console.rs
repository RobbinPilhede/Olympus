use std::io::{self, BufRead, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::runtime::Handle;
use windows::Win32::System::Console::{
    GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
};

use crate::hf::{Fit, QuantFiles, SearchHit};
use crate::node::{Node, StartError};
use crate::store::LocalModel;

const HELP: &str = "Commands:
  search <text>   find GGUF models on Hugging Face
  files <n>       list the quants of result n from the last search
  pull <n>        download quant n from the last files list
  list            show downloaded models
  load <n>        run model n from the last list
  unload          stop the model and free its memory
  status          show what the node is doing
  help            show this list
  quit            stop the node";

const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

/// Turns on ANSI colour; returns false when stdout is not a console.
pub fn enable_colour() -> bool {
    unsafe {
        let Ok(handle) = GetStdHandle(STD_OUTPUT_HANDLE) else {
            return false;
        };
        let mut mode = Default::default();
        if GetConsoleMode(handle, &mut mode).is_err() {
            return false;
        }
        SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING).is_ok()
    }
}

/// ANSI styling that collapses to nothing when colour is off.
#[derive(Clone, Copy)]
pub struct Paint {
    pub on: bool,
}

impl Paint {
    /// Wraps text in an SGR code.
    pub fn code(self, code: &str, text: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    /// Green, for success.
    pub fn good(self, text: &str) -> String {
        self.code("32", text)
    }

    /// Red, for failures.
    pub fn bad(self, text: &str) -> String {
        self.code("31", text)
    }

    /// Bold, for headings.
    pub fn bold(self, text: &str) -> String {
        self.code("1", text)
    }
}

/// Formats bytes for people.
pub fn human_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let value = bytes as f64;
    if value >= GIB {
        format!("{:.1} GB", value / GIB)
    } else {
        format!("{:.0} MB", value / MIB)
    }
}

#[derive(Default)]
struct Session {
    hits: Vec<SearchHit>,
    repo: String,
    quants: Vec<QuantFiles>,
    local: Vec<LocalModel>,
}

/// Reads commands from stdin until it closes or the user quits; the node keeps serving after EOF.
pub fn run_repl(node: Arc<Node>, rt: Handle, paint: Paint) {
    let mut session = Session::default();
    println!("Type a command, or help for the list. Start with: search qwen");
    prompt();
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else {
            break;
        };
        let line = line.trim();
        if line == "quit" || line == "exit" {
            println!("Stopping.");
            std::process::exit(0);
        }
        if !line.is_empty() {
            rt.block_on(command(&node, &mut session, line, paint));
        }
        prompt();
    }
    println!("Input closed. The node keeps serving; close this window to stop it.");
}

fn prompt() {
    print!("> ");
    io::stdout().flush().ok();
}

async fn command(node: &Arc<Node>, session: &mut Session, line: &str, paint: Paint) {
    let (name, arg) = line.split_once(char::is_whitespace).map_or((line, ""), |(n, a)| (n, a.trim()));
    match name {
        "help" => println!("{HELP}"),
        "search" => search(node, session, arg, paint).await,
        "files" => files(node, session, arg, paint).await,
        "pull" => pull(node, session, arg, paint).await,
        "list" => list(node, session),
        "load" => load(node, session, arg, paint),
        "unload" => {
            node.unload().await;
            println!("Model stopped.");
        }
        "status" => status(node),
        _ => println!("Unknown command: {name}. Type help for the list."),
    }
}

fn pick<'a, T>(items: &'a [T], arg: &str, what: &str, how: &str) -> Option<&'a T> {
    let item = arg.parse::<usize>().ok().and_then(|n| items.get(n.checked_sub(1)?));
    if item.is_none() {
        println!("Pick a {what} by its number. Run {how} to see the numbers.");
    }
    item
}

async fn search(node: &Node, session: &mut Session, query: &str, paint: Paint) {
    if query.is_empty() {
        println!("Type what to search for, like: search qwen");
        return;
    }
    match node.hf.search(query).await {
        Ok(hits) if hits.is_empty() => println!("Nothing found for {query}. Try a shorter name."),
        Ok(hits) => {
            for (i, hit) in hits.iter().enumerate() {
                let arch = hit.architecture.as_deref().unwrap_or("unknown");
                let label = if hit.supported { paint.good(arch) } else { paint.bad(&format!("{arch}, not supported")) };
                println!("{:>3}. {}  [{label}]  {} downloads", i + 1, hit.repo, hit.downloads);
            }
            println!("Next: files <n>");
            session.hits = hits;
        }
        Err(e) => println!("{} {e:#}", paint.bad("Search failed:")),
    }
}

async fn files(node: &Node, session: &mut Session, arg: &str, paint: Paint) {
    let Some(repo) = pick(&session.hits, arg, "search result", "search <text>").map(|h| h.repo.clone()) else {
        return;
    };
    match node.quant_files(&repo).await {
        Ok(quants) if quants.is_empty() => println!("{repo} has no complete GGUF model files."),
        Ok(quants) => {
            for (i, q) in quants.iter().enumerate() {
                let fit = match q.fit {
                    Fit::Gpu => paint.good("fits GPU"),
                    Fit::Cpu => "CPU only".to_string(),
                    Fit::None => paint.bad("too big"),
                };
                let mark = if q.recommended { "  recommended" } else { "" };
                println!("{:>3}. {:<10} {:>9}  {fit}{mark}", i + 1, q.quant, human_bytes(q.size));
            }
            println!("Next: pull <n>");
            session.repo = repo;
            session.quants = quants;
        }
        Err(e) => println!("{} {e:#}", paint.bad("Could not list files:")),
    }
}

async fn pull(node: &Arc<Node>, session: &Session, arg: &str, paint: Paint) {
    let Some(quant) = pick(&session.quants, arg, "quant", "files <n>") else {
        return;
    };
    match node.start_pull(session.repo.clone(), quant.quant.clone(), None).await {
        Ok(()) => println!("Downloading {} {}. Progress prints here every few seconds.", session.repo, quant.quant),
        Err(StartError::Busy(m) | StartError::NotFound(m) | StartError::BadRequest(m) | StartError::Upstream(m)) => {
            println!("{} {m}", paint.bad("Could not start the download:"));
        }
    }
}

fn list(node: &Node, session: &mut Session) {
    session.local = node.store.list();
    if session.local.is_empty() {
        println!("No models yet. Use search, files and pull to get one.");
        return;
    }
    for (i, model) in session.local.iter().enumerate() {
        println!("{:>3}. {} {}  {}", i + 1, model.repo, model.quant, human_bytes(model.size));
    }
    println!("Next: load <n>");
}

fn load(node: &Arc<Node>, session: &Session, arg: &str, paint: Paint) {
    let Some(model) = pick(&session.local, arg, "model", "list") else {
        return;
    };
    match node.start_load(&model.id, None, None) {
        Ok(()) => println!("Loading {} {}.", model.repo, model.quant),
        Err(StartError::Busy(m) | StartError::NotFound(m) | StartError::BadRequest(m) | StartError::Upstream(m)) => {
            println!("{} {m}", paint.bad("Could not load:"));
        }
    }
}

fn status(node: &Node) {
    let status = node.status();
    println!("State: {}", status.discovery.state);
    println!("Model: {}", status.discovery.model.as_deref().unwrap_or("none"));
    if let Some(d) = &status.download {
        println!("Download: {}", download_line(d.done, d.total, d.bytes_per_sec, &d.file));
    }
    if let Some(error) = &status.error {
        println!("Last error: {error}");
    }
}

fn download_line(done: u64, total: u64, rate: u64, file: &str) -> String {
    let percent = done.checked_mul(100).and_then(|d| d.checked_div(total)).unwrap_or(0);
    format!("{file}  {percent}%  {} of {}  {}/s", human_bytes(done), human_bytes(total), human_bytes(rate))
}

/// Prints state changes and throttled download progress as they happen.
pub async fn watch_state(node: Arc<Node>, paint: Paint) {
    let mut last_state = "idle";
    let mut last_progress = Instant::now();
    let mut was_downloading = false;
    let mut interval = tokio::time::interval(Duration::from_millis(500));
    loop {
        interval.tick().await;
        let status = node.status();
        let state = status.discovery.state;
        if state != last_state {
            last_state = state;
            match state {
                "ready" => println!("\n{} {}", paint.good("Model ready:"), status.discovery.model.as_deref().unwrap_or("")),
                "loading" => println!("\nLoading the model into memory."),
                "error" => println!("\n{} {}", paint.bad("Error:"), status.error.as_deref().unwrap_or("unknown")),
                _ => {}
            }
        }
        match &status.download {
            Some(d) if last_progress.elapsed() >= PROGRESS_INTERVAL && d.total > 0 => {
                last_progress = Instant::now();
                println!("{}", download_line(d.done, d.total, d.bytes_per_sec, &d.file));
            }
            Some(_) => {}
            None if was_downloading => println!("\nDownload finished or stopped. Type list to see your models."),
            None => {}
        }
        was_downloading = status.download.is_some();
    }
}
