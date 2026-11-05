use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, DhtSessionConfig, Session, SessionOptions};
use reqwest::{Client, Url};
use serde::Serialize;
use tokio::sync::{watch, OnceCell};

use crate::download::{cancelled, sha256_file, DownloadError};
use crate::hf::TreeFile;

/// Where model torrents are listed.
pub const PIRATE_BASE: &str = "https://pirateface.co";

const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
const METADATA_TIMEOUT: Duration = Duration::from_secs(90);
const STALL_LIMIT: Duration = Duration::from_secs(5 * 60);

/// One model torrent in the wire format.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TorrentHit {
    pub repo: String,
    pub size: String,
    pub seeds: u64,
    pub magnet: String,
}

/// One selected file of a torrent and what it should hash to, when that's known.
#[derive(Clone, Debug)]
pub struct TorrentFile {
    pub path: String,
    pub name: String,
    pub sha256: Option<String>,
}

/// Searches Pirate Face and downloads the files chosen from its torrents.
pub struct Pirate {
    client: Client,
    base: String,
    folder: PathBuf,
    session: OnceCell<Arc<Session>>,
}

/// The HTTPS web seed a magnet link names, if any.
pub fn web_seed(magnet: &str) -> Option<String> {
    let url = Url::parse(magnet).ok()?;
    let seed = url.query_pairs().find(|(key, _)| key == "ws")?.1.into_owned();
    seed.starts_with("https://").then_some(seed)
}

