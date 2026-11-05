use std::future::pending;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context};
use futures_util::StreamExt;
use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::{RequestBuilder, Response, StatusCode, Url};
use sha2::{Digest, Sha256};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::watch;

use crate::hf::Hf;

const MAX_FAILURES_WITHOUT_PROGRESS: u32 = 6;
const MAX_BACKOFF_SECS: u64 = 30;

/// One remote file and where it lands.
#[derive(Clone, Debug)]
pub struct FileSpec {
    pub url: Url,
    pub dest: PathBuf,
    pub size: u64,
    pub sha256: Option<String>,
}

/// Why a download stopped.
#[derive(Debug)]
pub enum DownloadError {
    Cancelled,
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for DownloadError {
    fn from(e: anyhow::Error) -> Self {
        Self::Failed(e)
    }
}

enum AttemptError {
    Cancelled,
    Fatal(anyhow::Error),
    Transient { error: anyhow::Error, progressed: bool },
}

/// Resolves when cancel flips to true, and never if the sender is gone.
pub async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    if cancel.wait_for(|c| *c).await.is_err() {
        pending::<()>().await;
    }
}

/// Downloads into `<dest>.part`, resuming and retrying, then verifies and renames it.
pub async fn fetch(
    hf: &Hf,
    spec: &FileSpec,
    cancel: &mut watch::Receiver<bool>,
    on_progress: &mut (impl FnMut(u64) + Send),
) -> Result<(), DownloadError> {
    if spec.size > 0 && fs::metadata(&spec.dest).await.is_ok_and(|m| m.len() == spec.size) {
        on_progress(spec.size);
        return Ok(());
    }
    let part = part_path(&spec.dest);
    let mut failures = 0;
    loop {
        match attempt(hf, spec, &part, cancel, on_progress).await {
            Ok(()) => break,
            Err(AttemptError::Cancelled) => return Err(DownloadError::Cancelled),
            Err(AttemptError::Fatal(e)) => return Err(e.into()),
            Err(AttemptError::Transient { error, progressed }) => {
                failures = if progressed { 1 } else { failures + 1 };
                if failures > MAX_FAILURES_WITHOUT_PROGRESS {
                    return Err(error
                        .context("the download kept failing. Check the internet connection and pull again to resume")
                        .into());
                }
                let delay = (1u64 << failures.min(10)).min(MAX_BACKOFF_SECS);
                tokio::select! {
                    _ = cancelled(cancel) => return Err(DownloadError::Cancelled),
                    _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
                }
            }
        }
    }
    verify(&part, spec).await?;
    fs::rename(&part, &spec.dest).await.with_context(|| format!("rename {}", part.display()))?;
    Ok(())
}

/// The temporary name a file has while it is incomplete.
pub fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

async fn attempt(
    hf: &Hf,
    spec: &FileSpec,
    part: &Path,
    cancel: &mut watch::Receiver<bool>,
    on_progress: &mut (impl FnMut(u64) + Send),
) -> Result<(), AttemptError> {
    let mut have = fs::metadata(part).await.map_or(0, |m| m.len());
    if spec.size > 0 && have > spec.size {
        fs::remove_file(part).await.map_err(fatal_io)?;
        have = 0;
    }
    if spec.size > 0 && have == spec.size {
        on_progress(have);
        return Ok(());
    }
    on_progress(have);

    let mut req: RequestBuilder = hf.auth(hf.client.get(spec.url.clone()));
    if have > 0 {
        req = req.header(RANGE, format!("bytes={have}-"));
    }
    let resp = tokio::select! {
        _ = cancelled(cancel) => return Err(AttemptError::Cancelled),
        r = req.send() => r.map_err(|e| transient(e.into(), false))?,
    };
    let status = resp.status();
    let resume = status == StatusCode::PARTIAL_CONTENT && content_range_start(&resp) == Some(have);
    let unusable_range = status == StatusCode::RANGE_NOT_SATISFIABLE || (status == StatusCode::PARTIAL_CONTENT && !resume);
    if unusable_range {
        fs::remove_file(part).await.map_err(fatal_io)?;
        return Err(transient(anyhow!("Hugging Face could not resume from byte {have}"), false));
    }
    if !status.is_success() {
        return Err(classify_status(status, &spec.url));
    }
    let mut start = have;
    let file = if resume {
        OpenOptions::new().append(true).open(part).await.map_err(fatal_io)?
    } else {
        start = 0;
        File::create(part).await.map_err(fatal_io)?
    };
    let mut out = BufWriter::with_capacity(1 << 20, file);
    let mut written = start;
    let mut stream = resp.bytes_stream();
    let end: Result<(), AttemptError> = loop {
        let next = tokio::select! {
            _ = cancelled(cancel) => break Err(AttemptError::Cancelled),
            n = stream.next() => n,
        };
        match next {
            None => break Ok(()),
            Some(Err(e)) => break Err(transient(e.into(), written > start)),
            Some(Ok(chunk)) => {
                if let Err(e) = out.write_all(&chunk).await {
                    break Err(fatal_io(e));
                }
                written += chunk.len() as u64;
                on_progress(written);
            }
        }
    };
    out.flush().await.map_err(fatal_io)?;
    end?;
    if spec.size > 0 && written < spec.size {
        return Err(transient(anyhow!("the connection closed at {written} of {} bytes", spec.size), written > start));
    }
    Ok(())
}

