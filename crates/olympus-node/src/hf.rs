use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::header::{HeaderValue, AUTHORIZATION, LINK};
use reqwest::{Client, RequestBuilder, Url};
use serde::{Deserialize, Serialize};

const GIB: u64 = 1024 * 1024 * 1024;
const API_TIMEOUT: Duration = Duration::from_secs(30);

/// Architecture names the embedded engine loads, kept equal to infr-llama's arch.rs by a test.
pub const SUPPORTED_ARCHITECTURES: &[&str] = &[
    "llama",
    "llama4",
    "qwen2",
    "qwen3",
    "qwen3moe",
    "gemma3",
    "gemma4",
    "qwen35",
    "qwen35moe",
    "diffusion-gemma",
    "bitnet",
    "deepseek",
    "deepseek2",
    "deepseek32",
    "deepseek4",
    "bitnet-b1.58",
];

const QUANT_PREFERENCE: &[&str] = &[
    "Q4_K_M", "Q4_K_XL", "Q5_K_M", "Q4_K_L", "Q4_K_S", "Q5_K_S", "Q6_K", "IQ4_NL", "IQ4_XS", "Q4_1", "Q4_0",
    "Q3_K_XL", "Q3_K_L", "Q3_K_M", "Q3_K_S", "IQ3_S", "IQ3_XXS", "Q8_0", "Q2_K_L", "Q2_K", "IQ2_M",
];

/// Hugging Face API client rooted at a base URL.
#[derive(Clone)]
pub struct Hf {
    pub client: Client,
    pub base: String,
    pub token: Option<String>,
}

/// One search result in the wire format.
#[derive(Clone, Debug, Serialize)]
pub struct SearchHit {
    pub repo: String,
    pub downloads: u64,
    pub likes: u64,
    pub architecture: Option<String>,
    pub supported: bool,
    pub updated: Option<String>,
}

/// One file from a repo tree listing.
#[derive(Clone, Debug, Deserialize)]
pub struct TreeFile {
    pub path: String,
    #[serde(default)]
    pub size: u64,
    pub lfs: Option<LfsInfo>,
}

/// The LFS pointer of a tree file; `oid` is its SHA-256.
#[derive(Clone, Debug, Deserialize)]
pub struct LfsInfo {
    pub oid: String,
}

/// Where a quant fits on this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Fit {
    Gpu,
    Cpu,
    None,
}

/// All files of one quant in the wire format.
#[derive(Clone, Debug, Serialize)]
pub struct QuantFiles {
    pub quant: String,
    pub files: Vec<String>,
    pub size: u64,
    pub fit: Fit,
    pub recommended: bool,
}

#[derive(Deserialize)]
struct ApiModel {
    id: String,
    #[serde(default)]
    downloads: u64,
    #[serde(default)]
    likes: u64,
    #[serde(rename = "lastModified")]
    last_modified: Option<String>,
    gguf: Option<ApiGguf>,
}

#[derive(Deserialize)]
struct ApiGguf {
    architecture: Option<String>,
}

impl Hf {
    /// Attaches the optional HF token to a request.
    pub fn auth(&self, req: RequestBuilder) -> RequestBuilder {
        match &self.token {
            Some(token) => req.header(AUTHORIZATION, format!("Bearer {token}")),
            None => req,
        }
    }

    /// Searches GGUF repos by download count.
    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>> {
        let url = Url::parse_with_params(
            &format!("{}/api/models", self.base),
            &[
                ("filter", "gguf"),
                ("search", query),
                ("sort", "downloads"),
                ("limit", "30"),
                ("expand[]", "gguf"),
                ("expand[]", "downloads"),
                ("expand[]", "likes"),
                ("expand[]", "lastModified"),
            ],
        )?;
        let resp = self.auth(self.client.get(url)).timeout(API_TIMEOUT).send().await?;
        let models: Vec<ApiModel> = checked(resp, "search").await?.json().await?;
        Ok(models.into_iter().map(hit_from).collect())
    }

    /// Lists every file of a repo at the main revision, following pagination.
    pub async fn tree(&self, repo: &str) -> Result<Vec<TreeFile>> {
        self.tree_at(repo, "main").await
    }