/// The Hugging Face commit a web seed is pinned to, which is what its checksums must come from.
pub fn seed_commit(seed: &str) -> Option<&str> {
    seed.trim_end_matches('/').rsplit('/').next().filter(|last| last.len() == 40 && last.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Where a web seed serves one torrent file.
pub fn seed_file_url(seed: &str, path: &str) -> Result<Url> {
    let mut url = Url::parse(seed).context("read the web seed address")?;
    {
        let mut segments = url.path_segments_mut().map_err(|()| anyhow!("the web seed address has no path"))?;
        segments.pop_if_empty();
        segments.extend(path.split('/'));
    }
    Ok(url)
}

/// Reads the site's plain-text results: `repo  55 GB  8 seeds  magnet:?...`.
pub fn parse_search(text: &str) -> Vec<TorrentHit> {
    text.lines()
        .filter_map(|line| match line.split_whitespace().collect::<Vec<_>>()[..] {
            [repo, number, unit, seeds, "seeds" | "seed", magnet] if is_magnet(magnet) => {
                Some(TorrentHit { repo: repo.to_string(), size: format!("{number} {unit}"), seeds: seeds.parse().ok()?, magnet: magnet.to_string() })
            }
            _ => None,
        })
        .collect()
}

/// True for a BitTorrent magnet link, which is all the torrent engine should be handed.
pub fn is_magnet(text: &str) -> bool {
    text.starts_with("magnet:?xt=urn:bt")
}

// Names come from a stranger's torrent, so nothing may climb out of the download folder.
fn is_safe_path(path: &str) -> bool {
    !path.is_empty() && Path::new(path).components().all(|c| matches!(c, Component::Normal(_)))
}

fn search_url(base: &str, query: &str) -> Result<Url> {
    let mut words: Vec<&str> = query.split_whitespace().collect();
    if !words.iter().any(|w| w.eq_ignore_ascii_case("gguf")) {
        words.push("gguf");
    }
    let mut url = Url::parse(base).context("build the search URL")?;
    url.path_segments_mut().map_err(|()| anyhow!("the search address has no path"))?.push("s").push(&words.join("+"));
    Ok(url)
}

impl Pirate {
    /// A client that downloads under `folder`; no peer connections open until the first torrent.
    pub fn new(client: Client, folder: PathBuf) -> Self {
        Self { client, base: PIRATE_BASE.to_string(), folder, session: OnceCell::new() }
    }

    async fn session(&self) -> Result<&Arc<Session>> {
        self.session
            .get_or_try_init(|| async {
                let options = SessionOptions {
                    dht: Some(DhtSessionConfig { persistence: None, ..DhtSessionConfig::default() }),
                    persistence: None,
                    listen: None,
                    disable_local_service_discovery: true,
                    ..SessionOptions::default()
                };
                Session::new_with_opts(self.folder.clone(), options).await.context("start the torrent engine")
            })
            .await
    }

    /// Finds GGUF model torrents; the engine can't run other formats.
    pub async fn search(&self, query: &str) -> Result<Vec<TorrentHit>> {
        let response = self.client.get(search_url(&self.base, query)?).timeout(SEARCH_TIMEOUT).send().await.context("reach Pirate Face")?;
        let status = response.status();
        if !status.is_success() {
            bail!("Pirate Face returned {status}. Try again in a moment, or use /hf.");
        }
        let text = response.text().await.context("read Pirate Face's results")?;
        Ok(parse_search(&text).into_iter().filter(|hit| hit.repo.to_ascii_lowercase().contains("gguf")).collect())
    }

    /// Asks the swarm which files a torrent holds, in torrent order.
    pub async fn files(&self, magnet: &str) -> Result<Vec<TreeFile>> {
        let session = self.session().await?;
        let options = AddTorrentOptions { list_only: true, ..AddTorrentOptions::default() };
        let added = tokio::time::timeout(METADATA_TIMEOUT, session.add_torrent(AddTorrent::from_url(magnet), Some(options)))
            .await
            .map_err(|_| anyhow!("Nobody sharing this torrent answered within 90 seconds. Try a listing with more seeds, or use /hf."))?
            .context("read the torrent's file list")?;
        let AddTorrentResponse::ListOnly(listing) = added else { bail!("The torrent engine didn't return a file list.") };
        Ok(listing.info.iter_file_details().map(|file| TreeFile { path: file.filename.to_vec().join("/"), size: file.len, lfs: None }).collect())
    }

    /// Downloads the chosen files, stops sharing, checks them and moves them into `dest`.
    pub async fn download(
        &self,
        magnet: &str,
        indices: Vec<usize>,
        files: &[TorrentFile],
        dest: &Path,
        cancel: &mut watch::Receiver<bool>,
        on_progress: &mut (impl FnMut(u64, u64) + Send),
    ) -> Result<(), DownloadError> {
        if let Some(bad) = files.iter().find(|f| !is_safe_path(&f.path)) {
            return Err(anyhow!("The torrent has an unsafe file name: {}", bad.path).into());
        }
        let work = dest.join(".torrent");
        let session = self.session().await?;
        let options = AddTorrentOptions { only_files: Some(indices), output_folder: Some(work.display().to_string()), overwrite: true, ..AddTorrentOptions::default() };
        let added = session.add_torrent(AddTorrent::from_url(magnet), Some(options)).await.context("start the torrent")?;
        let (id, handle) = match added {
            AddTorrentResponse::Added(id, handle) | AddTorrentResponse::AlreadyManaged(id, handle) => (id, handle),
            AddTorrentResponse::ListOnly(_) => return Err(anyhow!("The torrent engine didn't start the download.").into()),
        };
        let (mut last_bytes, mut last_change) = (0, Instant::now());
        let outcome = loop {
            let stats = handle.stats();
            on_progress(stats.progress_bytes, stats.total_bytes);
            if stats.finished {
                break Ok(());
            }
            if let Some(error) = stats.error {
                break Err(DownloadError::Failed(anyhow!("The torrent failed: {error}")));
            }
            if stats.progress_bytes != last_bytes {
                (last_bytes, last_change) = (stats.progress_bytes, Instant::now());
            } else if last_change.elapsed() > STALL_LIMIT {
                break Err(DownloadError::Failed(anyhow!("Nobody on the torrent network sent this file for 5 minutes, and its direct link didn't work. Pull again to resume, or use /hf.")));
            }
            tokio::select! {
                () = cancelled(cancel) => break Err(DownloadError::Cancelled),
                () = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        };
        // Removing it ends sharing as soon as the download stops; partial files stay for a resume.
        session.delete(id.into(), false).await.context("stop the torrent")?;
        outcome?;
        for file in files {
            let source = work.join(&file.path);
            if let Some(expected) = file.sha256.clone() {
                let path = source.clone();
                let actual = tokio::task::spawn_blocking(move || sha256_file(&path)).await.context("hash task")?.context("hash the downloaded file")?;
                if !actual.eq_ignore_ascii_case(&expected) {
                    let _ = tokio::fs::remove_file(&source).await;
                    return Err(anyhow!("{} doesn't match the checksum Hugging Face publishes, so it was deleted. Try another listing or use /hf.", file.name).into());
                }
            }
            tokio::fs::rename(&source, dest.join(&file.name)).await.with_context(|| format!("move {}", file.name))?;
        }
        let _ = tokio::fs::remove_dir_all(&work).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAGNET: &str = "magnet:?xt=urn:btih:975566b4867488c2ac455cdc8c37e32837851949&tr=udp%3A%2F%2Ftracker";

    #[test]
    fn results_parse_and_the_empty_message_is_not_a_result() {
        let text = format!("huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF      55 GB  8 seeds  {MAGNET}\nunsloth/Small-GGUF   271 MB  1 seed  {MAGNET}\n");
        let hits = parse_search(&text);
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].repo.as_str(), hits[0].size.as_str(), hits[0].seeds), ("huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF", "55 GB", 8));
        assert_eq!(hits[1].seeds, 1);
        assert!(parse_search("No torrents match \"x\" yet.\nBrowse or submit one: https://pirateface.co/models?q=x\n").is_empty());
    }

    #[test]
    fn web_seeds_are_read_from_magnets_and_must_be_https() {
        let magnet = "magnet:?xt=urn:btih:21d0&tr=udp%3A%2F%2Ft&ws=https%3A%2F%2Fpirateface.co%2Fapi%2Fws%2FHF%2FSmol-GGUF%2F2d4a76a30b4af41ecd395c35725ac11688d4cfe4%2F";
        let seed = web_seed(magnet).expect("seed");
        assert_eq!(seed, "https://pirateface.co/api/ws/HF/Smol-GGUF/2d4a76a30b4af41ecd395c35725ac11688d4cfe4/");
        assert_eq!(seed_commit(&seed), Some("2d4a76a30b4af41ecd395c35725ac11688d4cfe4"));
        assert_eq!(web_seed("magnet:?xt=urn:btih:21d0&ws=http%3A%2F%2Fexample.com%2F"), None);
        assert_eq!(web_seed("magnet:?xt=urn:btih:21d0"), None);
        assert_eq!(seed_commit("https://x.co/api/ws/repo/not-a-commit/"), None);
    }

    #[test]
    fn web_seed_file_urls_keep_folders_and_encode_names() {
        let url = seed_file_url("https://x.co/ws/abc/", "sub/my model.gguf").unwrap();
        assert_eq!(url.as_str(), "https://x.co/ws/abc/sub/my%20model.gguf");
    }

    #[test]
    fn only_magnet_links_are_accepted() {
        assert!(is_magnet(MAGNET));
        assert!(!is_magnet("https://example.com/evil.torrent"));
        assert!(!is_magnet("C:\\Windows\\evil.torrent"));
        assert!(parse_search("a/b 1 GB 2 seeds https://example.com/x").is_empty());
    }

    #[test]
    fn torrent_paths_cannot_leave_the_download_folder() {
        assert!(is_safe_path("sub/model-Q4_K_M.gguf"));
        for bad in ["../evil.gguf", "sub/../../evil.gguf", "C:\\evil.gguf", "\\evil.gguf", "/evil.gguf", ""] {
            assert!(!is_safe_path(bad), "{bad}");
        }
    }

    #[test]
    fn searches_ask_for_gguf_and_keep_words_in_one_path_segment() {
        assert_eq!(search_url(PIRATE_BASE, "kimi k3").unwrap().as_str(), "https://pirateface.co/s/kimi+k3+gguf");
        assert_eq!(search_url(PIRATE_BASE, "qwen GGUF").unwrap().as_str(), "https://pirateface.co/s/qwen+GGUF");
        assert_eq!(search_url(PIRATE_BASE, "../api").unwrap().path_segments().unwrap().count(), 2);
    }
}
