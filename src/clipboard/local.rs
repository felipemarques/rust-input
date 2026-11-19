//! Access to the clipboard of this machine.
//!
//! Reading the clipboard can take a while (the application owning it has to
//! answer, images have to be converted), so everything happens on a dedicated
//! thread and the async side only exchanges messages with it.

use std::{borrow::Cow, io::Cursor, sync::mpsc};

use rustinput_proto::ClipboardKind;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::oneshot;

use super::Content;

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
}

enum Request {
    Read(oneshot::Sender<Option<Content>>),
    Write(Content),
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

    /// the current content of the clipboard, if it holds text or an image
    pub(crate) async fn read(&self) -> Option<Content> {
        let (reply, content) = oneshot::channel();
        self.requests.send(Request::Read(reply)).ok()?;
        content.await.ok().flatten()
    }

    /// replaces the content of the clipboard
    pub(crate) fn write(&self, content: Content) {
        let _ = self.requests.send(Request::Write(content));
    }
}

fn worker(requests: mpsc::Receiver<Request>) {
    let mut clipboard: Option<arboard::Clipboard> = None;
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

/// Text wins over an image: applications offering both (spreadsheets, word
/// processors) mean the text, the image is only a rendering of it.
fn read(clipboard: &mut arboard::Clipboard) -> Option<Content> {
    match clipboard.get_text() {
        Ok(text) if !text.is_empty() => return Some(Content::text(text)),
        Ok(_) | Err(arboard::Error::ContentNotAvailable) => {}
        Err(e) => log::debug!("could not read text from the clipboard: {e}"),
    }
    match clipboard.get_image() {
        Ok(image) => match Content::image(image.width, image.height, &image.bytes) {
            Ok(content) => Some(content),
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
    }
    Ok(())
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
    fn text_and_image_hashes_differ() {
        let bytes = [1, 2, 3, 4];
        assert_ne!(text_hash(&bytes), image_hash(1, 1, &bytes));
        assert_eq!(Content::text("abc".into()).hash, text_hash(b"abc"));
    }
}
