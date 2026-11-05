use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use tokio::process::Child;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::download::{fetch, DownloadError, FileSpec};
use crate::engine::{Engine, ServeArgs};
use crate::hf::{check_repo, group_quants, quant_listing, Hf, QuantFiles, TreeFile};
use crate::hw::{free_ram, Hardware};
use crate::pirate::{is_magnet, seed_commit, seed_file_url, web_seed, Pirate, TorrentFile};
use crate::store::{local_name, LastUsed, LocalModel, MetaEntry, Store};

const MIB: u64 = 1024 * 1024;
const LOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const HEALTH_INTERVAL: Duration = Duration::from_millis(300);
const LOG_LINES_IN_ERROR: usize = 12;

/// The discovery reply, also the head of the status reply.
#[derive(Clone, Debug, Serialize)]
pub struct Discovery {
    pub name: String,
    pub port: u16,
    pub version: String,
    pub gpu: Option<String>,
    pub vram_mb: u64,
    pub ram_mb: u64,
    pub model: Option<String>,
    pub state: &'static str,
}

/// The full `/api/status` reply.
#[derive(Clone, Debug, Serialize)]
pub struct Status {
    #[serde(flatten)]
    pub discovery: Discovery,
    pub error: Option<String>,
    pub download: Option<DownloadStatus>,
    pub local: Vec<LocalModel>,
}

/// Progress of the running pull; `done` and `total` cover all files of the quant.
#[derive(Clone, Debug, Serialize)]
pub struct DownloadStatus {
    pub repo: String,
    pub file: String,
    pub done: u64,
    pub total: u64,
    pub bytes_per_sec: u64,
}

/// Why a request could not start an operation.
#[derive(Debug)]
pub enum StartError {
    NotFound(String),
    Busy(String),
    BadRequest(String),
    Upstream(String),
}

struct Running {
    kill: oneshot::Sender<()>,
    supervisor: JoinHandle<()>,
}

#[derive(Default)]
struct Inner {
    generation: u64,
    loading: bool,
    ready: bool,
    model: Option<String>,
    error: Option<String>,
    download: Option<DownloadStatus>,
    cancel: Option<watch::Sender<bool>>,
    running: Option<Running>,
    port: Option<u16>,
}

/// What a node is built from.
pub struct NodeParts {
    pub name: String,
    pub port: u16,
    pub token: String,
    pub hardware: Hardware,
    pub hf: Hf,
    pub proxy: reqwest::Client,
    pub store: Store,
    pub engine: Engine,
}

/// All node state: identity, hardware, model library and the engine lifecycle.
pub struct Node {
    pub name: String,
    pub port: u16,
    pub token: String,
    pub hardware: Hardware,
    pub hf: Hf,
    pub proxy: reqwest::Client,
    pub store: Store,
    pub engine: Engine,
    pub pirate: Pirate,
    inner: Mutex<Inner>,
    engine_lock: tokio::sync::Mutex<()>,
    device: tokio::sync::OnceCell<String>,
}

