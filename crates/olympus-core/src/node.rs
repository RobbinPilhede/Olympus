use crate::config::Config;
use crate::openai::client;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

const DISCOVERY_PORT: u16 = 47474;
const PROBE: &[u8] = b"OLYMPUS_DISCOVER";

/// A node's discovery reply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeInfo {
    pub name: String,
    pub port: u16,
    #[serde(default)]
    pub version: String,
    pub gpu: Option<String>,
    #[serde(default)]
    pub vram_mb: u64,
    #[serde(default)]
    pub ram_mb: u64,
    pub model: Option<String>,
    #[serde(default)]
    pub state: String,
    #[serde(skip)]
    pub host: String,
}

impl NodeInfo {
    /// `host:port` for HTTP calls.
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// One Hugging Face search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HfRepo {
    pub repo: String,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub likes: u64,
    pub architecture: Option<String>,
    #[serde(default)]
    pub supported: bool,
}

/// One model torrent listed on Pirate Face.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentHit {
    pub repo: String,
    pub size: String,
    #[serde(default)]
    pub seeds: u64,
    pub magnet: String,
}

/// One downloadable quantization of a repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HfQuant {
    pub quant: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub fit: String,
    #[serde(default)]
    pub recommended: bool,
}

/// A model already on a node's disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalModel {
    pub id: String,
    pub repo: String,
    pub quant: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default = "assume_verified")]
    pub verified: bool,
}

fn assume_verified() -> bool {
    true
}

/// Progress of a node's download.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Download {
    pub repo: String,
    #[serde(default)]
    pub done: u64,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub bytes_per_sec: u64,
}

/// A node's full status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStatus {
    pub name: String,
    pub model: Option<String>,
    #[serde(default)]
    pub state: String,
    pub error: Option<String>,
    pub download: Option<Download>,
    #[serde(default)]
    pub local: Vec<LocalModel>,
}

async fn local_broadcast_targets() -> Vec<SocketAddr> {
    let mut targets = vec![SocketAddr::from(([255, 255, 255, 255], DISCOVERY_PORT)), SocketAddr::from(([127, 0, 0, 1], DISCOVERY_PORT))];
    if let Ok(probe) = UdpSocket::bind("0.0.0.0:0").await {
        if probe.connect("8.8.8.8:53").await.is_ok() {
            if let Ok(SocketAddr::V4(local)) = probe.local_addr() {
                let [a, b, c, _] = local.ip().octets();
                targets.push(SocketAddr::from(([a, b, c, 255], DISCOVERY_PORT)));
            }
        }
    }
    targets
}

/// Finds nodes by UDP probe, plus any listed in config.
pub async fn discover(config: &Config, wait: Duration) -> Result<Vec<NodeInfo>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await.context("opening discovery socket")?;
    socket.set_broadcast(true).context("enabling broadcast")?;
    for target in local_broadcast_targets().await {
        // An unreachable interface shouldn't stop the probe on the others.
        let _ = socket.send_to(PROBE, target).await;
    }
    for address in config.nodes.values() {
        if let Some(host) = address.split(':').next() {
            let _ = socket.send_to(PROBE, (host, DISCOVERY_PORT)).await;
        }
    }
    let mut found: HashMap<String, NodeInfo> = HashMap::new();
    let deadline = Instant::now() + wait;
    let mut buffer = [0u8; 4096];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match tokio::time::timeout(left, socket.recv_from(&mut buffer)).await {
            Ok(Ok((n, peer))) => {
                if let Ok(mut info) = serde_json::from_slice::<NodeInfo>(&buffer[..n]) {
                    info.host = peer.ip().to_string();
                    let keep_existing = found.get(&info.name).is_some_and(|f| f.host != "127.0.0.1" && info.host == "127.0.0.1");
                    if !keep_existing {
                        found.insert(info.name.clone(), info);
                    }
                }
            }
            Ok(Err(_)) => continue,
            Err(_) => break,
        }
    }
    let mut nodes: Vec<NodeInfo> = found.into_values().collect();
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(nodes)
}

/// Resolves a node name to `host:port`, from config first, then a live probe.
pub async fn address(config: &Config, name: &str) -> Option<String> {
    if let Some(address) = config.nodes.get(name) {
        return Some(address.clone());
    }
    discover(config, Duration::from_millis(700)).await.ok()?.into_iter().find(|n| n.name.eq_ignore_ascii_case(name)).map(|n| n.address())
}

/// HTTP client for one node's control API.
const TORRENT_WAIT: Duration = Duration::from_secs(150);

#[derive(Debug, Clone)]
pub struct NodeClient {
    base: String,
    token: String,
}

