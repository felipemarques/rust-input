//! The history of the clipboard (0.35.0): what was copied on this computer
//! while RustInput ran, the latest first, in memory only.
//!
//! A copy is recorded when [`super::watch`] tells of it, once the
//! application that copied is done (a short delay). The same content copied
//! again moves to the top instead of being there twice. Secrets marked by a
//! password manager are not recorded at all. At most `size` copies are kept
//! (`clipboard_history`, 30 by default), and at most [`MAX_BYTES`] of them:
//! the oldest go first.

use std::{collections::VecDeque, path::PathBuf};

use rustinput_ipc::{ClipboardHistoryItem, ClipboardPeek};
use serde::{Deserialize, Serialize};

use super::{Content, LocalContent, local::Recorded};

/// most bytes the copies kept take together
const MAX_BYTES: usize = 64 * 1024 * 1024;
/// a larger copy is not kept
const MAX_ENTRY: usize = 32 * 1024 * 1024;

/// what is kept of a copy, to bring it back
#[derive(Clone, Debug)]
pub(crate) enum Kept {
    Data(Content),
    /// the paths of copied files and folders, which stay where they are
    Files(Vec<PathBuf>),
}

#[derive(Debug)]
struct Entry {
    hash: u64,
    when: u64,
    peek: ClipboardPeek,
    kept: Kept,
    bytes: usize,
}

/// the answer about the history, for the Clipboards tab of a computer
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum HistoryAnswer {
    /// still `version` the asker knows
    Unchanged,
    Items {
        version: u64,
        items: Vec<ClipboardHistoryItem>,
    },
    /// none kept (`clipboard_history = 0`)
    Off,
    /// copies can not be told here (Wayland)
    Unsupported,
    /// the other computer answered without a history (before 0.35.0); never
    /// sent
    Older,
}

pub(crate) struct History {
    entries: VecDeque<Entry>,
    /// raised at every change
    version: u64,
    size: usize,
    bytes: usize,
}

impl History {
    pub(crate) fn new(size: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            version: 1,
            size,
            bytes: 0,
        }
    }

    pub(crate) fn size(&self) -> usize {
        self.size
    }

    /// keeps at most `size` copies from now on (0: none)
    pub(crate) fn set_size(&mut self, size: usize) {
        self.size = size;
        self.trim();
    }

    /// keeps what was copied at `when` (seconds since 1970)
    pub(crate) fn add(&mut self, recorded: Recorded, when: u64) {
        if self.size == 0 {
            return;
        }
        let (hash, kept, size) = match recorded.content {
            LocalContent::Data(content) => {
                let size = content.data.len();
                (content.hash, Kept::Data(content), size)
            }
            LocalContent::Files { paths, key } => {
                let size = paths.iter().map(|p| p.as_os_str().len()).sum();
                (key, Kept::Files(paths), size)
            }
        };
        let bytes = size + peek_bytes(&recorded.peek);
        if bytes > MAX_ENTRY {
            log::info!("a copy of {size} bytes is too large for the history");
            return;
        }
        if let Some(i) = self.entries.iter().position(|e| e.hash == hash) {
            let again = self.entries.remove(i).expect("found");
            self.bytes -= again.bytes;
        }
        self.bytes += bytes;
        self.entries.push_front(Entry {
            hash,
            when,
            peek: recorded.peek,
            kept,
            bytes,
        });
        self.trim();
        self.version += 1;
    }

    fn trim(&mut self) {
        let mut trimmed = false;
        while self.entries.len() > self.size || self.bytes > MAX_BYTES {
            let Some(oldest) = self.entries.pop_back() else {
                break;
            };
            self.bytes -= oldest.bytes;
            trimmed = true;
        }
        if trimmed {
            self.version += 1;
        }
    }

    pub(crate) fn clear(&mut self) {
        if !self.entries.is_empty() {
            self.entries.clear();
            self.bytes = 0;
            self.version += 1;
        }
    }

    /// what was copied with `hash`, if it is still kept
    pub(crate) fn find(&self, hash: u64) -> Option<&Kept> {
        self.entries
            .iter()
            .find(|e| e.hash == hash)
            .map(|e| &e.kept)
    }

    /// everything, unless the asker still knows this `version`
    pub(crate) fn answer(&self, known: Option<u64>) -> HistoryAnswer {
        if self.size == 0 {
            return HistoryAnswer::Off;
        }
        if known == Some(self.version) {
            return HistoryAnswer::Unchanged;
        }
        HistoryAnswer::Items {
            version: self.version,
            items: self
                .entries
                .iter()
                .map(|e| ClipboardHistoryItem {
                    hash: e.hash,
                    when: e.when,
                    peek: e.peek.clone(),
                })
                .collect(),
        }
    }
}