impl Node {
    /// Builds the node with nothing loaded.
    pub fn new(parts: NodeParts) -> Self {
        let pirate = Pirate::new(parts.hf.client.clone(), parts.store.root().to_path_buf());
        Self {
            pirate,
            name: parts.name,
            port: parts.port,
            token: parts.token,
            hardware: parts.hardware,
            hf: parts.hf,
            proxy: parts.proxy,
            store: parts.store,
            engine: parts.engine,
            inner: Mutex::default(),
            engine_lock: tokio::sync::Mutex::const_new(()),
            device: tokio::sync::OnceCell::new(),
        }
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The discovery view of the current state.
    pub fn discovery(&self) -> Discovery {
        let inner = self.inner();
        let state = if inner.loading {
            "loading"
        } else if inner.ready {
            "ready"
        } else if inner.download.is_some() {
            "downloading"
        } else if inner.error.is_some() {
            "error"
        } else {
            "idle"
        };
        Discovery {
            name: self.name.clone(),
            port: self.port,
            version: env!("CARGO_PKG_VERSION").to_string(),
            gpu: self.hardware.gpu.clone(),
            vram_mb: self.hardware.vram_mb,
            ram_mb: self.hardware.ram_mb,
            model: inner.model.clone(),
            state,
        }
    }

    /// The status view of the current state.
    pub fn status(&self) -> Status {
        let discovery = self.discovery();
        let inner = self.inner();
        let (error, download) = (inner.error.clone(), inner.download.clone());
        drop(inner);
        Status { discovery, error, download, local: self.store.list() }
    }

    /// The port of the engine, once a model is ready.
    pub fn ready_port(&self) -> Option<u16> {
        let inner = self.inner();
        inner.port.filter(|_| inner.ready)
    }

    /// Lists a repo's quants with fit and recommendation for this machine.
    pub async fn quant_files(&self, repo: &str) -> Result<Vec<QuantFiles>> {
        let tree = self.hf.tree(repo).await?;
        Ok(quant_listing(&tree, self.hardware.vram_mb * MIB, free_ram()))
    }

    /// Lists a torrent's quants with fit and recommendation for this machine.
    pub async fn torrent_quants(&self, magnet: &str) -> Result<Vec<QuantFiles>> {
        let tree = self.pirate.files(magnet).await?;
        Ok(quant_listing(&tree, self.hardware.vram_mb * MIB, free_ram()))
    }

    /// Reserves the single download slot and starts pulling a quant in the background.
    pub async fn start_pull(self: &Arc<Self>, repo: String, quant: String, magnet: Option<String>) -> Result<(), StartError> {
        check_repo(&repo).map_err(|e| StartError::BadRequest(e.to_string()))?;
        if magnet.as_deref().is_some_and(|m| !is_magnet(m)) {
            return Err(StartError::BadRequest("That isn't a magnet link.".to_string()));
        }
        let (cancel_tx, cancel_rx) = watch::channel(false);
        {
            let mut inner = self.inner();
            if inner.download.is_some() {
                return Err(StartError::Busy("A download is already running. Wait for it or cancel it.".to_string()));
            }
            inner.error = None;
            inner.cancel = Some(cancel_tx);
            inner.download = Some(DownloadStatus {
                repo: repo.clone(),
                file: String::new(),
                done: 0,
                total: 0,
                bytes_per_sec: 0,
            });
        }
        let prepared = match &magnet {
            Some(magnet) => self.prepare_torrent(&repo, &quant, magnet).await.map(|p| (p.0, p.1, p.2, p.3)),
            None => self.prepare_pull(&repo, &quant).await.map(|p| (p.0, p.1, p.2, Vec::new())),
        };
        let (files, quant, architecture, indices) = match prepared {
            Ok(p) => p,
            Err(e) => {
                self.finish_download(None);
                return Err(e);
            }
        };
        let node = self.clone();
        tokio::spawn(async move {
            let result = match magnet {
                Some(magnet) => node.run_torrent(&repo, &quant, architecture, files, indices, &magnet, cancel_rx).await,
                None => node.run_pull(&repo, &quant, architecture, files, cancel_rx).await,
            };
            node.finish_download(match result {
                Ok(()) | Err(DownloadError::Cancelled) => None,
                Err(DownloadError::Failed(e)) => Some(format!("{e:#}")),
            });
        });
        Ok(())
    }

    async fn prepare_pull(
        &self,
        repo: &str,
        quant: &str,
    ) -> Result<(Vec<TreeFile>, String, Option<String>), StartError> {
        let tree = self.hf.tree(repo).await.map_err(|e| StartError::Upstream(format!("{e:#}")))?;
        let (quant, files) = group_quants(&tree)
            .into_iter()
            .find(|(q, _)| q.eq_ignore_ascii_case(quant))
            .ok_or_else(|| StartError::NotFound(format!("{repo} has no quant named {quant}. List its files and pick another.")))?;
        let architecture = self.hf.architecture(repo).await.unwrap_or(None);
        Ok((files, quant, architecture))
    }

    // Hugging Face is only consulted for checksums, so a repo it has removed can still download.
    async fn prepare_torrent(&self, repo: &str, quant: &str, magnet: &str) -> Result<(Vec<TreeFile>, String, Option<String>, Vec<usize>), StartError> {
        let tree = self.pirate.files(magnet).await.map_err(|e| StartError::Upstream(format!("{e:#}")))?;
        let (quant, mut files) = group_quants(&tree)
            .into_iter()
            .find(|(q, _)| q.eq_ignore_ascii_case(quant))
            .ok_or_else(|| StartError::NotFound(format!("This torrent has no quant named {quant}. List its files and pick another.")))?;
        let indices = files.iter().filter_map(|file| tree.iter().position(|t| t.path == file.path)).collect();
        let revision = web_seed(magnet).and_then(|seed| seed_commit(&seed).map(String::from)).unwrap_or_else(|| "main".to_string());
        let published = self.hf.tree_at(repo, &revision).await.unwrap_or_default();
        for file in &mut files {
            let name = file.path.rsplit('/').next().unwrap_or(&file.path);
            file.lfs = published.iter().find(|p| p.path.rsplit('/').next() == Some(name)).and_then(|p| p.lfs.clone());
        }
        let architecture = self.hf.architecture(repo).await.unwrap_or(None);
        Ok((files, quant, architecture, indices))
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_torrent(
        &self,
        repo: &str,
        quant: &str,
        architecture: Option<String>,
        files: Vec<TreeFile>,
        indices: Vec<usize>,
        magnet: &str,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<(), DownloadError> {
        let dir = self.store.repo_dir(repo)?;
        let verified = files.iter().all(|f| f.lfs.is_some());
        let mut selected = Vec::with_capacity(files.len());
        for file in &files {
            selected.push(TorrentFile { path: file.path.clone(), name: local_name(&file.path)?.to_string(), sha256: file.lfs.as_ref().map(|l| l.oid.clone()) });
        }
        let entry = |files: Vec<String>| MetaEntry { repo: repo.to_string(), quant: quant.to_string(), architecture: architecture.clone(), files, verified };
        let names: Vec<String> = selected.iter().map(|f| f.name.clone()).collect();
        if let Some(seed) = web_seed(magnet) {
            let specs: anyhow::Result<Vec<FileSpec>> = files
                .iter()
                .zip(&selected)
                .map(|(file, chosen)| Ok(FileSpec { url: seed_file_url(&seed, &file.path)?, dest: dir.join(&chosen.name), size: file.size, sha256: chosen.sha256.clone() }))
                .collect();
            // The seed is a third party, so it must never see the Hugging Face token.
            let anonymous = Hf { token: None, ..self.hf.clone() };
            match self.fetch_all(&anonymous, specs?, &mut cancel).await {
                Ok(()) => {
                    self.store.add(entry(names))?;
                    return Ok(());
                }
                Err(DownloadError::Cancelled) => return Err(DownloadError::Cancelled),
                Err(DownloadError::Failed(_)) => {}
            }
        }
        let label = names.first().cloned().unwrap_or_default();
        let mut meter = Meter::default();
        self.pirate
            .download(magnet, indices, &selected, &dir, &mut cancel, &mut |done, total| {
                let rate = meter.rate(done);
                if let Some(status) = self.inner().download.as_mut() {
                    status.file.clone_from(&label);
                    status.done = done;
                    status.total = total;
                    if let Some(rate) = rate {
                        status.bytes_per_sec = rate;
                    }
                }
            })
            .await?;
        self.store.add(entry(names))?;
        Ok(())
    }

    async fn fetch_all(&self, hf: &Hf, specs: Vec<FileSpec>, cancel: &mut watch::Receiver<bool>) -> Result<(), DownloadError> {
        let total: u64 = specs.iter().map(|s| s.size).sum();
        let mut finished = 0;
        for spec in specs {
            let name = spec.dest.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            let mut meter = Meter::default();
            fetch(hf, &spec, cancel, &mut |have| {
                let done = finished + have;
                let rate = meter.rate(done);
                if let Some(status) = self.inner().download.as_mut() {
                    status.file.clone_from(&name);
                    status.done = done;
                    status.total = total;
                    if let Some(rate) = rate {
                        status.bytes_per_sec = rate;
                    }
                }
            })
            .await?;
            finished += spec.size;
        }
        Ok(())
    }

    async fn run_pull(
        &self,
        repo: &str,
        quant: &str,
        architecture: Option<String>,
        files: Vec<TreeFile>,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<(), DownloadError> {
        let dir = self.store.repo_dir(repo)?;
        let mut names = Vec::with_capacity(files.len());
        let mut specs = Vec::with_capacity(files.len());
        for file in files {
            let name = local_name(&file.path)?.to_string();
            specs.push(FileSpec {
                url: self.hf.file_url(repo, &file.path)?,
                dest: dir.join(&name),
                size: file.size,
                sha256: file.lfs.as_ref().map(|l| l.oid.clone()),
            });
            names.push(name);
        }
        self.fetch_all(&self.hf, specs, &mut cancel).await?;
        self.store.add(MetaEntry {
            repo: repo.to_string(),
            quant: quant.to_string(),
            architecture,
            files: names,
            verified: true,
        })?;
        Ok(())
    }

    fn finish_download(&self, error: Option<String>) {
        let mut inner = self.inner();
        inner.download = None;
        inner.cancel = None;
        if error.is_some() {
            inner.error = error;
        }
    }

    /// Asks the running pull to stop; its partial file stays for a later resume.
    pub fn cancel_pull(&self) {
        if let Some(cancel) = self.inner().cancel.as_ref() {
            cancel.send_replace(true);
        }
    }

    /// Starts loading a local model in the background, replacing whatever is loaded.
    pub fn start_load(self: &Arc<Self>, id: &str, ctx: Option<String>, parallel: Option<u32>) -> Result<(), StartError> {
        let model = self
            .store
            .get(id)
            .ok_or_else(|| StartError::NotFound(format!("No local model has the id {id}. Pull it first.")))?;
        let generation = {
            let mut inner = self.inner();
            inner.generation += 1;
            inner.loading = true;
            inner.ready = false;
            inner.port = None;
            inner.error = None;
            inner.model = Some(model.id.clone());
            inner.generation
        };
        let node = self.clone();
        tokio::spawn(async move {
            if let Err(e) = node.load(generation, &model, ctx, parallel).await {
                node.fail_load(generation, format!("{e:#}"));
            }
        });
        Ok(())
    }

    async fn load(
        self: &Arc<Self>,
        generation: u64,
        model: &LocalModel,
        ctx: Option<String>,
        parallel: Option<u32>,
    ) -> Result<()> {
        let port = {
            let _engine = self.engine_lock.lock().await;
            self.stop_running().await;
            if self.is_stale(generation) {
                return Ok(());
            }
            let device = self.device.get_or_init(|| self.engine.pick_device()).await;
            let args = ServeArgs {
                model: &model.path,
                device,
                ctx: ctx.as_deref(),
                parallel,
            };
            let spawned = self.engine.serve(&args).await?;
            self.supervise(generation, spawned.child);
            spawned.port
        };
        self.await_health(generation, port).await?;
        let became_ready = {
            let mut inner = self.inner();
            let current = inner.generation == generation && inner.loading;
            if current {
                inner.loading = false;
                inner.ready = true;
                inner.port = Some(port);
            }
            current
        };
        if became_ready {
            self.store.set_last_used(&LastUsed { id: model.id.clone(), ctx, parallel })?;
        }
        Ok(())
    }

    fn supervise(self: &Arc<Self>, generation: u64, mut child: Child) {
        let (kill, killed) = oneshot::channel::<()>();
        let node = self.clone();
        let supervisor = tokio::spawn(async move {
            tokio::select! {
                status = child.wait() => {
                    let status = status.map_or_else(|e| e.to_string(), |s| s.to_string());
                    node.on_engine_exit(generation, &status).await;
                }
                _ = killed => {
                    child.kill().await.ok();
                }
            }
        });
        self.inner().running = Some(Running { kill, supervisor });
    }

    async fn on_engine_exit(&self, generation: u64, status: &str) {
        // The log pump needs a moment to drain the pipes of the dead process.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let tail = self.engine.log_tail(LOG_LINES_IN_ERROR);
        self.fail_load(
            generation,
            format!(
                "The engine stopped ({status}). Pick a smaller quant or a smaller ctx and load again. Log: {}\n{tail}",
                self.engine.log_path().display()
            ),
        );
    }

    fn fail_load(&self, generation: u64, error: String) {
        let mut inner = self.inner();
        if inner.generation == generation {
            inner.loading = false;
            inner.ready = false;
            inner.port = None;
            inner.model = None;
            inner.error = Some(error);
        }
    }

    fn is_stale(&self, generation: u64) -> bool {
        self.inner().generation != generation
    }

    async fn await_health(&self, generation: u64, port: u16) -> Result<()> {
        let url = format!("http://127.0.0.1:{port}/health");
        let started = Instant::now();
        loop {
            tokio::time::sleep(HEALTH_INTERVAL).await;
            {
                let inner = self.inner();
                if inner.generation != generation || !inner.loading {
                    return Ok(());
                }
            }
            if started.elapsed() > LOAD_TIMEOUT {
                let _engine = self.engine_lock.lock().await;
                if !self.is_stale(generation) {
                    self.stop_running().await;
                }
                return Err(anyhow!("The engine did not become ready in {} minutes. Try a smaller model.", LOAD_TIMEOUT.as_secs() / 60));
            }
            let health = self.proxy.get(&url).timeout(Duration::from_secs(2)).send().await;
            if health.is_ok_and(|r| r.status().is_success()) {
                return Ok(());
            }
        }
    }

    async fn stop_running(&self) {
        let running = self.inner().running.take();
        if let Some(running) = running {
            running.kill.send(()).ok();
            running.supervisor.await.ok();
        }
    }

    /// Stops the engine and waits until its process is gone, releasing its VRAM.
    pub async fn unload(&self) {
        {
            let mut inner = self.inner();
            inner.generation += 1;
            inner.loading = false;
            inner.ready = false;
            inner.port = None;
            inner.model = None;
            inner.error = None;
        }
        let _engine = self.engine_lock.lock().await;
        self.stop_running().await;
    }

    /// Deletes a local model, unloading it first if it is in use.
    pub async fn delete_local(&self, id: &str) -> Result<bool> {
        if self.inner().model.as_deref() == Some(id) {
            self.unload().await;
        }
        self.store.delete(id).with_context(|| format!("delete {id}"))
    }
}

#[derive(Default)]
struct Meter {
    last: Option<(Instant, u64)>,
}

impl Meter {
    fn rate(&mut self, done: u64) -> Option<u64> {
        let Some((at, previous)) = self.last else {
            self.last = Some((Instant::now(), done));
            return None;
        };
        let elapsed = at.elapsed();
        if elapsed < Duration::from_millis(500) {
            return None;
        }
        self.last = Some((Instant::now(), done));
        Some((done.saturating_sub(previous) as f64 / elapsed.as_secs_f64()) as u64)
    }
}
