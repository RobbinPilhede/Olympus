//! Which files change under a folder, as told by Windows, so the daily note can say what got saved.
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use windows::core::HSTRING;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

const NOISE_FOLDERS: [&str; 10] = ["appdata", "node_modules", "target", "dist", "build", "obj", "__pycache__", "venv", "$recycle.bin", "temp"];
const NOISE_FILES: [&str; 2] = ["desktop.ini", "thumbs.db"];
const NOISE_ENDINGS: [&str; 14] = ["tmp", "temp", "log", "lock", "crdownload", "part", "partial", "swp", "lnk", "db-wal", "db-shm", "pyc", "etl", "opdownload"];
const BUFFER_WORDS: usize = 16 * 1024;

/// What happened to a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Saved,
    Deleted,
}

impl Action {
    /// The name stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Saved => "saved",
            Self::Deleted => "deleted",
        }
    }
}

/// One file changing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub path: PathBuf,
    pub action: Action,
}

/// True for caches, build output and hidden folders, which change constantly without you doing anything.
pub fn is_noise(rel: &Path) -> bool {
    let parts: Vec<String> = rel.iter().map(|part| part.to_string_lossy().to_lowercase()).collect();
    let Some((name, folders)) = parts.split_last() else { return true };
    let hidden_folder = folders.iter().any(|folder| folder.starts_with('.') || NOISE_FOLDERS.contains(&folder.as_str()));
    let ending = name.rsplit_once('.').map_or("", |(_, ending)| ending);
    hidden_folder || name.starts_with(['.', '~']) || name.ends_with('~') || NOISE_FILES.contains(&name.as_str()) || NOISE_ENDINGS.contains(&ending)
}

/// Reads the records `ReadDirectoryChangesW` wrote, checking every offset.
pub fn decode(bytes: &[u8]) -> Vec<(u32, String)> {
    let word = |at: usize| bytes.get(at..at + 4).map(|part| u32::from_le_bytes([part[0], part[1], part[2], part[3]]));
    let mut records = Vec::new();
    let mut at = 0;
    while let (Some(next), Some(action), Some(length)) = (word(at), word(at + 4), word(at + 8)) {
        let Some(name) = bytes.get(at + 12..at + 12 + length as usize) else { break };
        let units: Vec<u16> = name.chunks_exact(2).map(|unit| u16::from_le_bytes([unit[0], unit[1]])).collect();
        records.push((action, String::from_utf16_lossy(&units)));
        if next == 0 {
            break;
        }
        at += next as usize;
    }
    records
}

fn change(root: &Path, action: u32, rel: &str) -> Option<Change> {
    let action = match action {
        a if a == FILE_ACTION_ADDED.0 || a == FILE_ACTION_MODIFIED.0 || a == FILE_ACTION_RENAMED_NEW_NAME.0 => Action::Saved,
        a if a == FILE_ACTION_REMOVED.0 => Action::Deleted,
        _ => return None,
    };
    (!is_noise(Path::new(rel))).then(|| Change { path: root.join(rel), action })
}

/// Watches a folder and everything in it until the receiver is gone.
pub fn watch(root: PathBuf, tx: Sender<Change>) -> std::io::Result<()> {
    // SAFETY: the name outlives the call, and the handle is closed by the thread below.
    let handle = unsafe { CreateFileW(&HSTRING::from(root.as_os_str()), FILE_LIST_DIRECTORY.0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, None, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, None) }?;
    let handle_value = handle.0 as isize;
    std::thread::Builder::new().name("obi-files".into()).spawn(move || {
        let handle = windows::Win32::Foundation::HANDLE(handle_value as *mut _);
        // A u32 buffer, because Windows needs the records word-aligned.
        let mut buffer = vec![0u32; BUFFER_WORDS];
        loop {
            let mut written = 0u32;
            // SAFETY: the buffer outlives this blocking call, and its length is given in bytes.
            let read = unsafe { ReadDirectoryChangesW(handle, buffer.as_mut_ptr().cast(), (buffer.len() * 4) as u32, true, FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE, Some(&raw mut written), None, None) };
            if read.is_err() {
                break;
            }
            // SAFETY: Windows wrote `written` bytes into this u32 buffer, which is at least that long.
            let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), written as usize) };
            // Stopping can only be noticed here, so an old watcher lasts until the next file changes.
            if decode(bytes).iter().filter_map(|(action, rel)| change(&root, *action, rel)).any(|found| tx.send(found).is_err()) {
                break;
            }
        }
        // SAFETY: this thread is the only owner of the handle.
        let _ = unsafe { CloseHandle(handle) };
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn caches_and_build_output_are_noise_and_your_files_are_not() {
        for noise in [r"AppData\Local\x.txt", r"Desktop\app\target\debug\a.exe", r"Desktop\app\.git\index", r"Documents\~$report.docx", r"Downloads\movie.crdownload", r"Desktop\desktop.ini", r".olympus\obiview.db"] {
            assert!(is_noise(Path::new(noise)), "{noise}");
        }
        for real in [r"Documents\report.docx", r"Desktop\app\src\main.rs", "notes.txt", r"Downloads\invoice.pdf"] {
            assert!(!is_noise(Path::new(real)), "{real}");
        }
    }

    #[test]
    fn decoding_survives_a_cut_off_buffer() {
        let mut bytes = Vec::new();
        for value in [20u32, 1, 4] {
            bytes.extend(value.to_le_bytes());
        }
        bytes.extend("a.".encode_utf16().flat_map(u16::to_le_bytes));
        bytes.extend([0u8; 4]);
        for value in [0u32, 2, 4000] {
            bytes.extend(value.to_le_bytes());
        }
        assert_eq!(decode(&bytes), [(1, "a.".to_string())], "the second record claims more than was written");
        assert!(decode(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn a_real_folder_reports_saves_and_deletes_but_not_noise() {
        let dir = tempfile::tempdir().expect("dir");
        let (tx, rx) = std::sync::mpsc::channel();
        watch(dir.path().to_path_buf(), tx).expect("watch");
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(dir.path().join("skip.tmp"), "x").expect("write");
        let saved = dir.path().join("report.txt");
        std::fs::write(&saved, "x").expect("write");
        let first = rx.recv_timeout(Duration::from_secs(5)).expect("told about the save");
        assert_eq!(first, Change { path: saved.clone(), action: Action::Saved });
        std::fs::remove_file(&saved).expect("delete");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut deleted = false;
        while !deleted && std::time::Instant::now() < deadline {
            deleted = rx.recv_timeout(Duration::from_secs(1)).is_ok_and(|next| next.action == Action::Deleted);
        }
        assert!(deleted, "told about the delete");
    }
}
