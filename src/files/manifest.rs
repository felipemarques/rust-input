//! What gets sent when files are copied: every copied file and folder, with
//! the contents of the folders.

use std::{
    collections::HashSet,
    fs::{self, Metadata},
    io,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use sha2::{Digest, Sha256};

use super::wire::Entry;

/// the entries of a transfer and where their contents come from
#[derive(Debug, Default)]
pub(crate) struct Manifest {
    pub(crate) entries: Vec<Entry>,
    /// local path of every file entry, in the order of `entries`
    pub(crate) sources: Vec<PathBuf>,
}

impl Manifest {
    pub(crate) fn files(&self) -> usize {
        self.sources.len()
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }
}

fn modified(metadata: &Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// a name not used yet among `taken`: "name", then "name (2)", "name (3)"...
fn unique_name(name: &str, taken: &mut HashSet<String>) -> String {
    let mut candidate = name.to_owned();
    let mut n = 2;
    while !taken.insert(candidate.to_lowercase()) {
        let (stem, ext) = match name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
            _ => (name, String::new()),
        };
        candidate = format!("{stem} ({n}){ext}");
        n += 1;
    }
    candidate
}

/// Builds the manifest of the copied `paths`. Folders are walked
/// recursively; symbolic links to files are followed, links to folders are
/// skipped (they could loop). Entries that can not be read are skipped with
/// a warning.
pub(crate) fn scan(paths: &[PathBuf]) -> io::Result<Manifest> {
    let mut manifest = Manifest::default();
    let mut taken = HashSet::new();
    for path in paths {
        let Some(name) = path.file_name() else {
            log::warn!("skipping {path:?}: no file name");
            continue;
        };
        let name = unique_name(&name.to_string_lossy(), &mut taken);
        add(&mut manifest, path, name)?;
    }
    if manifest.entries.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "none of the copied files could be read",
        ));
    }
    Ok(manifest)
}

fn add(manifest: &mut Manifest, path: &Path, name: String) -> io::Result<()> {
    let link = fs::symlink_metadata(path)?;
    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            log::warn!("skipping {path:?}: {e}");
            return Ok(());
        }
    };
    if metadata.is_dir() {
        if link.file_type().is_symlink() {
            log::warn!("skipping {path:?}: link to a folder");
            return Ok(());
        }
        manifest.entries.push(Entry {
            path: name.clone(),
            dir: true,
            size: 0,
            modified: modified(&metadata),
        });
        let mut children = match fs::read_dir(path) {
            Ok(dir) => dir.filter_map(Result::ok).collect::<Vec<_>>(),
            Err(e) => {
                log::warn!("can not read the folder {path:?}: {e}");
                return Ok(());
            }
        };
        children.sort_by_key(|c| c.file_name());
        for child in children {
            let child_name = format!("{name}/{}", child.file_name().to_string_lossy());
            if let Err(e) = add(manifest, &child.path(), child_name) {
                log::warn!("skipping {:?}: {e}", child.path());
            }
        }
    } else if metadata.is_file() {
        manifest.entries.push(Entry {
            path: name,
            dir: false,
            size: metadata.len(),
            modified: modified(&metadata),
        });
        manifest.sources.push(path.to_owned());
    } else {
        log::warn!("skipping {path:?}: not a regular file");
    }
    Ok(())
}

/// Identifies a list of copied files by their paths, sizes and modification
/// times, without reading them: copying the same files again gives the same
/// key, so they are not sent twice.
pub(crate) fn files_key(paths: &[PathBuf]) -> u64 {
    let mut entries: Vec<String> = paths
        .iter()
        .map(|p| {
            let (size, modified) = fs::metadata(p)
                .map(|m| (m.len(), modified(&m).unwrap_or(0)))
                .unwrap_or((0, 0));
            format!("{}\0{size}\0{modified}", p.to_string_lossy())
        })
        .collect();
    entries.sort();
    let mut hasher = Sha256::new();
    hasher.update(b"files");
    for entry in entries {
        hasher.update(entry.as_bytes());
        hasher.update([0xff]);
    }
    u64::from_be_bytes(hasher.finalize()[..8].try_into().expect("8 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::write(root.join("relatório.pdf"), b"pdf").unwrap();
        fs::create_dir_all(root.join("Fotos/2026/vazia")).unwrap();
        fs::write(root.join("Fotos/b.jpg"), vec![1; 1000]).unwrap();
        fs::write(root.join("Fotos/a.jpg"), vec![2; 10]).unwrap();
        fs::write(root.join("Fotos/2026/c.jpg"), b"").unwrap();
        dir
    }

    #[test]
    fn folders_are_walked_in_order() {
        let dir = tree();
        let root = dir.path();
        let manifest = scan(&[root.join("relatório.pdf"), root.join("Fotos")]).expect("scan");
        let paths: Vec<_> = manifest
            .entries
            .iter()
            .map(|e| (e.path.as_str(), e.dir, e.size))
            .collect();
        assert_eq!(
            paths,
            [
                ("relatório.pdf", false, 3),
                ("Fotos", true, 0),
                ("Fotos/2026", true, 0),
                ("Fotos/2026/c.jpg", false, 0),
                ("Fotos/2026/vazia", true, 0),
                ("Fotos/a.jpg", false, 10),
                ("Fotos/b.jpg", false, 1000),
            ]
        );
        assert_eq!(manifest.files(), 4);
        assert_eq!(manifest.bytes(), 1013);
        assert_eq!(manifest.sources[1], root.join("Fotos/2026/c.jpg"));
    }

    #[test]
    fn same_names_from_different_folders_are_kept_apart() {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["a", "b"] {
            fs::create_dir(dir.path().join(sub)).unwrap();
            fs::write(dir.path().join(sub).join("nota.txt"), sub).unwrap();
        }
        let manifest =
            scan(&[dir.path().join("a/nota.txt"), dir.path().join("b/nota.txt")]).unwrap();
        let names: Vec<_> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(names, ["nota.txt", "nota (2).txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn links_to_folders_are_skipped() {
        let dir = tree();
        let root = dir.path();
        std::os::unix::fs::symlink(root.join("Fotos"), root.join("atalho")).unwrap();
        std::os::unix::fs::symlink(root.join("relatório.pdf"), root.join("link.pdf")).unwrap();
        let manifest = scan(&[root.join("atalho"), root.join("link.pdf")]).unwrap();
        let names: Vec<_> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(names, ["link.pdf"]);
        assert!(scan(&[root.join("atalho")]).is_err());
    }

    #[test]
    fn key_changes_with_the_files() {
        let dir = tree();
        let root = dir.path();
        let paths = vec![root.join("relatório.pdf"), root.join("Fotos")];
        let key = files_key(&paths);
        assert_eq!(
            key,
            files_key(&[root.join("Fotos"), root.join("relatório.pdf")])
        );
        fs::write(root.join("relatório.pdf"), b"other content").unwrap();
        assert_ne!(key, files_key(&paths));
        assert_ne!(key, files_key(&paths[..1]));
    }
}
