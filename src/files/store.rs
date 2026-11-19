//! Where received files are kept until they are pasted.
//!
//! Every transfer gets a folder of its own below [`received_dir`]; the
//! clipboard then points at its top level items, and pasting them in a file
//! manager copies them to wherever the user pastes. Only the latest transfers
//! are kept.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::{fs::File, io::AsyncWriteExt};

use super::wire::Entry;

/// how many received transfers are kept
pub(crate) const KEEP_TRANSFERS: usize = 3;

/// folder holding the received transfers
pub(crate) fn received_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Caches"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")));
    base.map(|b| b.join("rustinput").join("received"))
}

/// Removes all but the `keep` newest transfers below `dir`.
pub(crate) fn cleanup(dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    // the folder names start with the time they were created
    let mut transfers: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    transfers.sort();
    let remove = transfers.len().saturating_sub(keep);
    for old in &transfers[..remove] {
        if let Err(e) = fs::remove_dir_all(old) {
            log::warn!("could not remove the old transfer {old:?}: {e}");
        }
    }
}

/// a file name that is valid on this system
fn safe_name(name: &str) -> String {
    let mut name: String = name
        .chars()
        .map(|c| if c == '\0' { '_' } else { c })
        .collect();
    if cfg!(windows) {
        name = name
            .chars()
            .map(|c| {
                if c.is_control() || matches!(c, '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        // Windows drops trailing dots and spaces
        while name.ends_with(['.', ' ']) {
            name.pop();
        }
        let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.len() == 4
                && stem.as_bytes()[3].is_ascii_digit());
        if reserved {
            name.insert(0, '_');
        }
    }
    if name.is_empty() {
        name.push('_');
    }
    name
}

/// Turns a path of an offer into a relative path inside the transfer
/// folder, `None` if it tries to leave it.
pub(crate) fn safe_path(path: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return None;
        }
        out.push(safe_name(component));
    }
    Some(out)
}

/// a file being written
struct Current {
    file: File,
    path: PathBuf,
    remaining: u64,
    modified: Option<u64>,
}

/// the receiving side of a transfer
pub(crate) struct Receiving {
    root: PathBuf,
    entries: Vec<(Entry, PathBuf)>,
    next: usize,
    current: Option<Current>,
    pub(crate) files: usize,
    pub(crate) bytes: u64,
    pub(crate) files_done: usize,
    pub(crate) bytes_done: u64,
}