    /// Lists every file of a repo at a revision, following pagination.
    pub async fn tree_at(&self, repo: &str, revision: &str) -> Result<Vec<TreeFile>> {
        check_repo(repo)?;
        let mut next = Some(format!("{}/api/models/{repo}/tree/{revision}?recursive=true", self.base));
        let mut files = Vec::new();
        while let Some(url) = next.take() {
            let resp = self.auth(self.client.get(&url)).timeout(API_TIMEOUT).send().await?;
            let resp = checked(resp, repo).await?;
            next = resp.headers().get(LINK).and_then(next_link);
            files.extend(resp.json::<Vec<TreeFile>>().await?);
        }
        Ok(files)
    }

    /// Reads the repo's GGUF architecture, or None when Hugging Face has not parsed one.
    pub async fn architecture(&self, repo: &str) -> Result<Option<String>> {
        check_repo(repo)?;
        let url = Url::parse_with_params(&format!("{}/api/models/{repo}", self.base), &[("expand[]", "gguf")])?;
        let resp = self.auth(self.client.get(url)).timeout(API_TIMEOUT).send().await?;
        let model: ApiModelGgufOnly = checked(resp, repo).await?.json().await?;
        Ok(model.gguf.and_then(|g| g.architecture))
    }

    /// The direct download URL for a repo file.
    pub fn file_url(&self, repo: &str, file: &str) -> Result<Url> {
        Url::parse(&format!("{}/{repo}/resolve/main/{file}", self.base)).context("build the download URL")
    }
}

#[derive(Deserialize)]
struct ApiModelGgufOnly {
    gguf: Option<ApiGguf>,
}

async fn checked(resp: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    match status.as_u16() {
        401 | 403 => bail!("Hugging Face refused access to {what} ({status}). If the repo is gated, accept its terms on huggingface.co and set HF_TOKEN."),
        404 => bail!("Hugging Face has no repo named {what} ({status}). Check the name and try again."),
        _ => bail!("Hugging Face returned {status} for {what}. Try again in a moment."),
    }
}

fn next_link(value: &HeaderValue) -> Option<String> {
    let value = value.to_str().ok()?;
    value.split(',').find(|part| part.contains("rel=\"next\"")).and_then(|part| {
        let start = part.find('<')? + 1;
        let end = part.find('>')?;
        part.get(start..end).map(str::to_string)
    })
}

fn hit_from(model: ApiModel) -> SearchHit {
    let architecture = model.gguf.and_then(|g| g.architecture);
    SearchHit {
        repo: model.id,
        downloads: model.downloads,
        likes: model.likes,
        supported: architecture.as_deref().is_some_and(|a| SUPPORTED_ARCHITECTURES.contains(&a)),
        architecture,
        updated: model.last_modified,
    }
}

/// Rejects anything but `owner/name`, since repo names become URL paths and directory names.
pub fn check_repo(repo: &str) -> Result<()> {
    let safe = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    match repo.split_once('/') {
        Some((owner, name)) if safe(owner) && safe(name) => Ok(()),
        _ => Err(anyhow!("`{repo}` is not a repo name. Use the form owner/name.")),
    }
}

/// The quant tag in a GGUF file name, upper-cased, e.g. `Q4_K_M` or `BF16`.
pub fn quant_of(file_name: &str) -> Option<String> {
    let base = file_name.rsplit('/').next().unwrap_or(file_name);
    let stem = base.strip_suffix(".gguf").unwrap_or(base);
    let stem = shard_of(base).map_or(stem, |(prefix, _, _)| prefix);
    let upper = stem.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    let at_boundary = |i: usize| i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
    let is_tag_start = |i: usize| {
        let rest = &bytes[i..];
        let q = match rest {
            [b'I' | b'T', b'Q', d, ..] if d.is_ascii_digit() => true,
            [b'Q', d, ..] if d.is_ascii_digit() => true,
            _ => false,
        };
        q || rest.starts_with(b"F16")
            || rest.starts_with(b"F32")
            || rest.starts_with(b"BF16")
            || rest.starts_with(b"FP16")
            || rest.starts_with(b"FP32")
    };
    let start = (0..bytes.len()).rev().find(|&i| at_boundary(i) && is_tag_start(i))?;
    let tag = upper[start..].split(['-', '.']).next()?;
    Some(tag.to_string())
}

