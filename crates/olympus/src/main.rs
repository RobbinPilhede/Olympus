//! Olympus: a desktop orchestrator for Claude Code, Codex, OpenRouter and LAN models.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod app;
mod capture;
mod commands;
mod composer;
mod diff;
mod gui;
mod hotkeys;
mod input;
mod markdown;
mod notify;
mod pill;
mod pty;
mod schedule;
mod session;
mod telegram;
mod theme;
mod update;
mod voice;

use anyhow::Result;
use olympus_core::config::{home_dir, user_home};
use olympus_core::runtime::Runtime;
use std::path::PathBuf;
use std::sync::Arc;
use windows::core::HSTRING;
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

fn show_fatal(message: &str) {
    let _ = std::fs::write(home_dir().join("crash.log"), message);
    let message = message.to_string();
    // A message box on this thread would pump the window's messages back into the panicking app.
    let dialog = std::thread::spawn(move || {
        // SAFETY: both strings outlive the call, and a null owner is allowed.
        unsafe { MessageBoxW(None, &HSTRING::from(message), &HSTRING::from("Olympus"), MB_OK | MB_ICONERROR) };
    });
    let _ = dialog.join();
}

fn open_window(folder: Option<PathBuf>) -> Result<()> {
    std::panic::set_hook(Box::new(|info| show_fatal(&format!("Olympus stopped unexpectedly.\n\n{info}"))));
    let tokio = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let _guard = tokio.enter();
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from));
    let launched = std::env::current_dir().unwrap_or_else(|_| user_home());
    // Double-clicking the exe starts in its own folder, which is never the project.
    let cwd = folder.unwrap_or(if Some(&launched) == exe_dir.as_ref() { user_home() } else { launched });
    let exe = std::env::current_exe()?;
    // Agents inherit this environment, and a relaunch only announces itself once.
    let welcome = std::env::var(update::WELCOME_VAR).ok();
    std::env::remove_var(update::WELCOME_VAR);
    let job = welcome.is_none().then(|| update::job_for(&exe, &update::install_dir())).flatten();
    // Finding the CLIs takes a moment, and the splash shouldn't wait for it.
    let loading = tokio.spawn_blocking(|| Runtime::load().map(Arc::new));
    let handle = tokio.handle().clone();
    gui::run(move || handle.block_on(loading)?, cwd, job, welcome);
    Ok(())
}

fn main() -> Result<()> {
    let first = std::env::args().nth(1);
    if first.is_some() {
        // SAFETY: takes no pointers; failing only means there's no console to print to.
        let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }
    match first.as_deref() {
        Some("mcp") => {
            let tokio = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            tokio.block_on(olympus_core::mcp::serve_stdio(Arc::new(Runtime::load()?)))
        }
        Some("--version" | "-V") => {
            println!("olympus {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--help" | "-h") => {
            println!("Olympus {}\n\n  olympus             Open Olympus here\n  olympus <folder>    Open Olympus in a folder\n  olympus mcp         Serve the model bridge over stdio (agents start this themselves)\n  olympus --uninstall Remove the installed app and keep your data", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--uninstall") => update::uninstall(),
        Some("guard") => std::process::exit(olympus_core::guard::run_hook()),
        Some(other) if !PathBuf::from(other).is_dir() => {
            eprintln!("{other} isn't a folder. Run olympus --help.");
            std::process::exit(2);
        }
        other => {
            let folder = other.and_then(|path| std::path::absolute(path).ok());
            if let Err(error) = open_window(folder) {
                show_fatal(&format!("Olympus couldn't start.\n\n{error:#}"));
                std::process::exit(1);
            }
            Ok(())
        }
    }
}