impl NodeClient {
    /// Creates a client for `host:port`.
    pub fn new(address: &str, token: &str) -> Self {
        Self { base: format!("http://{address}"), token: token.to_string() }
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        self.get_within(path, query, Duration::from_secs(30)).await
    }

    async fn get_within(&self, path: &str, query: &[(&str, &str)], limit: Duration) -> Result<Value> {
        let response = client().get(format!("{}{path}", self.base)).bearer_auth(&self.token).query(query).timeout(limit).send().await.context("reaching the node")?;
        decode(response).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.post_within(path, body, Duration::from_secs(30)).await
    }

    async fn post_within(&self, path: &str, body: Value, limit: Duration) -> Result<Value> {
        let response = client().post(format!("{}{path}", self.base)).bearer_auth(&self.token).json(&body).timeout(limit).send().await.context("reaching the node")?;
        decode(response).await
    }

    /// Reads the node's state, including downloads and local models.
    pub async fn status(&self) -> Result<NodeStatus> {
        Ok(serde_json::from_value(self.get("/api/status", &[]).await?)?)
    }

    /// Searches Hugging Face for GGUF repos.
    pub async fn search(&self, query: &str) -> Result<Vec<HfRepo>> {
        Ok(serde_json::from_value(self.get("/api/hf/search", &[("q", query)]).await?)?)
    }

    /// Lists a repo's quantizations with their fit on this node.
    pub async fn files(&self, repo: &str) -> Result<Vec<HfQuant>> {
        Ok(serde_json::from_value(self.get("/api/hf/files", &[("repo", repo)]).await?)?)
    }

    /// Searches Pirate Face for model torrents.
    pub async fn torrent_search(&self, query: &str) -> Result<Vec<TorrentHit>> {
        Ok(serde_json::from_value(self.get("/api/pf/search", &[("q", query)]).await?)?)
    }

    /// Lists a torrent's quantizations; the swarm can take over a minute to answer.
    pub async fn torrent_files(&self, magnet: &str) -> Result<Vec<HfQuant>> {
        Ok(serde_json::from_value(self.get_within("/api/pf/files", &[("magnet", magnet)], TORRENT_WAIT).await?)?)
    }

    /// Starts downloading one quantization from a torrent.
    pub async fn pull_torrent(&self, repo: &str, quant: &str, magnet: &str) -> Result<()> {
        self.post_within("/api/pull", json!({"repo": repo, "quant": quant, "magnet": magnet}), TORRENT_WAIT).await.map(drop)
    }

    /// Starts downloading a quantization.
    pub async fn pull(&self, repo: &str, quant: &str) -> Result<()> {
        self.post("/api/pull", json!({"repo": repo, "quant": quant})).await.map(drop)
    }

    /// Loads a downloaded model, unloading whatever is running.
    pub async fn load(&self, id: &str) -> Result<()> {
        self.post("/api/load", json!({"id": id})).await.map(drop)
    }

    /// Unloads the running model.
    pub async fn unload(&self) -> Result<()> {
        self.post("/api/unload", json!({})).await.map(drop)
    }
}

async fn decode(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    let text = response.text().await.context("reading the node's reply")?;
    if !status.is_success() {
        let detail = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["error"].as_str().map(String::from)).unwrap_or(text);
        return Err(anyhow!("Node returned {status}: {detail}"));
    }
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).context("the node sent invalid JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn discovery_finds_a_loopback_responder_and_uses_its_address() {
        let responder = match UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)).await {
            Ok(socket) => socket,
            Err(_) => return,
        };
        tokio::spawn(async move {
            let mut buffer = [0u8; 64];
            while let Ok((n, peer)) = responder.recv_from(&mut buffer).await {
                if &buffer[..n] == PROBE {
                    let reply = json!({"name": "box", "port": 47475, "gpu": null, "model": null, "state": "idle"}).to_string();
                    let _ = responder.send_to(reply.as_bytes(), peer).await;
                }
            }
        });
        let nodes = discover(&Config::default(), Duration::from_millis(500)).await.expect("discover");
        let found = nodes.iter().find(|n| n.name == "box").expect("found");
        assert!(found.address().ends_with(":47475"));
    }

    #[tokio::test]
    async fn configured_address_wins_without_a_probe() {
        let mut config = Config::default();
        config.nodes.insert("rig".into(), "10.0.0.5:47475".into());
        assert_eq!(address(&config, "rig").await.as_deref(), Some("10.0.0.5:47475"));
    }

    #[test]
    fn status_tolerates_extra_and_missing_fields() {
        let status: NodeStatus = serde_json::from_value(json!({"name": "a", "model": null, "error": null, "download": null, "future": 1})).expect("parse");
        assert!(status.local.is_empty());
    }
}
