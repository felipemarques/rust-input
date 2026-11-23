//! Access to the clipboard of this machine.
//!
//! Reading the clipboard can take a while (the application owning it has to
//! answer, images have to be converted), so everything happens on a dedicated
//! thread and the async side only exchanges messages with it.

use std::{borrow::Cow, io::Cursor, path::PathBuf, sync::mpsc};

use rustinput_ipc::{
    CLIPBOARD_PEEK_FILES, CLIPBOARD_PEEK_TEXT, CLIPBOARD_THUMBNAIL, ClipboardPeek,
};
use rustinput_proto::{
    ClipboardKind,
    clipboard::{decode_html, encode_html},
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::oneshot;

use super::{Content, secret::Marks};
use crate::files::files_key;

/// decoded images larger than this are refused (8192 x 8192 RGBA)
const MAX_IMAGE_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Error)]
enum ClipboardError {
    #[error(transparent)]
    Clipboard(#[from] arboard::Error),
    #[error("could not encode the image: {0}")]
    Encode(#[from] png::EncodingError),
    #[error("could not decode the image: {0}")]
    Decode(#[from] png::DecodingError),
    #[error("unsupported image format, expected 8 bit RGBA")]
    ImageFormat,
    #[error("the image is too large")]
    ImageTooLarge,
    #[error("the text is not valid UTF-8")]
    InvalidText,
    #[error("the content does not match its hash")]
    HashMismatch,
    #[error("malformed formatted text")]
    InvalidHtml,
}

/// what the clipboard of this machine holds
#[derive(Debug)]
pub(crate) enum LocalContent {
    /// text or an image
    Data(Content),
    /// copied files and folders, identified by `key` (see [`files_key`])
    Files { paths: Vec<PathBuf>, key: u64 },
}

/// what the clipboard holds as the Clipboard tab of the computers shows it
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Peeked {
    /// still what the hash asked about stands for
    Unchanged,
    /// `hash` is that of the [`Content`] (of the files: [`files_key`]), 0
    /// for an empty clipboard
    Holds { hash: u64, peek: ClipboardPeek },
}

enum Request {
    Read(oneshot::Sender<Option<LocalContent>>),
    Peek(Option<u64>, oneshot::Sender<Peeked>),
    Write(Content),
    WriteFiles(Vec<PathBuf>),
}

/// handle to the clipboard thread
#[derive(Clone)]
pub(crate) struct LocalClipboard {
    requests: mpsc::Sender<Request>,
}

impl LocalClipboard {
    pub(crate) fn new() -> Self {
        let (requests, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || worker(rx));
        if let Err(e) = spawned {
            log::warn!("could not start the clipboard thread: {e}");
        }
        Self { requests }
    }

    /// the current content of the clipboard: files, text or an image
    pub(crate) async fn read(&self) -> Option<LocalContent> {
        let (reply, content) = oneshot::channel();
        self.requests.send(Request::Read(reply)).ok()?;
        content.await.ok().flatten()
    }

    /// What the clipboard holds, shortened for the Clipboard tab:
    /// [`Peeked::Unchanged`] when it still holds what `known` (a hash)
    /// stands for. `None` when the clipboard can not be read.
    pub(crate) async fn peek(&self, known: Option<u64>) -> Option<Peeked> {
        let (reply, peeked) = oneshot::channel();
        self.requests.send(Request::Peek(known, reply)).ok()?;
        peeked.await.ok()
    }

    /// replaces the content of the clipboard
    pub(crate) fn write(&self, content: Content) {
        let _ = self.requests.send(Request::Write(content));
    }

    /// puts files on the clipboard, ready to be pasted in a file manager
    pub(crate) fn write_files(&self, paths: Vec<PathBuf>) {
        let _ = self.requests.send(Request::WriteFiles(paths));
    }
}

fn worker(requests: mpsc::Receiver<Request>) {
    let mut clipboard: Option<arboard::Clipboard> = None;
    let mut marks = Marks::default();
    let mut warned = false;
    for request in requests {
        if clipboard.is_none() {
            match arboard::Clipboard::new() {
                Ok(c) => clipboard = Some(c),
                Err(e) if !warned => {
                    warned = true;
                    log::warn!("the clipboard is not available: {e}");
                }
                Err(_) => {}
            }
        }
        match request {
            Request::Read(reply) => {
                let _ = reply.send(clipboard.as_mut().and_then(read));
            }
            Request::Peek(known, reply) => {
                if let Some(clipboard) = clipboard.as_mut() {
                    let _ = reply.send(peek(clipboard, &mut marks, known));
                }
            }
            Request::WriteFiles(paths) => write_files(clipboard.as_mut(), paths),
            Request::Write(content) => {
                let Some(clipboard) = clipboard.as_mut() else {
                    continue;
                };
                let (kind, size) = (content.kind, content.data.len());
                match write(clipboard, content) {
                    Ok(()) => log::debug!("clipboard set to {kind:?} ({size} bytes)"),
                    Err(e) => log::warn!("could not set the clipboard: {e}"),
                }
            }
        }
    }
}

fn write_files(clipboard: Option<&mut arboard::Clipboard>, paths: Vec<PathBuf>) {
    let count = paths.len();
    // Nemo, Nautilus and friends only paste their own format on X11
    #[cfg(all(unix, not(target_os = "macos")))]
    if super::x11_files::is_x11() {
        match super::x11_files::serve(paths) {
            Ok(()) => log::debug!("clipboard set to {count} received item(s)"),
            Err(e) => log::warn!("could not put the received files on the clipboard: {e}"),
        }
        return;
    }
    // cut rather than copied: Explorer moves them out of the temporary folder
    #[cfg(windows)]
    {
        let _ = clipboard;
        match super::windows_files::write_cut_files(&paths) {
            Ok(()) => log::debug!("clipboard set to {count} received item(s)"),
            Err(e) => log::warn!("could not put the received files on the clipboard: {e}"),
        }
    }
    #[cfg(not(windows))]
    let Some(clipboard) = clipboard else {
        return;
    };
    #[cfg(not(windows))]
    match clipboard.set().file_list(&paths) {
        Ok(()) => log::debug!("clipboard set to {count} received item(s)"),
        Err(e) => log::warn!("could not put the received files on the clipboard: {e}"),
    }
}

/// Files win over text: file managers offer the paths of copied files as text
/// too. Text wins over an image: applications offering both (spreadsheets,
/// word processors) mean the text, the image is only a rendering of it.
fn read(clipboard: &mut arboard::Clipboard) -> Option<LocalContent> {
    match clipboard.get().file_list() {
        Ok(paths) if !paths.is_empty() => {
            let key = files_key(&paths);
            return Some(LocalContent::Files { paths, key });
        }
        Ok(_) | Err(arboard::Error::ContentNotAvailable) => {}
        Err(e) => log::debug!("could not read files from the clipboard: {e}"),
    }
    // Formatted text (browsers, word processors, spreadsheets) wins over the
    // same text without formatting and over the image some of them add. An
    // HTML without any text is not meant though: browsers offer the image they
    // copy as an <img> as well.
    match clipboard.get().html() {
        Ok(html) if !html_to_text(&html).trim().is_empty() => {
            let text = clipboard
                .get_text()
                .ok()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| html_to_text(&html));
            return Some(LocalContent::Data(Content::html(&html, &text)));
        }
        Ok(_) | Err(arboard::Error::ContentNotAvailable) => {}
        Err(e) => log::debug!("could not read formatted text from the clipboard: {e}"),
    }
    match clipboard.get_text() {
        Ok(text) if !text.is_empty() => return Some(LocalContent::Data(Content::text(text))),
        Ok(_) | Err(arboard::Error::ContentNotAvailable) => {}
        Err(e) => log::debug!("could not read text from the clipboard: {e}"),
    }
    match clipboard.get_image() {
        Ok(image) => match Content::image(image.width, image.height, &image.bytes) {
            Ok(content) => Some(LocalContent::Data(content)),
            Err(e) => {
                log::warn!("could not read the image from the clipboard: {e}");
                None
            }
        },
        Err(arboard::Error::ContentNotAvailable) => None,
        Err(e) => {
            log::debug!("could not read an image from the clipboard: {e}");
            None
        }
    }
}

/// What [`read`] would take, shortened: the start of a text, a thumbnail of
/// an image, the names of files. Nothing of a secret (see [`Marks`]).
fn peek(clipboard: &mut arboard::Clipboard, marks: &mut Marks, known: Option<u64>) -> Peeked {
    let holds = |hash: u64, peek: ClipboardPeek| match known {
        Some(known) if known == hash => Peeked::Unchanged,
        _ => Peeked::Holds { hash, peek },
    };
    if let Ok(paths) = clipboard.get().file_list() {
        if !paths.is_empty() {
            let names = paths
                .iter()
                .take(CLIPBOARD_PEEK_FILES)
                .map(|p| {
                    p.file_name()
                        .unwrap_or(p.as_os_str())
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            let count = paths.len() as u64;
            return holds(files_key(&paths), ClipboardPeek::Files { names, count });
        }
    }
    let secret = marks.secret();
    let text = clipboard.get_text().ok().filter(|t| !t.is_empty());
    if let Ok(html) = clipboard.get().html() {
        if !html_to_text(&html).trim().is_empty() {
            let text = text.unwrap_or_else(|| html_to_text(&html));
            let hash = html_hash(&html, &text);
            return holds(hash, text_peek(&text, true, secret));
        }
    }
    if let Some(text) = text {
        return holds(text_hash(text.as_bytes()), text_peek(&text, false, secret));
    }
    match clipboard.get_image() {
        Ok(image) if image.bytes.len() == image.width * image.height * 4 => {
            let hash = image_hash(image.width, image.height, &image.bytes);
            if known == Some(hash) {
                return Peeked::Unchanged;
            }
            let peek = image_peek(image.width, image.height, &image.bytes).unwrap_or_else(|e| {
                log::debug!("no thumbnail of the image on the clipboard: {e}");
                ClipboardPeek::Image {
                    width: image.width as u32,
                    height: image.height as u32,
                    thumbnail: Vec::new(),
                }
            });
            holds(hash, peek)
        }
        _ => holds(0, ClipboardPeek::Empty),
    }
}

fn text_peek(text: &str, formatted: bool, secret: bool) -> ClipboardPeek {
    if secret {
        return ClipboardPeek::Secret;
    }
    ClipboardPeek::Text {
        start: text_start(text, CLIPBOARD_PEEK_TEXT).to_owned(),
        chars: text.chars().count() as u64,
        lines: text.lines().count().max(1) as u64,
        formatted,
    }
}

/// the longest start of `text` of at most `max` bytes, whole characters
fn text_start(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn image_peek(width: usize, height: usize, rgba: &[u8]) -> Result<ClipboardPeek, ClipboardError> {
    let (w, h, small) = thumbnail(width, height, rgba, CLIPBOARD_THUMBNAIL as usize);
    Ok(ClipboardPeek::Image {
        width: u32::try_from(width).map_err(|_| ClipboardError::ImageTooLarge)?,
        height: u32::try_from(height).map_err(|_| ClipboardError::ImageTooLarge)?,
        thumbnail: encode_png(w, h, &small)?,
    })
}

/// The image shrunk to fit in `max` pixels each way, keeping its shape, each
/// pixel the average of those it covers. Small images stay as they are.
fn thumbnail(width: usize, height: usize, rgba: &[u8], max: usize) -> (usize, usize, Vec<u8>) {
    if width <= max && height <= max {
        return (width, height, rgba.to_vec());
    }
    let (w, h) = if width >= height {
        (max, (height * max / width).max(1))
    } else {
        ((width * max / height).max(1), max)
    };
    let mut small = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let (top, bottom) = (
            y * height / h,
            ((y + 1) * height / h).max(y * height / h + 1),
        );
        for x in 0..w {
            let (left, right) = (x * width / w, ((x + 1) * width / w).max(x * width / w + 1));
            let mut sum = [0u64; 4];
            for row in top..bottom {
                let start = (row * width + left) * 4;
                for pixel in rgba[start..start + (right - left) * 4].as_chunks::<4>().0 {
                    for (s, &c) in sum.iter_mut().zip(pixel) {
                        *s += c as u64;
                    }
                }
            }
            let count = ((bottom - top) * (right - left)) as u64;
            small.extend(sum.iter().map(|s| (s / count) as u8));
        }
    }
    (w, h, small)
}

fn write(clipboard: &mut arboard::Clipboard, content: Content) -> Result<(), ClipboardError> {
    match content.kind {
        ClipboardKind::Text => {
            if text_hash(&content.data) != content.hash {
                return Err(ClipboardError::HashMismatch);
            }
            let text = String::from_utf8(content.data).map_err(|_| ClipboardError::InvalidText)?;
            clipboard.set_text(text)?;
        }
        ClipboardKind::Image => {
            let (width, height, rgba) = decode_png(&content.data)?;
            if image_hash(width, height, &rgba) != content.hash {
                return Err(ClipboardError::HashMismatch);
            }
            clipboard.set_image(arboard::ImageData {
                width,
                height,
                bytes: Cow::Owned(rgba),
            })?;
        }
        ClipboardKind::Html => {
            let (html, text) = decode_html(&content.data).ok_or(ClipboardError::InvalidHtml)?;
            if html_hash(html, text) != content.hash {
                return Err(ClipboardError::HashMismatch);
            }
            // both: applications without formatting paste the text
            clipboard.set_html(html, Some(text))?;
        }
    }
    Ok(())
}

/// The text of `html` without tags, for HTML that comes without it, and to
/// tell whether there is any text at all.
fn html_to_text(html: &str) -> String {
    // lowering ASCII letters keeps every byte where it is
    let lower = html.to_ascii_lowercase();
    let mut text = String::with_capacity(html.len());
    let mut at = 0;
    while let Some(start) = html[at..].find('<').map(|s| at + s) {
        text.push_str(&html[at..start]);
        // nothing of scripts, styles and comments is text
        let close = [
            ("<script", "</script>"),
            ("<style", "</style>"),
            ("<!--", "-->"),
        ]
        .iter()
        .find(|(open, _)| lower[start..].starts_with(open))
        .map_or(">", |(_, close)| *close);
        at = match lower[start..].find(close) {
            Some(end) => start + end + close.len(),
            None => html.len(),
        };
    }
    text.push_str(&html[at..]);
    let entities = [
        ("&nbsp;", " "),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&amp;", "&"),
    ];
    entities.iter().fold(text, |text, (entity, character)| {
        text.replace(entity, character)
    })
}

impl Content {
    fn text(text: String) -> Self {
        let data = text.into_bytes();
        Self {
            kind: ClipboardKind::Text,
            hash: text_hash(&data),
            data,
        }
    }

    fn html(html: &str, text: &str) -> Self {
        Self {
            kind: ClipboardKind::Html,
            hash: html_hash(html, text),
            data: encode_html(html, text),
        }
    }

    /// the same without formatting, for peers that do not take HTML
    pub(crate) fn plain(self) -> Self {
        match self.kind {
            ClipboardKind::Html => match decode_html(&self.data) {
                Some((_, text)) => Self::text(text.to_owned()),
                None => self,
            },
            _ => self,
        }
    }

    /// The hash covers the pixels, not the PNG: the same image read back from
    /// the clipboard must hash the same no matter how it gets compressed.
    fn image(width: usize, height: usize, rgba: &[u8]) -> Result<Self, ClipboardError> {
        if width == 0 || height == 0 || rgba.len() != width * height * 4 {
            return Err(ClipboardError::ImageFormat);
        }
        Ok(Self {
            kind: ClipboardKind::Image,
            hash: image_hash(width, height, rgba),
            data: encode_png(width, height, rgba)?,
        })
    }
}

fn digest(parts: &[&[u8]]) -> u64 {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    let digest = hasher.finalize();
    u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"))
}

fn text_hash(text: &[u8]) -> u64 {
    digest(&[b"text", text])
}

fn html_hash(html: &str, text: &str) -> u64 {
    digest(&[b"html", html.as_bytes(), &[0], text.as_bytes()])
}

fn image_hash(width: usize, height: usize, rgba: &[u8]) -> u64 {
    let (width, height) = ((width as u64).to_be_bytes(), (height as u64).to_be_bytes());
    digest(&[b"image", &width, &height, rgba])
}

fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    let (w, h) = (
        u32::try_from(width).map_err(|_| ClipboardError::ImageTooLarge)?,
        u32::try_from(height).map_err(|_| ClipboardError::ImageTooLarge)?,
    );
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    // speed matters more than the last few percent of size on a local network
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    writer.finish()?;
    Ok(png)
}

fn decode_png(png: &[u8]) -> Result<(usize, usize, Vec<u8>), ClipboardError> {
    let mut decoder = png::Decoder::new(Cursor::new(png));
    decoder.set_limits(png::Limits {
        bytes: MAX_IMAGE_BYTES,
    });
    let mut reader = decoder.read_info()?;
    let info = reader.info();
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(ClipboardError::ImageFormat);
    }
    let (width, height) = (info.width as usize, info.height as usize);
    let size = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|&size| size <= MAX_IMAGE_BYTES)
        .ok_or(ClipboardError::ImageTooLarge)?;
    let mut rgba = vec![0; size];
    reader.next_frame(&mut rgba)?;
    Ok((width, height, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_of_html() {
        assert_eq!(
            html_to_text("<p>Olá <b>mundo</b> &amp; você</p><!-- x --><style>p{}</style>"),
            "Olá mundo & você"
        );
        // only an image: no text, the image is what was copied
        assert_eq!(html_to_text("<img src=\"a.png\">").trim(), "");
        // accents right after a tag, unclosed tags
        assert_eq!(html_to_text("<é>á<SCRIPT>x</script>b<i"), "áb");
        // large documents take linear time
        let big = "<td>é</td>".repeat(200_000);
        assert_eq!(html_to_text(&big).chars().count(), 200_000);
    }

    #[test]
    fn formatted_text_and_its_plain_version() {
        let html = Content::html("<b>olá</b>", "olá");
        assert_eq!(html.kind, ClipboardKind::Html);
        assert_eq!(html.hash, html_hash("<b>olá</b>", "olá"));
        assert_eq!(html.plain(), Content::text("olá".into()));
        assert_ne!(
            html_hash("<b>olá</b>", "ola"),
            html_hash("<b>olá</b>", "olá")
        );
        assert_ne!(html_hash("a", "b"), text_hash(b"b"));
    }

    fn gradient(width: usize, height: usize) -> Vec<u8> {
        (0..width * height)
            .flat_map(|i| [(i % 256) as u8, (i / width) as u8, 0x80, 0xff])
            .collect()
    }

    #[test]
    fn image_survives_the_png_roundtrip() {
        let rgba = gradient(37, 11);
        let content = Content::image(37, 11, &rgba).expect("encode");
        assert_eq!(content.kind, ClipboardKind::Image);
        let (width, height, decoded) = decode_png(&content.data).expect("decode");
        assert_eq!((width, height), (37, 11));
        assert_eq!(decoded, rgba);
        assert_eq!(image_hash(width, height, &decoded), content.hash);
    }

    #[test]
    fn image_hash_depends_on_pixels_and_dimensions() {
        let rgba = gradient(4, 6);
        let mut other = rgba.clone();
        other[5] ^= 1;
        assert_ne!(image_hash(4, 6, &rgba), image_hash(4, 6, &other));
        // same bytes, different shape
        assert_ne!(image_hash(4, 6, &rgba), image_hash(6, 4, &rgba));
    }

    #[test]
    fn inconsistent_images_are_rejected() {
        assert!(Content::image(4, 4, &[0; 10]).is_err());
        assert!(Content::image(0, 4, &[]).is_err());
    }

    #[test]
    fn only_rgba_png_is_accepted() {
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 2, 2);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("header");
            writer.write_image_data(&[0, 1, 2, 3]).expect("data");
        }
        assert!(matches!(decode_png(&png), Err(ClipboardError::ImageFormat)));
        assert!(decode_png(b"not a png").is_err());
    }

    #[test]
    fn the_start_of_a_text_keeps_whole_characters() {
        assert_eq!(text_start("abc", 10), "abc");
        assert_eq!(text_start("abcdef", 4), "abcd");
        // "é" takes two bytes: not cut in half
        assert_eq!(text_start("aé", 2), "a");
        assert_eq!(text_start("aé", 3), "aé");
        match text_peek("um\ndois\ntrês", false, false) {
            ClipboardPeek::Text {
                start,
                chars,
                lines,
                formatted,
            } => {
                assert_eq!(start, "um\ndois\ntrês");
                assert_eq!((chars, lines, formatted), (12, 3, false));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(text_peek("senha", false, true), ClipboardPeek::Secret);
    }

    #[test]
    fn thumbnails_keep_the_shape_and_the_colors() {
        // small images stay as they are
        let rgba = gradient(30, 20);
        assert_eq!(thumbnail(30, 20, &rgba, 320), (30, 20, rgba.clone()));
        // wide and tall ones fit in the square
        let wide = vec![200; 1000 * 500 * 4];
        let (w, h, small) = thumbnail(1000, 500, &wide, 320);
        assert_eq!((w, h), (320, 160));
        assert_eq!(small.len(), w * h * 4);
        assert!(
            small.iter().all(|&c| c == 200),
            "a plain color stays the same"
        );
        let (w, h, _) = thumbnail(10, 4000, &vec![0; 10 * 4000 * 4], 320);
        assert_eq!((w, h), (1, 320));
        // half black, half white: the halves stay apart
        let halves: Vec<u8> = (0..400 * 400)
            .flat_map(|i| {
                if i % 400 < 200 {
                    [0, 0, 0, 255]
                } else {
                    [255; 4]
                }
            })
            .collect();
        let (w, _, small) = thumbnail(400, 400, &halves, 100);
        assert_eq!(&small[..4], &[0, 0, 0, 255]);
        assert_eq!(&small[(w - 1) * 4..w * 4], &[255; 4]);
        match image_peek(400, 400, &halves).expect("thumbnail") {
            ClipboardPeek::Image {
                width,
                height,
                thumbnail,
            } => {
                assert_eq!((width, height), (400, 400));
                let (w, h, _) = decode_png(&thumbnail).expect("a PNG");
                assert_eq!((w, h), (320, 320));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn text_and_image_hashes_differ() {
        let bytes = [1, 2, 3, 4];
        assert_ne!(text_hash(&bytes), image_hash(1, 1, &bytes));
        assert_eq!(Content::text("abc".into()).hash, text_hash(b"abc"));
    }
}
