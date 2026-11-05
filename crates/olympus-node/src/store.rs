use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

const META_FILE: &str = "meta.json";

/// A downloaded model in the wire format.
#[derive(Clone, Debug, Serialize)]
pub struct LocalModel {
    pub id: String,
    pub repo: String,
    pub quant: String,
    pub path: String,
    pub size: u64,
    pub architecture: Option<String>,
    pub verified: bool,
}

/// One quant recorded in a repo directory's meta.json.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetaEntry {
    pub repo: String,
    pub quant: String,
    pub architecture: Option<String>,
    pub files: Vec<String>,
    /// False when a torrent's files had no published checksum to compare against.
    #[serde(default = "always")]
    pub verified: bool,
}

fn always() -> bool {
    true
}

/// The arguments of the last model that reached ready.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LastUsed {
    pub id: String,
    pub ctx: Option<String>,
    pub parallel: Option<u32>,
}

/// The on-disk model library.
pub struct Store {
    root: PathBuf,
    meta_lock: Mutex<()>,
}

/// The directory name for a repo.
pub fn repo_dir_name(repo: &str) -> String {
    repo.replace('/', "__")
}

/// The stable, URL-safe id of a repo quant.
pub fn model_id(repo: &str, quant: &str) -> String {
    format!("{}__{quant}", repo_dir_name(repo))
}

/// The flat local name of a repo file; repo subfolders are dropped.
pub fn local_name(repo_file: &str) -> Result<&str> {
    let name = repo_file.rsplit('/').next().unwrap_or(repo_file);
    let safe = !name.is_empty() && name != ".." && !name.contains(['\\', ':']);
    safe.then_some(name).ok_or_else(|| anyhow!("Unsafe file name in the repo: {repo_file}"))
}

impl Store {
    /// Opens the library at the given root.
    pub fn new(root: PathBuf) -> Self {
        Self { root, meta_lock: Mutex::new(()) }
    }

    /// The directory that holds every repo.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory for one repo's files, created if missing.
    pub fn repo_dir(&self, repo: &str) -> Result<PathBuf> {
        let dir = self.root.join(repo_dir_name(repo));
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        Ok(dir)
    }

    /// Records a completed quant in its repo's meta.json.
    pub fn add(&self, entry: MetaEntry) -> Result<()> {
        let _guard = self.meta_lock.lock().unwrap_or_else(|e| e.into_inner());
        let dir = self.repo_dir(&entry.repo)?;
        let mut entries = read_meta(&dir);
        entries.retain(|e| e.quant != entry.quant);
        entries.push(entry);
        write_atomic(&dir.join(META_FILE), &serde_json::to_vec_pretty(&entries)?)
    }

    /// Lists complete local models, sorted by id.
    pub fn list(&self) -> Vec<LocalModel> {
        let Ok(dirs) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut models: Vec<LocalModel> = dirs
            .flatten()
            .flat_map(|dir| {
                let path = dir.path();
                read_meta(&path).into_iter().filter_map(move |e| local_model(&path, &e))
            })
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        models
    }

    /// Finds a local model by id.
    pub fn get(&self, id: &str) -> Option<LocalModel> {
        self.list().into_iter().find(|m| m.id == id)
    }

