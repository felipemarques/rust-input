//! Where received files are kept until they are pasted.
//!
//! Every transfer gets a folder of its own below
//! [`rustinput_ipc::received_dir`]; the clipboard then points at its top level
//! items, and pasting them in a file manager moves them to wherever the user
//! pastes. Folders left over by an earlier run are removed at startup.
//!
//! A resumable transfer (0.37.0) also gets `<its folder>.resume`, with the
//! device it comes from, its key and its entries: when its connection is
//! lost, what arrived stays, and when the same files are offered again the
//! transfer goes on at the first byte that is missing, found from the sizes
//! of the files on disk. Such folders outlive restarts for [`PARTIAL_MAX_AGE`].

use std::{
    fs, io,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{fs::File, io::AsyncWriteExt};

use super::wire::{Entry, Position, TAIL_HASH};

/// what both sides compare of a partial file before going on with it: the
/// SHA-256 of its last bytes, up to this many
pub(crate) const TAIL: u64 = 4 * 1024 * 1024;

/// partial transfers nobody went on with are removed after this long
const PARTIAL_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// the extension of the file kept next to the folder of a resumable transfer
const RESUME: &str = "resume";

/// `<folder>.resume`: what a resumable transfer is
#[derive(Serialize, Deserialize)]
struct Kept {
    /// fingerprint of the sending device
    from: String,
    key: String,
    entries: Vec<Entry>,
}

fn resume_file(root: &Path) -> PathBuf {
    let mut name = root.as_os_str().to_owned();
    name.push(".");
    name.push(RESUME);
    PathBuf::from(name)
}

/// Opens an existing plain file, not following a link in its place: the
/// daemon may run as SYSTEM in the folder of its user (see [`no_links`]).
pub(crate) fn open_plain(path: &Path, write: bool) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(write);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: a link (or junction) is opened
        // itself, and refused below
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{path:?} is a link"),
            ));
        }
    }
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{path:?} is not a file"),
        ));
    }
    Ok(file)
}

/// The SHA-256 of the last bytes of `file` before `offset` (up to
/// [`TAIL`]), what [`Position::tail`] holds. Blocking.
pub(crate) fn tail_hash(file: &mut fs::File, offset: u64) -> io::Result<[u8; TAIL_HASH]> {
    if offset == 0 {
        return Ok([0; TAIL_HASH]);
    }
    let start = offset.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(start))?;
    let mut hasher = Sha256::new();
    let mut left = offset - start;
    let mut buffer = vec![0; 256 * 1024];
    while left > 0 {
        let want = left.min(buffer.len() as u64) as usize;
        let read = file.read(&mut buffer[..want])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the file is shorter than its position",
            ));
        }
        hasher.update(&buffer[..read]);
        left -= read as u64;
    }
    Ok(hasher.finalize().into())
}

/// Whether `dir` and the folder it is in are folders and not links
/// (symbolic links, junctions): the daemon may run as SYSTEM in the folder of
/// its user, where a program of the user could put a junction to another
/// folder, to have SYSTEM write or delete there.
fn no_links(dir: &Path) -> bool {
    let plain =
        |path: &Path| fs::symlink_metadata(path).map_or(true, |m| !m.file_type().is_symlink());
    plain(dir) && dir.parent().is_none_or(plain)
}

/// The space free for this user and the size of the file system that
/// holds `dir`, in bytes.
pub(crate) fn space(dir: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
        // SAFETY: `path` is a valid C string and `stat` a buffer statvfs fills
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        let block = stat.f_frsize as u64;
        #[allow(clippy::unnecessary_cast)]
        Some((stat.f_bavail as u64 * block, stat.f_blocks as u64 * block))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let path: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
        let (mut free, mut total) = (0u64, 0u64);
        // SAFETY: `path` ends with a 0, the outputs are valid u64s
        let ok = unsafe {
            GetDiskFreeSpaceExW(path.as_ptr(), &mut free, &mut total, std::ptr::null_mut())
        };
        (ok != 0).then_some((free, total))
    }
}

/// what is kept free on the file system that receives files: 2 % of it,
/// between 256 MB and 2 GB
pub(crate) fn margin(total: u64) -> u64 {
    (total / 50).clamp(256 << 20, 2 << 30)
}

/// how long ago `path` was last changed
fn age(path: &Path) -> Duration {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .unwrap_or_default()
}

