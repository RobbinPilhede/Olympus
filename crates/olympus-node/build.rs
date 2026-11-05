use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn main() {
    embed_resource::compile("olympus.rc", embed_resource::NONE).manifest_optional().expect("embedding the icon");
    if let Err(e) = run() {
        panic!("building the vendored infr engine failed: {e}");
    }
}

fn run() -> Result<(), String> {
    let root = workspace_root()?;
    let vendor = root.join("vendor").join("infr");
    let tools = root.join(".tools").join("glslc");
    let glslc_bin = tools.join("Bin");
    let target = root.join("target").join("infr");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", vendor.join("Cargo.lock").display());

    if !glslc_bin.join("glslc.exe").is_file() {
        return Err(format!("{} is missing glslc.exe", glslc_bin.display()));
    }
    fs::create_dir_all(&target).map_err(|e| format!("create {}: {e}", target.display()))?;

    let log_path = target.join("build.log");
    let log = fs::File::create(&log_path).map_err(|e| format!("create log: {e}"))?;
    let log_err = log.try_clone().map_err(|e| format!("clone log: {e}"))?;

    let mut path = vec![glslc_bin.clone()];
    path.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    let path = env::join_paths(path).map_err(|e| format!("join PATH: {e}"))?;

    let cargo = env::var_os("CARGO").ok_or("CARGO is not set")?;
    let mut cmd = Command::new(cargo);
    cmd.args(["build", "--release", "--locked", "-v", "-p", "infr-cli"])
        .current_dir(&vendor)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err);
    for (name, _) in env::vars_os() {
        if is_inherited_build_var(&name) {
            cmd.env_remove(name);
        }
    }
    cmd.env("RUSTFLAGS", "-C target-cpu=x86-64-v3 -C target-feature=+crt-static")
        .env("CARGO_TARGET_DIR", &target)
        .env("CARGO_PROFILE_RELEASE_DEBUG", "0")
        .env("CARGO_PROFILE_RELEASE_STRIP", "true")
        .env("VULKAN_SDK", &tools)
        .env("PATH", path);

    let status = cmd.status().map_err(|e| format!("run cargo: {e}"))?;
    if !status.success() {
        return Err(format!("{status}\n{}", log_tail(&log_path)));
    }

    let built = target.join("release").join("infr.exe");
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?).join("infr.exe");
    fs::copy(&built, &out).map_err(|e| format!("copy {}: {e}", built.display()))?;
    Ok(())
}

fn workspace_root() -> Result<PathBuf, String> {
    let manifest = env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is not set")?;
    let real = fs::canonicalize(&manifest).map_err(|e| format!("canonicalize manifest dir: {e}"))?;
    let real = real.to_string_lossy();
    let plain = PathBuf::from(real.strip_prefix(r"\\?\").unwrap_or(&real));
    plain
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .ok_or_else(|| "the crate is not inside a workspace".to_string())
}

fn is_inherited_build_var(name: &OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_uppercase();
    if name == "CARGO_HOME" || name.starts_with("RUSTUP_") {
        return false;
    }
    name.starts_with("CARGO")
        || name.starts_with("RUST")
        || matches!(name.as_str(), "TARGET" | "HOST" | "PROFILE" | "OPT_LEVEL" | "DEBUG" | "NUM_JOBS" | "OUT_DIR")
}

fn log_tail(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(40)..].join("\n")
}