    /// Deletes a model's files and its meta entry, staying inside the library root.
    pub fn delete(&self, id: &str) -> Result<bool> {
        let _guard = self.meta_lock.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(dirs) = fs::read_dir(&self.root) else {
            return Ok(false);
        };
        let root = fs::canonicalize(&self.root).context("resolve the model directory")?;
        for dir in dirs.flatten().map(|d| d.path()) {
            let mut entries = read_meta(&dir);
            let Some(pos) = entries.iter().position(|e| model_id(&e.repo, &e.quant) == id) else {
                continue;
            };
            let entry = entries.remove(pos);
            for name in &entry.files {
                let file = dir.join(local_name(name)?);
                match fs::canonicalize(&file) {
                    Ok(real) if real.starts_with(&root) => {
                        fs::remove_file(&real).with_context(|| format!("delete {}", file.display()))?;
                    }
                    Ok(real) => anyhow::bail!("{} is outside the model directory", real.display()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e).with_context(|| format!("resolve {}", file.display())),
                }
            }
            let meta = dir.join(META_FILE);
            if entries.is_empty() {
                fs::remove_file(&meta).context("delete meta.json")?;
                fs::remove_dir(&dir).ok();
            } else {
                write_atomic(&meta, &serde_json::to_vec_pretty(&entries)?)?;
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Reads the last-used record, if any.
    pub fn last_used(&self) -> Option<LastUsed> {
        serde_json::from_slice(&fs::read(self.last_path()?).ok()?).ok()
    }

    /// Saves the last-used record.
    pub fn set_last_used(&self, last: &LastUsed) -> Result<()> {
        let path = self.last_path().ok_or_else(|| anyhow!("the model directory has no parent"))?;
        write_atomic(&path, &serde_json::to_vec_pretty(last)?)
    }

    fn last_path(&self) -> Option<PathBuf> {
        Some(self.root.parent()?.join("last.json"))
    }
}

fn read_meta(dir: &Path) -> Vec<MetaEntry> {
    fs::read(dir.join(META_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn local_model(dir: &Path, entry: &MetaEntry) -> Option<LocalModel> {
    let mut files: Vec<PathBuf> =
        entry.files.iter().map(|f| Some(dir.join(local_name(f).ok()?))).collect::<Option<_>>()?;
    files.sort();
    let size = files.iter().map(|f| fs::metadata(f).map(|m| m.len()).ok()).sum::<Option<u64>>()?;
    Some(LocalModel {
        id: model_id(&entry.repo, &entry.quant),
        repo: entry.repo.clone(),
        quant: entry.quant.clone(),
        path: files.first()?.to_string_lossy().into_owned(),
        size,
        architecture: entry.architecture.clone(),
        verified: entry.verified,
    })
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> Store {
        let root = std::env::temp_dir().join(format!("olympus-store-test-{}-{name}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).unwrap();
        Store::new(root.join("models"))
    }

    fn add_model(store: &Store, quant: &str, files: &[&str]) {
        let dir = store.repo_dir("org/repo").unwrap();
        for f in files {
            fs::write(dir.join(f), b"12345").unwrap();
        }
        store
            .add(MetaEntry {
                repo: "org/repo".to_string(),
                quant: quant.to_string(),
                architecture: Some("qwen3".to_string()),
                files: files.iter().map(|f| f.to_string()).collect(),
                verified: true,
            })
            .unwrap();
    }

    #[test]
    fn lists_shards_as_one_model_pointing_at_the_first() {
        let store = store("list");
        add_model(&store, "Q4_K_M", &["m-Q4_K_M-00002-of-00002.gguf", "m-Q4_K_M-00001-of-00002.gguf"]);

        let models = store.list();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "org__repo__Q4_K_M");
        assert_eq!(models[0].size, 10);
        assert!(models[0].path.ends_with("m-Q4_K_M-00001-of-00002.gguf"));
    }

    #[test]
    fn delete_removes_only_the_named_quant() {
        let store = store("delete");
        add_model(&store, "Q4_K_M", &["m-Q4_K_M.gguf"]);
        add_model(&store, "Q8_0", &["m-Q8_0.gguf"]);

        assert!(store.delete("org__repo__Q4_K_M").unwrap());

        let dir = store.repo_dir("org/repo").unwrap();
        assert!(!dir.join("m-Q4_K_M.gguf").exists());
        assert!(dir.join("m-Q8_0.gguf").exists());
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn delete_of_an_unknown_id_changes_nothing() {
        let store = store("unknown");
        add_model(&store, "Q4_K_M", &["m-Q4_K_M.gguf"]);

        assert!(!store.delete("..__..__x").unwrap());

        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn traversal_in_meta_never_escapes_the_library() {
        let store = store("traversal");
        let dir = store.repo_dir("org/repo").unwrap();
        let outside = store.root().parent().unwrap().join("outside.gguf");
        fs::write(&outside, b"keep").unwrap();
        let meta = r#"[{"repo":"org/repo","quant":"Q4","architecture":null,"files":["..\\..\\outside.gguf"]}]"#;
        fs::write(dir.join(META_FILE), meta).unwrap();

        assert!(store.delete("org__repo__Q4").is_err());

        assert!(outside.exists());
    }

    #[test]
    fn local_names_drop_folders_and_refuse_escapes() {
        assert_eq!(local_name("Q4/m.gguf").unwrap(), "m.gguf");
        assert!(local_name("a/..").is_err());
        assert!(local_name("..\\x.gguf").is_err());
    }
}