fn time(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

impl Receiving {
    /// Creates the folder of the transfer below `dir` with the folders of the
    /// offer, ready for the contents of its files.
    pub(crate) async fn start(dir: &Path, id: u32, entries: Vec<Entry>) -> io::Result<Self> {
        let mut checked = Vec::with_capacity(entries.len());
        for entry in entries {
            let Some(path) = safe_path(&entry.path) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsafe path in the offer: {:?}", entry.path),
                ));
            };
            checked.push((entry, path));
        }
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let root = dir.join(format!("{created:015}-{id}"));
        tokio::fs::create_dir_all(&root).await?;
        let files = checked.iter().filter(|(e, _)| !e.dir).count();
        let bytes = checked.iter().map(|(e, _)| e.size).sum();
        let mut receiving = Self {
            root,
            entries: checked,
            next: 0,
            current: None,
            files,
            bytes,
            files_done: 0,
            bytes_done: 0,
        };
        if let Err(e) = receiving.advance().await {
            receiving.discard().await;
            return Err(e);
        }
        Ok(receiving)
    }

    /// creates the next entries up to the next file that has contents
    async fn advance(&mut self) -> io::Result<()> {
        while self.current.is_none() && self.next < self.entries.len() {
            let (entry, relative) = &self.entries[self.next];
            self.next += 1;
            let path = self.root.join(relative);
            if entry.dir {
                tokio::fs::create_dir_all(&path).await?;
                continue;
            }
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            // never overwrite or follow anything that is already there
            let file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await?;
            let current = Current {
                file,
                path,
                remaining: entry.size,
                modified: entry.modified,
            };
            if entry.size == 0 {
                self.finish_file(current).await?;
            } else {
                self.current = Some(current);
            }
        }
        Ok(())
    }

    async fn finish_file(&mut self, mut current: Current) -> io::Result<()> {
        current.file.flush().await?;
        let file = current.file.into_std().await;
        if let Some(modified) = current.modified {
            if let Err(e) = file.set_modified(time(modified)) {
                log::debug!("could not set the time of {:?}: {e}", current.path);
            }
        }
        self.files_done += 1;
        Ok(())
    }

    /// the next bytes of the transfer
    pub(crate) async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let Some(current) = self.current.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "more data than the offer announced",
            ));
        };
        if bytes.len() as u64 > current.remaining {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a chunk goes past the end of {:?}", current.path),
            ));
        }
        current.file.write_all(bytes).await?;
        current.remaining -= bytes.len() as u64;
        self.bytes_done += bytes.len() as u64;
        if current.remaining == 0 {
            let current = self.current.take().expect("current file");
            self.finish_file(current).await?;
            self.advance().await?;
        }
        Ok(())
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.current.is_none() && self.next == self.entries.len()
    }

    /// name of the file being received
    pub(crate) fn current_name(&self) -> Option<String> {
        self.current
            .as_ref()
            .and_then(|c| c.path.file_name())
            .map(|n| n.to_string_lossy().into_owned())
    }

    /// Finishes a complete transfer: sets the times of the folders and
    /// returns the copied items, which go to the clipboard.
    pub(crate) fn finish(self) -> Vec<PathBuf> {
        // after their contents, which change the time of a folder
        for (entry, relative) in self.entries.iter().rev() {
            if let (true, Some(modified)) = (entry.dir, entry.modified) {
                let path = self.root.join(relative);
                if let Ok(dir) = fs::File::open(&path) {
                    let _ = dir.set_modified(time(modified));
                }
            }
        }
        let mut top = Vec::new();
        for (_, relative) in &self.entries {
            if let Some(first) = relative.components().next() {
                let path = self.root.join(first);
                if !top.contains(&path) {
                    top.push(path);
                }
            }
        }
        top
    }

    /// removes everything of an incomplete transfer
    pub(crate) async fn discard(self) {
        drop(self.current);
        if let Err(e) = tokio::fs::remove_dir_all(&self.root).await {
            log::warn!("could not remove {:?}: {e}", self.root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, dir: bool, size: u64) -> Entry {
        Entry {
            path: path.into(),
            dir,
            size,
            modified: Some(1_600_000_000),
        }
    }

    #[test]
    fn paths_can_not_leave_the_transfer() {
        assert_eq!(safe_path("a/b.txt"), Some(PathBuf::from("a").join("b.txt")));
        assert_eq!(safe_path("../etc/passwd"), None);
        assert_eq!(safe_path("a/../../x"), None);
        assert_eq!(safe_path("/etc/passwd"), None);
        assert_eq!(safe_path("a//b"), None);
        assert_eq!(safe_path("."), None);
        assert_eq!(safe_path(""), None);
        assert_eq!(safe_path("a\0b"), Some(PathBuf::from("a_b")));
    }

    #[cfg(windows)]
    #[test]
    fn names_valid_on_windows() {
        assert_eq!(safe_path("a:b?.txt"), Some(PathBuf::from("a_b_.txt")));
        assert_eq!(safe_path("con.txt"), Some(PathBuf::from("_con.txt")));
        assert_eq!(safe_path("notas. "), Some(PathBuf::from("notas")));
        assert_eq!(safe_path("C:\\x"), Some(PathBuf::from("C__x")));
    }

    #[tokio::test]
    async fn a_transfer_is_written_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            entry("Fotos", true, 0),
            entry("Fotos/vazio.txt", false, 0),
            entry("Fotos/a.bin", false, 5),
            entry("Fotos/sub", true, 0),
            entry("leia.txt", false, 3),
        ];
        let mut receiving = Receiving::start(dir.path(), 9, entries).await.unwrap();
        assert_eq!((receiving.files, receiving.bytes), (3, 8));
        // the empty file exists before any data arrived
        assert_eq!(receiving.files_done, 1);
        assert_eq!(receiving.current_name().as_deref(), Some("a.bin"));
        receiving.write(b"abc").await.unwrap();
        receiving.write(b"de").await.unwrap();
        assert_eq!(receiving.current_name().as_deref(), Some("leia.txt"));
        assert!(receiving.write(b"toolong").await.is_err());
        receiving.write(b"xyz").await.unwrap();
        assert!(receiving.is_complete());
        assert!(receiving.write(b"!").await.is_err());
        assert_eq!(receiving.bytes_done, 8);
        let top = receiving.finish();
        assert_eq!(top.len(), 2);
        assert!(top[0].ends_with("Fotos"));
        assert!(top[1].ends_with("leia.txt"));
        assert_eq!(fs::read(top[0].join("a.bin")).unwrap(), b"abcde");
        assert_eq!(fs::read(&top[1]).unwrap(), b"xyz");
        assert!(top[0].join("vazio.txt").is_file());
        assert!(top[0].join("sub").is_dir());
        let modified = fs::metadata(&top[1]).unwrap().modified().unwrap();
        assert_eq!(modified, time(1_600_000_000));
    }

    #[tokio::test]
    async fn unsafe_offers_are_refused_and_nothing_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![entry("ok.txt", false, 1), entry("../fora.txt", false, 1)];
        assert!(Receiving::start(dir.path(), 1, entries).await.is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        // a file and a folder of the same name
        let entries = vec![entry("x", false, 0), entry("x", true, 0)];
        assert!(Receiving::start(dir.path(), 2, entries).await.is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn discarding_removes_the_partial_transfer() {
        let dir = tempfile::tempdir().unwrap();
        let mut receiving = Receiving::start(dir.path(), 3, vec![entry("big", false, 100)])
            .await
            .unwrap();
        receiving.write(&[0; 50]).await.unwrap();
        receiving.discard().await;
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn only_the_newest_transfers_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "000000000000001-1",
            "000000000000003-1",
            "000000000000002-7",
        ] {
            fs::create_dir(dir.path().join(name)).unwrap();
        }
        cleanup(dir.path(), 2);
        let mut left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["000000000000002-7", "000000000000003-1"]);
    }
}