/// Removes all but the `keep` newest transfers below `dir`. Partial
/// resumable transfers stay, unless nothing went on with them for
/// [`PARTIAL_MAX_AGE`].
pub(crate) fn cleanup(dir: &Path, keep: usize) {
    if !no_links(dir) {
        log::warn!("{dir:?} or the folder it is in is a link: nothing is removed");
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    // the folder names start with the time they were created; links among
    // them are not followed
    let entries: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    // partial transfers nobody went on with for long, and `.resume` files
    // whose folder is gone
    for kept in entries
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == RESUME))
    {
        let root = kept.with_extension("");
        let folder = fs::symlink_metadata(&root).is_ok_and(|m| m.is_dir());
        if !folder || age(kept) > PARTIAL_MAX_AGE {
            remove_partial(&root);
        }
    }
    let mut transfers: Vec<PathBuf> = entries
        .into_iter()
        .filter(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
        .filter(|p| fs::symlink_metadata(resume_file(p)).is_err())
        .collect();
    transfers.sort();
    let remove = transfers.len().saturating_sub(keep);
    for old in &transfers[..remove] {
        if let Err(e) = fs::remove_dir_all(old) {
            log::warn!("could not remove the old transfer {old:?}: {e}");
        }
    }
}

/// the characters that change the direction of the text, which can make
/// "photo\u{202e}gpj.exe" look like "photoexe.jpg"
fn bidi(c: char) -> bool {
    matches!(c, '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// a file name that is valid on this system
fn safe_name(name: &str) -> String {
    let mut name: String = name
        .chars()
        .filter(|&c| !bidi(c))
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
        // "con .txt" and "COM¹" are devices too
        let stem = name
            .split('.')
            .next()
            .unwrap_or("")
            .trim_end_matches(' ')
            .to_ascii_uppercase();
        let device = |prefix: &str| {
            stem.strip_prefix(prefix).is_some_and(|n| {
                let mut digits = n.chars();
                digits
                    .next()
                    .is_some_and(|c| c.is_ascii_digit() || matches!(c, '¹' | '²' | '³'))
                    && digits.next().is_none()
            })
        };
        let reserved = matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || device("COM")
            || device("LPT");
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
    /// resumable: where the data goes on, until [`Receiving::begin`]
    waiting: Option<Position>,
    /// resumable: `<root>.resume`
    kept: Option<PathBuf>,
}

fn time(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

/// the paths of an offer inside the folder of the transfer, refused if one
/// would leave it
fn checked(entries: Vec<Entry>) -> io::Result<Vec<(Entry, PathBuf)>> {
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
    Ok(checked)
}

/// The folder of a partial transfer of `key` from `from` below `dir`, kept
/// with the same `entries`. Blocking.
fn find_kept(dir: &Path, from: &str, key: &str, entries: &[Entry]) -> Option<PathBuf> {
    for path in fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
    {
        if path.extension().is_none_or(|e| e != RESUME) {
            continue;
        }
        let Ok(text) = fs::read(&path) else {
            continue;
        };
        let Ok(kept) = serde_json::from_slice::<Kept>(&text) else {
            continue;
        };
        if kept.from != from || kept.key != key || kept.entries != entries {
            continue;
        }
        let root = path.with_extension("");
        if fs::symlink_metadata(&root).is_ok_and(|m| m.is_dir()) {
            return Some(root);
        }
    }
    None
}

impl Receiving {
    /// Creates the folder of the transfer below `dir` with the folders of the
    /// offer, ready for the contents of its files.
    pub(crate) async fn start(dir: &Path, id: u32, entries: Vec<Entry>) -> io::Result<Self> {
        let mut receiving = Self::create(dir, id, checked(entries)?).await?;
        if let Err(e) = receiving.advance().await {
            receiving.discard().await;
            return Err(e);
        }
        Ok(receiving)
    }

    /// The receiving side of a resumable transfer: what was kept of the
    /// same files from `from` (offered with the same `key`), or a new folder
    /// like [`Receiving::start`]. Returns where the data has to go on; the
    /// sender then says where it does, see [`Receiving::begin`].
    pub(crate) async fn resumable(
        dir: &Path,
        id: u32,
        from: &str,
        key: &str,
        entries: Vec<Entry>,
    ) -> io::Result<(Self, Position)> {
        let checked = checked(entries)?;
        if !no_links(dir) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{dir:?} or the folder it is in is a link, not a folder"),
            ));
        }
        let found = {
            let (dir, from, key) = (dir.to_path_buf(), from.to_owned(), key.to_owned());
            let entries: Vec<Entry> = checked.iter().map(|(e, _)| e.clone()).collect();
            tokio::task::spawn_blocking(move || find_kept(&dir, &from, &key, &entries))
                .await
                .map_err(io::Error::other)?
        };
        if let Some(root) = found {
            let kept = resume_file(&root);
            let (touched, again, entries) = (kept.clone(), root.clone(), checked.clone());
            let reopened = tokio::task::spawn_blocking(move || {
                // touched, so that it is not taken for forgotten
                let _ = open_plain(&touched, true).and_then(|f| f.set_modified(SystemTime::now()));
                Self::reopen(again, entries)
            })
            .await
            .map_err(io::Error::other)?;
            match reopened {
                Ok((mut receiving, position)) => {
                    receiving.kept = Some(kept);
                    receiving.waiting = Some(position);
                    return Ok((receiving, position));
                }
                // what was kept can not be gone on with: from the start
                Err(e) => {
                    log::warn!("{root:?}: can not go on with it ({e}), the files come again");
                    tokio::task::spawn_blocking(move || remove_partial(&root))
                        .await
                        .map_err(io::Error::other)?;
                }
            }
        }
        let mut receiving = Self::create(dir, id, checked).await?;
        let kept = resume_file(&receiving.root);
        let text = serde_json::to_vec(&Kept {
            from: from.to_owned(),
            key: key.to_owned(),
            entries: receiving.entries.iter().map(|(e, _)| e.clone()).collect(),
        })?;
        let written = async {
            // never over something already there
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&kept)
                .await?;
            file.write_all(&text).await?;
            file.flush().await
        };
        if let Err(e) = written.await {
            receiving.discard().await;
            return Err(e);
        }
        receiving.kept = Some(kept);
        receiving.waiting = Some(Position::default());
        Ok((receiving, Position::default()))
    }

    /// What is on disk of a partial transfer in `root`: every file before
    /// the first one that is shorter than it should be (or missing). What
    /// comes after it is removed, so it can be created again. Blocking.
    fn reopen(root: PathBuf, entries: Vec<(Entry, PathBuf)>) -> io::Result<(Self, Position)> {
        // Every folder inside the transfer is an entry: one that is not a
        // folder any more (a link, a junction) could lead the writes outside.
        for (entry, relative) in &entries {
            if entry.dir {
                if let Ok(m) = fs::symlink_metadata(root.join(relative)) {
                    if !m.is_dir() {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!("{relative:?} is not a folder any more"),
                        ));
                    }
                }
            }
        }
        let mut position = Position::default();
        let (mut files_done, mut bytes_done) = (0, 0);
        let mut next = entries.len();
        for (index, (entry, relative)) in entries.iter().enumerate() {
            if entry.dir {
                continue;
            }
            let path = root.join(relative);
            let length = match fs::symlink_metadata(&path) {
                Ok(m) if m.is_file() => Some(m.len()),
                Ok(_) => {
                    // not what was written there: a link, a folder
                    let _ = fs::remove_file(&path).or_else(|_| fs::remove_dir_all(&path));
                    None
                }
                Err(_) => None,
            };
            match length {
                Some(length) if length == entry.size => {
                    files_done += 1;
                    bytes_done += length;
                    position.file += 1;
                }
                Some(length) if length < entry.size => {
                    position.offset = length;
                    let mut file = open_plain(&path, false)?;
                    position.tail = tail_hash(&mut file, length)?;
                    bytes_done += length;
                    next = index;
                    break;
                }
                Some(_) => {
                    // longer than it should be: had again
                    fs::remove_file(&path)?;
                    next = index;
                    break;
                }
                None => {
                    next = index;
                    break;
                }
            }
        }
        // later files are created anew when their turn comes
        for (entry, relative) in entries.iter().skip(next + 1) {
            if !entry.dir {
                let path = root.join(relative);
                if fs::symlink_metadata(&path).is_ok() {
                    fs::remove_file(&path)?;
                }
            }
        }
        let files = entries.iter().filter(|(e, _)| !e.dir).count();
        let bytes = entries.iter().map(|(e, _)| e.size).sum();
        let receiving = Self {
            root,
            entries,
            next,
            current: None,
            files,
            bytes,
            files_done,
            bytes_done,
            waiting: None,
            kept: None,
        };
        Ok((receiving, position))
    }

    /// The sender begins the data at `file`, `offset` bytes into it: where
    /// [`Receiving::resumable`] said, or 0 to have that file again.
    pub(crate) async fn begin(&mut self, file: u32, offset: u64) -> io::Result<()> {
        let Some(at) = self.waiting.take() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the transfer began already",
            ));
        };
        if file != at.file || (offset != at.offset && offset != 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "asked to go on at file {} byte {}, the sender begins at file {file} byte {offset}",
                    at.file, at.offset
                ),
            ));
        }
        // the folders up to here, which may be empty
        for (entry, relative) in &self.entries[..self.next.min(self.entries.len())] {
            if entry.dir {
                tokio::fs::create_dir_all(self.root.join(relative)).await?;
            }
        }
        if self.next < self.entries.len() {
            let (entry, relative) = &self.entries[self.next];
            let path = self.root.join(relative);
            if offset == 0 && !entry.dir {
                // from its start: what is here (nothing of it yet, the file
                // created when the one before was complete, or what the
                // sender does not have) goes
                self.bytes_done -= at.offset;
                if fs::symlink_metadata(&path).is_ok() {
                    tokio::fs::remove_file(&path).await?;
                }
            } else if offset > 0 {
                let (size, modified) = (entry.size, entry.modified);
                let opened = path.clone();
                let file = tokio::task::spawn_blocking(move || {
                    let mut file = open_plain(&opened, true)?;
                    file.set_len(offset)?;
                    file.seek(SeekFrom::End(0))?;
                    Ok::<_, io::Error>(file)
                })
                .await
                .map_err(io::Error::other)??;
                self.next += 1;
                self.current = Some(Current {
                    file: File::from_std(file),
                    path,
                    remaining: size - offset,
                    modified,
                });
            }
        }
        self.advance().await
    }

    /// where the transfer was kept, for resumable ones
    pub(crate) fn kept(&self) -> bool {
        self.kept.is_some()
    }

    /// A new folder for the transfer below `dir`.
    async fn create(dir: &Path, id: u32, checked: Vec<(Entry, PathBuf)>) -> io::Result<Self> {
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let root = dir.join(format!("{created:015}-{id}"));
        if !no_links(dir) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{dir:?} or the folder it is in is a link, not a folder"),
            ));
        }
        tokio::fs::create_dir_all(&root).await?;
        // only this user may look into the received files (the folder it is
        // in may let others through)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = fs::set_permissions(dir, fs::Permissions::from_mode(0o700)) {
                log::warn!("{dir:?}: could not make it private: {e}");
            }
        }
        let files = checked.iter().filter(|(e, _)| !e.dir).count();
        let bytes = checked.iter().map(|(e, _)| e.size).sum();
        Ok(Self {
            root,
            entries: checked,
            next: 0,
            current: None,
            files,
            bytes,
            files_done: 0,
            bytes_done: 0,
            waiting: None,
            kept: None,
        })
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
            // Windows: from another computer, like a download, so Windows
            // (SmartScreen, Office) warns before it runs or opens it; the
            // mark goes along when the file is pasted on the same drive
            #[cfg(windows)]
            {
                let mut mark = path.clone().into_os_string();
                mark.push(":Zone.Identifier");
                if let Err(e) = tokio::fs::write(&mark, "[ZoneTransfer]\r\nZoneId=3\r\n").await {
                    log::debug!("{path:?}: no mark of another computer: {e}");
                }
            }
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
        if self.waiting.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "data before the sender said where it begins",
            ));
        }
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
        self.waiting.is_none() && self.current.is_none() && self.next == self.entries.len()
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
        if let Some(kept) = &self.kept {
            if let Err(e) = fs::remove_file(kept) {
                log::warn!("could not remove {kept:?}: {e}");
            }
        }
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
        if let Some(kept) = &self.kept {
            let _ = tokio::fs::remove_file(kept).await;
        }
    }

    /// The folder of the transfer and its `.resume`, to remove a partial
    /// transfer later (see [`remove_partial`]).
    pub(crate) fn partial(&self) -> PathBuf {
        self.root.clone()
    }
}