/// Splits `base-00001-of-00004.gguf` into its prefix, one-based index and shard count.
pub fn shard_of(file_name: &str) -> Option<(&str, u32, u32)> {
    let stem = file_name.strip_suffix(".gguf")?;
    let (head, total) = stem.rsplit_once("-of-")?;
    let (prefix, index) = head.rsplit_once('-')?;
    if index.len() != 5 || total.len() != 5 {
        return None;
    }
    Some((prefix, index.parse().ok()?, total.parse().ok()?))
}

fn is_model_file(path: &str) -> bool {
    let lower = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    lower.ends_with(".gguf") && !lower.contains("mmproj") && !lower.contains("imatrix")
}

/// Groups a repo tree by quant, keeping one complete file set per quant, smallest first.
pub fn group_quants(tree: &[TreeFile]) -> Vec<(String, Vec<TreeFile>)> {
    let mut by_set: BTreeMap<(String, String), Vec<TreeFile>> = BTreeMap::new();
    for file in tree.iter().filter(|f| is_model_file(&f.path)) {
        let base = file.path.rsplit('/').next().unwrap_or(&file.path);
        let set = shard_of(base).map_or(base, |(prefix, _, _)| prefix).to_string();
        if let Some(quant) = quant_of(&file.path) {
            by_set.entry((quant, set)).or_default().push(file.clone());
        }
    }
    let mut groups: BTreeMap<String, Vec<TreeFile>> = BTreeMap::new();
    for ((quant, _), mut files) in by_set {
        files.sort_by(|a, b| a.path.cmp(&b.path));
        if !groups.contains_key(&quant) && is_complete(&files) {
            groups.insert(quant, files);
        }
    }
    let mut out: Vec<_> = groups.into_iter().collect();
    out.sort_by_key(|(_, files)| files.iter().map(|f| f.size).sum::<u64>());
    out
}

fn is_complete(files: &[TreeFile]) -> bool {
    let name = |f: &TreeFile| f.path.rsplit('/').next().unwrap_or(&f.path).to_string();
    match files.first().map(name).as_deref().and_then(shard_of) {
        Some((_, _, total)) => files.len() == total as usize,
        None => files.len() == 1,
    }
}

/// Decides where a model of `size` bytes can run given VRAM and free RAM in bytes.
pub fn fit(size: u64, vram: u64, free_ram: u64) -> Fit {
    let needed = size.saturating_add(size / 5).saturating_add(GIB);
    if needed <= vram {
        Fit::Gpu
    } else if needed <= free_ram / 5 * 4 {
        Fit::Cpu
    } else {
        Fit::None
    }
}

/// Builds the wire listing for a repo tree and marks the recommended quant.
pub fn quant_listing(tree: &[TreeFile], vram: u64, free_ram: u64) -> Vec<QuantFiles> {
    let mut listing: Vec<QuantFiles> = group_quants(tree)
        .into_iter()
        .map(|(quant, files)| {
            let size = files.iter().map(|f| f.size).sum();
            QuantFiles {
                quant,
                files: files.into_iter().map(|f| f.path).collect(),
                size,
                fit: fit(size, vram, free_ram),
                recommended: false,
            }
        })
        .collect();
    let best = listing
        .iter_mut()
        .filter(|q| q.fit != Fit::None)
        .min_by_key(|q| (q.fit == Fit::Cpu, preference_rank(&q.quant), q.size));
    if let Some(best) = best {
        best.recommended = true;
    }
    listing
}