fn content_range_start(resp: &Response) -> Option<u64> {
    let value = resp.headers().get(CONTENT_RANGE)?.to_str().ok()?;
    let range = value.strip_prefix("bytes ")?;
    range.split('-').next()?.parse().ok()
}

fn classify_status(status: StatusCode, url: &Url) -> AttemptError {
    match status.as_u16() {
        401 | 403 => AttemptError::Fatal(anyhow!(
            "Hugging Face refused {url} ({status}). If the repo is gated, accept its terms on huggingface.co and set HF_TOKEN."
        )),
        404 => AttemptError::Fatal(anyhow!("Hugging Face has no file at {url} ({status}). Pick another quant.")),
        408 | 429 | 500..=599 => transient(anyhow!("Hugging Face returned {status}"), false),
        _ => AttemptError::Fatal(anyhow!("Hugging Face returned {status} for {url}.")),
    }
}

fn transient(error: anyhow::Error, progressed: bool) -> AttemptError {
    AttemptError::Transient { error, progressed }
}

fn fatal_io(e: std::io::Error) -> AttemptError {
    AttemptError::Fatal(anyhow!("could not write the model file: {e}. Check free disk space and pull again."))
}

async fn verify(part: &Path, spec: &FileSpec) -> Result<(), DownloadError> {
    let len = fs::metadata(part).await.context("read the downloaded file")?.len();
    let mut problem = None;
    if spec.size > 0 && len != spec.size {
        problem = Some(format!("its size is {len} bytes, expected {}", spec.size));
    } else if let Some(expected) = spec.sha256.as_deref() {
        let path = part.to_path_buf();
        let actual = tokio::task::spawn_blocking(move || sha256_file(&path))
            .await
            .context("hash task")?
            .context("hash the downloaded file")?;
        if !actual.eq_ignore_ascii_case(expected) {
            problem = Some(format!("its SHA-256 is {actual}, expected {expected}"));
        }
    }
    match problem {
        None => Ok(()),
        Some(p) => {
            fs::remove_file(part).await.context("remove the corrupt download")?;
            Err(anyhow!("the download of {} was corrupt: {p}. Pull again to retry.", spec.dest.display()).into())
        }
    }
}

/// Hex SHA-256 of a file.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hex(&hasher.finalize()));
        }
        hasher.update(&buf[..n]);
    }
}

