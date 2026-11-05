use std::mem::size_of;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

const RULE_NAME: &str = "Olympus Node";

/// Makes sure allow rules exist for this exe, asking for elevation at most once.
pub fn ensure_rules(exe: &Path, tcp_port: u16, udp_port: u16) {
    if rules_match(exe, tcp_port, udp_port) {
        return;
    }
    let script = rule_script(exe, tcp_port, udp_port);
    println!("Adding Windows Firewall rules so other computers can reach this node. Windows may ask for permission.");
    match run_elevated(&script) {
        Ok(()) if rules_match(exe, tcp_port, udp_port) => println!("Firewall rules added."),
        Ok(()) => print_manual(&script, "the rules were not found after adding them"),
        Err(e) => print_manual(&script, &format!("{e:#}")),
    }
}

fn print_manual(script: &str, reason: &str) {
    println!("Could not add the firewall rules: {reason}.");
    println!("Other computers may not reach this node. To fix it, run this in an administrator prompt:");
    println!("  {script}");
}

fn rules_match(exe: &Path, tcp_port: u16, udp_port: u16) -> bool {
    let output = Command::new("netsh")
        .args(["advfirewall", "firewall", "show", "rule"])
        .arg(format!("name={RULE_NAME}"))
        .arg("verbose")
        .output();
    match output {
        Ok(out) if out.status.success() => {
            show_output_matches(&String::from_utf8_lossy(&out.stdout), exe, tcp_port, udp_port)
        }
        _ => false,
    }
}

// Labels in netsh output are localized and its text is OEM-encoded, so match values only, ASCII only.
fn show_output_matches(show: &str, exe: &Path, tcp_port: u16, udp_port: u16) -> bool {
    let show = ascii_lower(show);
    let exe = ascii_lower(&exe.to_string_lossy());
    let has_port = |port: u16| show.lines().any(|line| line.trim_end().ends_with(&format!(" {port}")));
    !exe.is_empty() && show.matches(&exe).count() >= 2 && has_port(tcp_port) && has_port(udp_port)
}

fn ascii_lower(text: &str) -> String {
    text.chars().filter(char::is_ascii).map(|c| c.to_ascii_lowercase()).collect()
}

fn rule_script(exe: &Path, tcp_port: u16, udp_port: u16) -> String {
    let exe = exe.display();
    let add = |proto: &str, port: u16| {
        format!(
            "netsh advfirewall firewall add rule name=\"{RULE_NAME}\" dir=in action=allow program=\"{exe}\" \
             protocol={proto} localport={port} remoteip=localsubnet profile=any"
        )
    };
    format!(
        "netsh advfirewall firewall delete rule name=\"{RULE_NAME}\" & {} && {}",
        add("TCP", tcp_port),
        add("UDP", udp_port)
    )
}

fn run_elevated(script: &str) -> Result<()> {
    let params = HSTRING::from(format!("/c \"{script}\""));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: w!("cmd.exe"),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info) }.context("the elevation prompt was declined or failed")?;
    let process = info.hProcess;
    if process.is_invalid() {
        bail!("Windows did not return the elevated process");
    }
    let mut code = 1u32;
    unsafe {
        WaitForSingleObject(process, INFINITE);
        let exit = GetExitCodeProcess(process, &mut code);
        CloseHandle(process).ok();
        exit.context("read the elevated process's result")?;
    }
    if code != 0 {
        bail!("netsh exited with code {code}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const SHOW: &str = "Rule Name:   Olympus Node\n\
        Program:   C:\\Tools\\olympus-node.exe\nLocalPort:   47475\n\n\
        Rule Name:   Olympus Node\n\
        Program:   C:\\Tools\\olympus-node.exe\nLocalPort:   47474\n";

    #[test]
    fn show_output_matches_this_exe_and_both_ports() {
        assert!(show_output_matches(SHOW, &PathBuf::from(r"c:\tools\OLYMPUS-NODE.exe"), 47475, 47474));
    }

    #[test]
    fn a_moved_exe_or_changed_port_needs_new_rules() {
        assert!(!show_output_matches(SHOW, &PathBuf::from(r"D:\New\olympus-node.exe"), 47475, 47474));
        assert!(!show_output_matches(SHOW, &PathBuf::from(r"C:\Tools\olympus-node.exe"), 9000, 47474));
    }

    #[test]
    fn script_deletes_stale_rules_then_adds_both() {
        let script = rule_script(&PathBuf::from(r"C:\My Tools\n.exe"), 47475, 47474);
        assert!(script.starts_with("netsh advfirewall firewall delete rule name=\"Olympus Node\" & "));
        assert!(script.contains("program=\"C:\\My Tools\\n.exe\" protocol=TCP localport=47475 remoteip=localsubnet profile=any"));
        assert!(script.contains("protocol=UDP localport=47474 remoteip=localsubnet profile=any"));
    }
}