fn peek_bytes(peek: &ClipboardPeek) -> usize {
    match peek {
        ClipboardPeek::Text { start, .. } => start.len(),
        ClipboardPeek::Image { thumbnail, .. } => thumbnail.len(),
        ClipboardPeek::Files { names, .. } => names.iter().map(String::len).sum(),
        ClipboardPeek::Empty | ClipboardPeek::Secret => 0,
    }
}

#[cfg(test)]
mod tests {
    use rustinput_proto::ClipboardKind;

    use super::*;

    fn text(text: &str, hash: u64) -> Recorded {
        Recorded {
            content: LocalContent::Data(Content {
                kind: ClipboardKind::Text,
                data: text.as_bytes().to_vec(),
                hash,
            }),
            peek: ClipboardPeek::Text {
                start: text.into(),
                chars: text.chars().count() as u64,
                lines: 1,
                formatted: false,
            },
        }
    }

    fn hashes(history: &History) -> Vec<u64> {
        match history.answer(None) {
            HistoryAnswer::Items { items, .. } => items.iter().map(|i| i.hash).collect(),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn the_latest_first_and_a_copy_again_moves_up() {
        let mut history = History::new(30);
        history.add(text("um", 1), 100);
        history.add(text("dois", 2), 101);
        history.add(text("três", 3), 102);
        assert_eq!(hashes(&history), [3, 2, 1]);
        history.add(text("um", 1), 103);
        assert_eq!(hashes(&history), [1, 3, 2]);
        match history.answer(None) {
            HistoryAnswer::Items { items, .. } => assert_eq!(items[0].when, 103),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(history.find(2), Some(Kept::Data(c)) if c.data == b"dois"));
        assert!(history.find(9).is_none());
    }

    #[test]
    fn the_version_tells_what_changed() {
        let mut history = History::new(30);
        let HistoryAnswer::Items { version, items } = history.answer(None) else {
            panic!("items");
        };
        assert!(items.is_empty());
        assert_eq!(history.answer(Some(version)), HistoryAnswer::Unchanged);
        history.add(text("um", 1), 100);
        assert!(matches!(
            history.answer(Some(version)),
            HistoryAnswer::Items { .. }
        ));
        let HistoryAnswer::Items { version, .. } = history.answer(None) else {
            panic!("items");
        };
        history.clear();
        assert_eq!(
            history.answer(Some(version)),
            HistoryAnswer::Items {
                version: version + 1,
                items: Vec::new()
            }
        );
    }

    #[test]
    fn only_so_many_copies_are_kept() {
        let mut history = History::new(3);
        for i in 0..5 {
            history.add(text(&format!("copia {i}"), i), 100 + i);
        }
        assert_eq!(hashes(&history), [4, 3, 2]);
        history.set_size(1);
        assert_eq!(hashes(&history), [4]);
        history.set_size(0);
        assert_eq!(history.answer(None), HistoryAnswer::Off);
        history.add(text("nada", 7), 200);
        history.set_size(5);
        assert!(hashes(&history).is_empty(), "nothing was kept while off");
    }

    #[test]
    fn large_copies_push_the_oldest_out() {
        let mut history = History::new(30);
        let big = |hash: u64| Recorded {
            content: LocalContent::Data(Content {
                kind: ClipboardKind::Image,
                data: vec![0; 20 * 1024 * 1024],
                hash,
            }),
            peek: ClipboardPeek::Image {
                width: 1,
                height: 1,
                thumbnail: Vec::new(),
            },
        };
        for hash in 1..=4 {
            history.add(big(hash), hash);
        }
        // 64 MiB hold three of 20 MiB
        assert_eq!(hashes(&history), [4, 3, 2]);
        let huge = Recorded {
            content: LocalContent::Data(Content {
                kind: ClipboardKind::Image,
                data: vec![0; MAX_ENTRY + 1],
                hash: 9,
            }),
            peek: ClipboardPeek::Empty,
        };
        history.add(huge, 10);
        assert_eq!(hashes(&history), [4, 3, 2], "too large for the history");
    }
}