/// Removes a partial transfer that was kept to go on with: its folder and
/// its `.resume`.
pub(crate) fn remove_partial(root: &Path) {
    if fs::symlink_metadata(root).is_ok_and(|m| m.is_dir()) {
        if let Err(e) = fs::remove_dir_all(root) {
            log::warn!("could not remove {root:?}: {e}");
        }
    }
    let kept = resume_file(root);
    if let Err(e) = fs::remove_file(&kept) {
        if e.kind() != io::ErrorKind::NotFound {
            log::warn!("could not remove {kept:?}: {e}");
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
        // no text that runs the other way
        assert_eq!(
            safe_path("foto\u{202e}gpj.exe"),
            Some(PathBuf::from("fotogpj.exe"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn links_are_not_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir(elsewhere.path().join("000000000000001-1")).unwrap();
        // a link among the transfers
        std::os::unix::fs::symlink(elsewhere.path(), dir.path().join("000000000000002-2")).unwrap();
        cleanup(dir.path(), 0);
        assert!(elsewhere.path().join("000000000000001-1").exists());
        assert!(
            !dir.path().join("000000000000002-2").exists()
                || dir.path().join("000000000000002-2").is_symlink()
        );
        // the folder of the transfers is a link
        let linked = dir.path().join("received");
        std::os::unix::fs::symlink(elsewhere.path(), &linked).unwrap();
        cleanup(&linked, 0);
        assert!(elsewhere.path().join("000000000000001-1").exists());
    }

    #[cfg(windows)]
    #[test]
    fn names_valid_on_windows() {
        assert_eq!(safe_path("a:b?.txt"), Some(PathBuf::from("a_b_.txt")));
        assert_eq!(safe_path("con.txt"), Some(PathBuf::from("_con.txt")));
        assert_eq!(safe_path("con .txt"), Some(PathBuf::from("_con .txt")));
        assert_eq!(safe_path("COM¹"), Some(PathBuf::from("_COM¹")));
        assert_eq!(safe_path("conout$"), Some(PathBuf::from("_conout$")));
        assert_eq!(safe_path("COM10"), Some(PathBuf::from("COM10")));
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

    fn resume_entries() -> Vec<Entry> {
        vec![
            entry("Pasta", true, 0),
            entry("Pasta/a.bin", false, 10),
            entry("Pasta/vazia", true, 0),
            entry("Pasta/b.bin", false, 6),
            entry("c.bin", false, 4),
        ]
    }

    fn sha(bytes: &[u8]) -> [u8; TAIL_HASH] {
        Sha256::digest(bytes).into()
    }

    #[tokio::test]
    async fn a_lost_transfer_goes_on_where_it_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let (mut receiving, at) = Receiving::resumable(dir.path(), 1, "fp", "k1", resume_entries())
            .await
            .unwrap();
        assert_eq!(at, Position::default());
        assert!(!receiving.is_complete(), "nothing before the sender begins");
        assert!(receiving.write(b"x").await.is_err());
        receiving.begin(0, 0).await.unwrap();
        receiving.write(b"0123456789").await.unwrap();
        receiving.write(b"abc").await.unwrap();
        // the connection is lost: dropped, not discarded
        drop(receiving);

        // another sender, or other files, start anew
        let (other, at) = Receiving::resumable(dir.path(), 2, "outro", "k1", resume_entries())
            .await
            .unwrap();
        assert_eq!(at, Position::default());
        other.discard().await;

        let (mut receiving, at) = Receiving::resumable(dir.path(), 3, "fp", "k1", resume_entries())
            .await
            .unwrap();
        assert_eq!((at.file, at.offset), (1, 3));
        assert_eq!(at.tail, sha(b"abc"));
        assert_eq!((receiving.files_done, receiving.bytes_done), (1, 13));
        // only where it stopped, or that file again
        assert!(receiving.begin(1, 2).await.is_err());
        let (mut receiving, at) = Receiving::resumable(dir.path(), 4, "fp", "k1", resume_entries())
            .await
            .unwrap();
        assert_eq!((at.file, at.offset), (1, 3));
        receiving.begin(1, 3).await.unwrap();
        receiving.write(b"def").await.unwrap();
        receiving.write(b"wxyz").await.unwrap();
        assert!(receiving.is_complete());
        let top = receiving.finish();
        assert_eq!(fs::read(top[0].join("a.bin")).unwrap(), b"0123456789");
        assert_eq!(fs::read(top[0].join("b.bin")).unwrap(), b"abcdef");
        assert!(top[0].join("vazia").is_dir());
        assert_eq!(fs::read(&top[1]).unwrap(), b"wxyz");
        // nothing is kept to go on with any more
        let kept: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == RESUME))
            .collect();
        assert!(kept.is_empty());
    }

    #[tokio::test]
    async fn what_the_sender_does_not_have_is_had_again() {
        let dir = tempfile::tempdir().unwrap();
        let (mut receiving, _) = Receiving::resumable(dir.path(), 1, "fp", "k", resume_entries())
            .await
            .unwrap();
        receiving.begin(0, 0).await.unwrap();
        receiving.write(b"0123").await.unwrap();
        drop(receiving);
        let (mut receiving, at) = Receiving::resumable(dir.path(), 2, "fp", "k", resume_entries())
            .await
            .unwrap();
        assert_eq!((at.file, at.offset), (0, 4));
        // the sender's file is different: from its start
        receiving.begin(0, 0).await.unwrap();
        assert_eq!(receiving.bytes_done, 0);
        receiving.write(b"ABCDEFGHIJ").await.unwrap();
        receiving.write(b"abcdef").await.unwrap();
        receiving.write(b"wxyz").await.unwrap();
        let top = receiving.finish();
        assert_eq!(fs::read(top[0].join("a.bin")).unwrap(), b"ABCDEFGHIJ");
    }

    #[tokio::test]
    async fn a_complete_kept_transfer_is_complete_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![entry("um.txt", false, 2), entry("dois.txt", false, 0)];
        let (mut receiving, _) = Receiving::resumable(dir.path(), 1, "fp", "k", entries.clone())
            .await
            .unwrap();
        receiving.begin(0, 0).await.unwrap();
        receiving.write(b"ok").await.unwrap();
        // the last byte arrived, the answer did not
        drop(receiving);
        let (mut receiving, at) = Receiving::resumable(dir.path(), 2, "fp", "k", entries)
            .await
            .unwrap();
        assert_eq!((at.file, at.offset), (2, 0));
        receiving.begin(2, 0).await.unwrap();
        assert!(receiving.is_complete());
    }

    #[tokio::test]
    async fn a_longer_or_linked_file_is_had_again() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![entry("a.bin", false, 4), entry("b.bin", false, 4)];
        let (mut receiving, _) = Receiving::resumable(dir.path(), 1, "fp", "k", entries.clone())
            .await
            .unwrap();
        receiving.begin(0, 0).await.unwrap();
        receiving.write(b"1234").await.unwrap();
        receiving.write(b"56").await.unwrap();
        let root = receiving.partial();
        drop(receiving);
        fs::write(root.join("b.bin"), b"123456789").unwrap();
        let (receiving, at) = Receiving::resumable(dir.path(), 2, "fp", "k", entries.clone())
            .await
            .unwrap();
        assert_eq!((at.file, at.offset), (1, 0));
        assert!(!root.join("b.bin").exists());
        drop(receiving);
        #[cfg(unix)]
        {
            let elsewhere = tempfile::tempdir().unwrap();
            let target = elsewhere.path().join("alvo");
            fs::write(&target, b"12").unwrap();
            std::os::unix::fs::symlink(&target, root.join("b.bin")).unwrap();
            let (mut receiving, at) = Receiving::resumable(dir.path(), 3, "fp", "k", entries)
                .await
                .unwrap();
            assert_eq!((at.file, at.offset), (1, 0), "a link is not gone on with");
            receiving.begin(1, 0).await.unwrap();
            receiving.write(b"abcd").await.unwrap();
            assert_eq!(
                fs::read(&target).unwrap(),
                b"12",
                "nothing written through it"
            );
        }
    }

    #[tokio::test]
    async fn lost_between_two_files_goes_on_with_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            entry("Pasta", true, 0),
            entry("Pasta/a.txt", false, 4),
            entry("Pasta/b.txt", false, 4),
        ];
        let (mut receiving, _) = Receiving::resumable(dir.path(), 1, "fp", "k", entries.clone())
            .await
            .unwrap();
        receiving.begin(0, 0).await.unwrap();
        // a.txt is complete, b.txt is created and empty
        receiving.write(b"aaaa").await.unwrap();
        let root = receiving.partial();
        drop(receiving);
        assert_eq!(fs::metadata(root.join("Pasta/b.txt")).unwrap().len(), 0);
        let (mut receiving, at) = Receiving::resumable(dir.path(), 2, "fp", "k", entries.clone())
            .await
            .unwrap();
        assert_eq!((at.file, at.offset), (1, 0));
        receiving.begin(1, 0).await.unwrap();
        receiving.write(b"bbbb").await.unwrap();
        assert!(receiving.is_complete());
        let top = receiving.finish();
        assert_eq!(fs::read(top[0].join("b.txt")).unwrap(), b"bbbb");
        // lost right after the sender said where it begins
        let (mut receiving, _) = Receiving::resumable(dir.path(), 3, "fp", "k2", entries.clone())
            .await
            .unwrap();
        receiving.begin(0, 0).await.unwrap();
        drop(receiving);
        let (mut receiving, at) = Receiving::resumable(dir.path(), 4, "fp", "k2", entries)
            .await
            .unwrap();
        assert_eq!((at.file, at.offset), (0, 0));
        receiving.begin(0, 0).await.unwrap();
        receiving.write(b"aaaa").await.unwrap();
        receiving.write(b"bbbb").await.unwrap();
        assert!(receiving.is_complete());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_kept_folder_with_a_link_inside_starts_anew() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        fs::write(elsewhere.path().join("b.txt"), b"bb").unwrap();
        let entries = vec![
            entry("Pasta", true, 0),
            entry("Pasta/a.txt", false, 4),
            entry("Pasta/b.txt", false, 4),
        ];
        let (mut receiving, _) = Receiving::resumable(dir.path(), 1, "fp", "k", entries.clone())
            .await
            .unwrap();
        receiving.begin(0, 0).await.unwrap();
        receiving.write(b"aaaa").await.unwrap();
        let root = receiving.partial();
        drop(receiving);
        // the folder becomes a link to somewhere else
        fs::remove_dir_all(root.join("Pasta")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), root.join("Pasta")).unwrap();
        let (mut receiving, at) = Receiving::resumable(dir.path(), 2, "fp", "k", entries)
            .await
            .unwrap();
        assert_eq!(at, Position::default(), "from the start, in a new folder");
        assert!(!root.exists() && !resume_file(&root).exists());
        receiving.begin(0, 0).await.unwrap();
        receiving.write(b"aaaa").await.unwrap();
        receiving.write(b"bbbb").await.unwrap();
        assert!(receiving.is_complete());
        assert_eq!(fs::read(elsewhere.path().join("b.txt")).unwrap(), b"bb");
        assert!(
            elsewhere.path().join("b.txt").exists(),
            "nothing removed there"
        );
    }

    #[cfg(unix)]
    #[test]
    fn plain_files_are_opened_without_following_links() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        fs::write(&file, b"abc").unwrap();
        let link = dir.path().join("l");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(open_plain(&file, false).is_ok());
        assert!(open_plain(&link, false).is_err());
        assert!(open_plain(dir.path(), false).is_err());
        let mut opened = open_plain(&file, false).unwrap();
        assert_eq!(tail_hash(&mut opened, 2).unwrap(), sha(b"ab"));
        assert_eq!(tail_hash(&mut opened, 0).unwrap(), [0; TAIL_HASH]);
        assert!(tail_hash(&mut opened, 4).is_err());
    }

    #[tokio::test]
    async fn kept_transfers_outlive_the_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let (mut receiving, _) = Receiving::resumable(dir.path(), 1, "fp", "k", resume_entries())
            .await
            .unwrap();
        receiving.begin(0, 0).await.unwrap();
        receiving.write(b"01").await.unwrap();
        let root = receiving.partial();
        drop(receiving);
        fs::create_dir(dir.path().join("000000000000001-9")).unwrap();
        fs::write(dir.path().join("000000000000002-9.resume"), b"{}").unwrap();
        cleanup(dir.path(), 0);
        assert!(root.is_dir(), "kept to go on with");
        assert!(resume_file(&root).is_file());
        assert!(!dir.path().join("000000000000001-9").exists());
        assert!(!dir.path().join("000000000000002-9.resume").exists());
        remove_partial(&root);
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