fn preference_rank(quant: &str) -> usize {
    QUANT_PREFERENCE.iter().position(|q| *q == quant).unwrap_or(QUANT_PREFERENCE.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, size: u64) -> TreeFile {
        TreeFile { path: path.to_string(), size, lfs: None }
    }

    #[test]
    fn quant_tags_come_from_file_names() {
        let cases = [
            ("Qwen3-0.6B-Q4_K_M.gguf", Some("Q4_K_M")),
            ("Qwen3-0.6B-BF16.gguf", Some("BF16")),
            ("Qwen3-0.6B-IQ4_XS.gguf", Some("IQ4_XS")),
            ("model.q5_k_s.gguf", Some("Q5_K_S")),
            ("Model-UD-Q4_K_XL-00002-of-00003.gguf", Some("Q4_K_XL")),
            ("Q4_K_M/Big-Q4_K_M-00001-of-00002.gguf", Some("Q4_K_M")),
            ("Foo_Q8_0.gguf", Some("Q8_0")),
            ("Qwen3.gguf", None),
        ];
        for (name, want) in cases {
            assert_eq!(quant_of(name).as_deref(), want, "{name}");
        }
    }

    #[test]
    fn shard_names_parse_and_plain_names_do_not() {
        assert_eq!(shard_of("A-Q4-00002-of-00004.gguf"), Some(("A-Q4", 2, 4)));
        assert_eq!(shard_of("A-Q4.gguf"), None);
        assert_eq!(shard_of("A-1-of-2.gguf"), None);
    }

    #[test]
    fn shards_group_under_one_quant_and_sum_size() {
        let tree = [
            file("Q4/M-Q4_K_M-00002-of-00002.gguf", 20),
            file("Q4/M-Q4_K_M-00001-of-00002.gguf", 10),
            file("M-Q8_0.gguf", 50),
            file("mmproj-F16.gguf", 5),
            file("README.md", 1),
        ];
        let listing = quant_listing(&tree, 0, 0);
        assert_eq!(listing.len(), 2);
        assert_eq!(listing[0].quant, "Q4_K_M");
        assert_eq!(listing[0].size, 30);
        assert_eq!(
            listing[0].files,
            ["Q4/M-Q4_K_M-00001-of-00002.gguf", "Q4/M-Q4_K_M-00002-of-00002.gguf"]
        );
    }

    #[test]
    fn incomplete_shard_set_is_dropped() {
        let tree = [file("M-Q4_K_M-00001-of-00003.gguf", 10)];
        assert!(group_quants(&tree).is_empty());
    }

    #[test]
    fn fit_prefers_gpu_then_cpu_then_none() {
        let size = 10 * GIB;
        let needed = 12 * GIB + GIB;
        assert_eq!(fit(size, needed, 0), Fit::Gpu);
        assert_eq!(fit(size, needed - 1, needed * 2), Fit::Cpu);
        assert_eq!(fit(size, needed - 1, needed), Fit::None);
        assert_eq!(fit(size, 0, 0), Fit::None);
    }

    #[test]
    fn recommendation_prefers_q4_k_m_when_it_fits() {
        let tree = [file("M-Q2_K.gguf", GIB), file("M-Q4_K_M.gguf", 2 * GIB), file("M-Q8_0.gguf", 4 * GIB)];
        let listing = quant_listing(&tree, 16 * GIB, 0);
        let picked: Vec<_> = listing.iter().filter(|q| q.recommended).map(|q| q.quant.as_str()).collect();
        assert_eq!(picked, ["Q4_K_M"]);
    }

    #[test]
    fn recommendation_steps_down_to_what_fits_on_the_gpu() {
        let tree = [file("M-Q2_K.gguf", GIB), file("M-Q4_K_M.gguf", 8 * GIB)];
        let listing = quant_listing(&tree, 3 * GIB, 64 * GIB);
        let picked: Vec<_> = listing.iter().filter(|q| q.recommended).map(|q| q.quant.as_str()).collect();
        assert_eq!(picked, ["Q2_K"]);
    }

    #[test]
    fn nothing_is_recommended_when_nothing_fits() {
        let listing = quant_listing(&[file("M-Q4_K_M.gguf", 8 * GIB)], 0, 0);
        assert!(listing.iter().all(|q| !q.recommended));
    }

    #[test]
    fn repo_names_cannot_escape_the_model_dir() {
        assert!(check_repo("unsloth/Qwen3-0.6B-GGUF").is_ok());
        for bad in ["..", "a/..", "../a", "a/b/c", "a\\b/c", "/a", "a/"] {
            assert!(check_repo(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn supported_list_matches_the_vendored_engine() {
        let source = include_str!("../../../vendor/infr/crates/infr-llama/src/arch.rs");
        let mut found: Vec<&str> = source
            .lines()
            .filter(|l| l.starts_with("pub const ") && l.contains(": &str = \""))
            .filter_map(|l| l.split('"').nth(1))
            .collect();
        let mut ours = SUPPORTED_ARCHITECTURES.to_vec();
        found.sort_unstable();
        ours.sort_unstable();
        assert_eq!(found, ours);
    }
}