/// Lower-case hex of a byte slice.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{header, HeaderMap, StatusCode as AxumStatus};
    use axum::response::Response as AxumResponse;
    use axum::routing::get;
    use axum::Router;
    use futures_util::stream;

    use super::*;

    #[derive(Clone)]
    struct Server {
        body: Arc<Vec<u8>>,
        ranges: Arc<Mutex<Vec<Option<String>>>>,
        honour_range: bool,
        cut_first_after: Option<usize>,
        requests: Arc<AtomicUsize>,
    }

    fn server(body: Vec<u8>) -> Server {
        Server {
            body: Arc::new(body),
            ranges: Arc::default(),
            honour_range: true,
            cut_first_after: None,
            requests: Arc::default(),
        }
    }

    async fn serve_file(State(s): State<Server>, headers: HeaderMap) -> AxumResponse {
        let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).map(str::to_string);
        s.ranges.lock().unwrap().push(range.clone());
        let nth = s.requests.fetch_add(1, Ordering::SeqCst);
        let start: usize = range
            .filter(|_| s.honour_range)
            .and_then(|r| r.strip_prefix("bytes=")?.strip_suffix('-')?.parse().ok())
            .unwrap_or(0);
        let total = s.body.len();
        let partial = start > 0;
        let mut slice = s.body[start..].to_vec();
        let builder = AxumResponse::builder()
            .status(if partial { AxumStatus::PARTIAL_CONTENT } else { AxumStatus::OK })
            .header(header::CONTENT_LENGTH, slice.len());
        let builder = if partial {
            builder.header(header::CONTENT_RANGE, format!("bytes {start}-{}/{total}", total - 1))
        } else {
            builder
        };
        let body = match s.cut_first_after {
            Some(cut) if nth == 0 => {
                slice.truncate(cut);
                let head = stream::iter([Ok::<_, std::io::Error>(slice)]);
                let cut = stream::once(async {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Err(std::io::Error::other("connection cut"))
                });
                Body::from_stream(head.chain(cut))
            }
            _ => Body::from(slice),
        };
        builder.body(body).unwrap()
    }

    async fn start(state: Server) -> Hf {
        let app = Router::new().route("/file", get(serve_file)).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        rustls::crypto::ring::default_provider().install_default().ok();
        Hf { client: reqwest::Client::new(), base: format!("http://{addr}"), token: None }
    }

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn sha(bytes: &[u8]) -> String {
        hex(&Sha256::digest(bytes))
    }

    fn scratch_dest(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("olympus-node-test-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("model.gguf")
    }

    fn spec(hf: &Hf, dest: &Path, body: &[u8], sha256: Option<String>) -> FileSpec {
        FileSpec {
            url: Url::parse(&format!("{}/file", hf.base)).unwrap(),
            dest: dest.to_path_buf(),
            size: body.len() as u64,
            sha256,
        }
    }

    #[tokio::test]
    async fn resumes_from_the_part_file_with_a_range_request() {
        let body = sample(300_000);
        let state = server(body.clone());
        let hf = start(state.clone()).await;
        let dest = scratch_dest("resume");
        std::fs::write(part_path(&dest), &body[..100_000]).unwrap();
        let (_tx, mut rx) = watch::channel(false);
        let mut last = 0;

        let result = fetch(&hf, &spec(&hf, &dest, &body, Some(sha(&body))), &mut rx, &mut |n| last = n).await;

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(!part_path(&dest).exists());
        assert_eq!(state.ranges.lock().unwrap().as_slice(), [Some("bytes=100000-".to_string())]);
        assert_eq!(last, 300_000);
    }

    #[tokio::test]
    async fn restarts_when_the_server_ignores_range() {
        let body = sample(200_000);
        let mut state = server(body.clone());
        state.honour_range = false;
        let hf = start(state).await;
        let dest = scratch_dest("ignore-range");
        std::fs::write(part_path(&dest), &body[..50_000]).unwrap();
        let (_tx, mut rx) = watch::channel(false);

        let result = fetch(&hf, &spec(&hf, &dest, &body, Some(sha(&body))), &mut rx, &mut |_| {}).await;

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
    }

    #[tokio::test]
    async fn retries_and_resumes_after_a_cut_connection() {
        let body = sample(400_000);
        let mut state = server(body.clone());
        state.cut_first_after = Some(150_000);
        let hf = start(state.clone()).await;
        let dest = scratch_dest("cut");
        let (_tx, mut rx) = watch::channel(false);

        let result = fetch(&hf, &spec(&hf, &dest, &body, Some(sha(&body))), &mut rx, &mut |_| {}).await;

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        let ranges = state.ranges.lock().unwrap();
        assert_eq!(ranges.len(), 2);
        assert!(ranges[1].as_deref().is_some_and(|r| r.starts_with("bytes=")), "{ranges:?}");
    }

    #[tokio::test]
    async fn sha_mismatch_is_rejected_and_nothing_is_kept() {
        let body = sample(64_000);
        let hf = start(server(body.clone())).await;
        let dest = scratch_dest("sha");
        let (_tx, mut rx) = watch::channel(false);
        let wrong = "0".repeat(64);

        let result = fetch(&hf, &spec(&hf, &dest, &body, Some(wrong)), &mut rx, &mut |_| {}).await;

        let Err(DownloadError::Failed(e)) = result else {
            panic!("expected a failure, got {result:?}");
        };
        assert!(e.to_string().contains("SHA-256"), "{e}");
        assert!(!dest.exists());
        assert!(!part_path(&dest).exists());
    }

    #[tokio::test]
    async fn cancel_stops_the_download_and_keeps_the_part_file() {
        let body = sample(64_000);
        let hf = start(server(body.clone())).await;
        let dest = scratch_dest("cancel");
        std::fs::write(part_path(&dest), &body[..1_000]).unwrap();
        let (tx, mut rx) = watch::channel(false);
        tx.send(true).unwrap();

        let result = fetch(&hf, &spec(&hf, &dest, &body, None), &mut rx, &mut |_| {}).await;

        assert!(matches!(result, Err(DownloadError::Cancelled)), "{result:?}");
        assert!(!dest.exists());
        assert!(part_path(&dest).exists());
    }

    #[tokio::test]
    async fn missing_file_is_a_fatal_error_without_retries() {
        let hf = start(server(sample(10))).await;
        let dest = scratch_dest("missing");
        let (_tx, mut rx) = watch::channel(false);
        let bad = FileSpec {
            url: Url::parse(&format!("{}/nope", hf.base)).unwrap(),
            dest: dest.clone(),
            size: 10,
            sha256: None,
        };

        let result = fetch(&hf, &bad, &mut rx, &mut |_| {}).await;

        let Err(DownloadError::Failed(e)) = result else {
            panic!("expected a failure, got {result:?}");
        };
        assert!(e.to_string().contains("404"), "{e}");
    }
}
