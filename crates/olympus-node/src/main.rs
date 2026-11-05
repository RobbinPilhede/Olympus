mod api;
mod console;
mod download;
mod engine;
mod firewall;
mod hf;
mod hw;
mod node;
mod pirate;
mod store;

use std::io::BufRead;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use console::{human_bytes, Paint};
use engine::Engine;
use hf::Hf;
use node::{Node, NodeParts};
use store::Store;
use tokio::net::{TcpListener, UdpSocket};

const DISCOVERY_PORT: u16 = 47474;
const HTTP_PORT: u16 = 47475;
const DEFAULT_TOKEN: &str = "olympus";
const HF_BASE: &str = "https://huggingface.co";

struct Args {
    firewall: bool,
    port: u16,
    name: Option<String>,
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Args> {
    let mut parsed = Args { firewall: true, port: HTTP_PORT, name: None };
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--no-firewall" => parsed.firewall = false,
            "--port" => {
                let value = args.next().ok_or_else(|| anyhow!("--port needs a number"))?;
                parsed.port = value.parse().with_context(|| format!("--port {value} is not a port number"))?;
            }
            "--name" => parsed.name = Some(args.next().ok_or_else(|| anyhow!("--name needs a value"))?),
            other => bail!("Unknown option {other}. Options: --no-firewall, --port <n>, --name <text>."),
        }
    }
    Ok(parsed)
}

fn main() -> ExitCode {
    let colour = console::enable_colour();
    let paint = Paint { on: colour };
    match run(paint) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            println!("{} {e:#}", paint.bad("Olympus Node could not start:"));
            println!("Press Enter to close.");
            std::io::stdin().lock().lines().next();
            ExitCode::FAILURE
        }
    }
}

#[tokio::main]
async fn run(paint: Paint) -> Result<()> {
    let args = parse_args(std::env::args().skip(1))?;
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow!("the TLS provider was already installed"))?;

    let user_home = std::env::var_os("USERPROFILE").map(PathBuf::from).ok_or_else(|| anyhow!("USERPROFILE is not set"))?;
    let models_dir = user_home.join(".olympus").join("models");
    std::fs::create_dir_all(&models_dir).with_context(|| format!("create {}", models_dir.display()))?;

    if args.firewall {
        let exe = std::env::current_exe().context("find this program's path")?;
        tokio::task::spawn_blocking(move || firewall::ensure_rules(&exe, args.port, DISCOVERY_PORT))
            .await
            .context("firewall setup")?;
    }

    let udp = UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)).await.with_context(|| {
        format!("UDP port {DISCOVERY_PORT} is in use. Another Olympus Node may be running; close it and start this one again")
    })?;
    let tcp = TcpListener::bind(("0.0.0.0", args.port)).await.with_context(|| {
        format!("TCP port {} is in use. Another Olympus Node may be running; close it and start this one again", args.port)
    })?;

    let hf_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(60))
        .build()
        .context("build the Hugging Face client")?;
    let proxy_client = reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(0)
        .build()
        .context("build the proxy client")?;
    let hf = Hf {
        client: hf_client,
        base: HF_BASE.to_string(),
        token: std::env::var("HF_TOKEN").ok().filter(|t| !t.is_empty()),
    };

    let node = Arc::new(Node::new(NodeParts {
        name: args.name.unwrap_or_else(|| std::env::var("COMPUTERNAME").unwrap_or_else(|_| "olympus-node".to_string())),
        port: args.port,
        token: std::env::var("OLYMPUS_NODE_TOKEN").ok().filter(|t| !t.is_empty()).unwrap_or_else(|| DEFAULT_TOKEN.to_string()),
        hardware: hw::detect(),
        hf,
        proxy: proxy_client,
        store: Store::new(models_dir),
        engine: Engine::new().context("prepare the inference engine")?,
    }));

    print_banner(&node, paint);
    auto_load(&node);

    tokio::spawn(api::run_discovery(udp, node.clone()));
    tokio::spawn(console::watch_state(node.clone(), paint));
    let repl_node = node.clone();
    let rt = tokio::runtime::Handle::current();
    std::thread::spawn(move || console::run_repl(repl_node, rt, paint));

    axum::serve(tcp, api::router(node)).await.context("the HTTP server stopped")
}

fn print_banner(node: &Node, paint: Paint) {
    println!("{}", paint.bold(&format!("Olympus Node {}", env!("CARGO_PKG_VERSION"))));
    println!("Name:    {}", node.name);
    let ips = hw::lan_ips();
    if ips.is_empty() {
        println!("Address: no network found, only this computer can connect. Check the network cable or Wi-Fi.");
    }
    for ip in ips {
        println!("Address: http://{ip}:{}", node.port);
    }
    match &node.hardware.gpu {
        Some(gpu) => println!("GPU:     {gpu}, {} VRAM", human_bytes(node.hardware.vram_mb * 1024 * 1024)),
        None => println!("GPU:     none found, models will run on the CPU"),
    }
    println!("RAM:     {}", human_bytes(node.hardware.ram_mb * 1024 * 1024));
    println!("Models:  {}", node.store.root().display());
    println!("Token:   {}", node.token);
    println!();
}

fn auto_load(node: &Arc<Node>) {
    let Some(last) = node.store.last_used() else {
        return;
    };
    if node.start_load(&last.id, last.ctx, last.parallel).is_ok() {
        println!("Loading the last used model: {}", last.id);
    } else {
        println!("The last used model {} is no longer on this computer. Type list to see what is.", last.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args> {
        parse_args(args.iter().map(|a| a.to_string()))
    }

    #[test]
    fn defaults_match_the_contract() {
        let args = parse(&[]).unwrap();
        assert_eq!(args.port, 47475);
        assert!(args.firewall);
        assert!(args.name.is_none());
    }

    #[test]
    fn flags_parse_and_unknown_flags_fail() {
        let args = parse(&["--no-firewall", "--port", "9000", "--name", "box"]).unwrap();
        assert!(!args.firewall);
        assert_eq!(args.port, 9000);
        assert_eq!(args.name.as_deref(), Some("box"));
        assert!(parse(&["--nope"]).is_err());
        assert!(parse(&["--port", "x"]).is_err());
    }
}
