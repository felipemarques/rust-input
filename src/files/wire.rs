//! Frames of the file channel.
//!
//! Every frame is a type byte, a big endian u32 payload length and the
//! payload. Both sides start with [`MAGIC`], so a peer speaking something else
//! is recognized right away.

use std::io;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// sent by both sides before the first frame: "RustInput Files", version 1
pub(crate) const MAGIC: [u8; 4] = *b"RIF1";

/// payload of one data frame
pub(crate) const CHUNK_SIZE: usize = 256 * 1024;

/// largest accepted frame, offers of many files are the largest ones
const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

const OFFER: u8 = 1;
const ACCEPT: u8 = 2;
const DECLINE: u8 = 3;
const DATA: u8 = 4;
const CANCEL: u8 = 5;
const COMPLETE: u8 = 6;
const ABORT: u8 = 7;

/// a file or folder of a transfer
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Entry {
    /// path inside the transfer, components separated by `/`; the first
    /// component is one of the items that were copied
    pub(crate) path: String,
    pub(crate) dir: bool,
    /// size in bytes, 0 for folders
    pub(crate) size: u64,
    /// modification time in seconds since the Unix epoch
    pub(crate) modified: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Frame {
    /// the sender proposes a transfer of `entries`
    Offer { id: u32, entries: Vec<Entry> },
    /// the receiver wants the data
    Accept { id: u32 },
    /// the receiver does not want the transfer
    Decline { id: u32, reason: String },
    /// the next bytes of the transfer: the contents of the files of the offer,
    /// in order, a chunk never spans two files
    Data { id: u32, bytes: Vec<u8> },
    /// the sender abandons the transfer; also its answer to [`Frame::Abort`]
    Cancel { id: u32 },
    /// the receiver has everything
    Complete { id: u32 },
    /// the receiver abandons the transfer, the sender answers with
    /// [`Frame::Cancel`] once it stopped sending
    Abort { id: u32 },
}

impl Frame {
    pub(crate) fn id(&self) -> u32 {
        match self {
            Frame::Offer { id, .. }
            | Frame::Accept { id }
            | Frame::Decline { id, .. }
            | Frame::Data { id, .. }
            | Frame::Cancel { id }
            | Frame::Complete { id }
            | Frame::Abort { id } => *id,
        }
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

pub(crate) async fn write_magic<W: AsyncWrite + Unpin>(writer: &mut W) -> io::Result<()> {
    writer.write_all(&MAGIC).await?;
    writer.flush().await
}

/// the first four bytes of a connection on the TCP port: [`MAGIC`] for the
/// file channel, [`crate::pairing::MAGIC`] for pairing
pub(crate) async fn read_any_magic<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<[u8; 4]> {
    let mut magic = [0; 4];
    reader.read_exact(&mut magic).await?;
    Ok(magic)
}

pub(crate) async fn read_magic<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<()> {
    let mut magic = [0; 4];
    reader.read_exact(&mut magic).await?;
    if magic != MAGIC {
        return Err(invalid("not a RustInput file channel"));
    }
    Ok(())
}

pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &Frame,
) -> io::Result<()> {
    let (kind, rest): (u8, Vec<u8>) = match frame {
        Frame::Offer { entries, .. } => (OFFER, serde_json::to_vec(entries)?),
        Frame::Accept { .. } => (ACCEPT, vec![]),
        Frame::Decline { reason, .. } => (DECLINE, reason.as_bytes().to_vec()),
        Frame::Data { .. } => (DATA, vec![]),
        Frame::Cancel { .. } => (CANCEL, vec![]),
        Frame::Complete { .. } => (COMPLETE, vec![]),
        Frame::Abort { .. } => (ABORT, vec![]),
    };
    // data frames are written without copying their bytes
    let data: &[u8] = match frame {
        Frame::Data { bytes, .. } => bytes,
        _ => &rest,
    };
    let length = 4 + data.len();
    if length > MAX_FRAME_SIZE {
        return Err(invalid(format!("frame of {length} bytes is too large")));
    }
    let mut header = [0; 9];
    header[0] = kind;
    header[1..5].copy_from_slice(&(length as u32).to_be_bytes());
    header[5..9].copy_from_slice(&frame.id().to_be_bytes());
    writer.write_all(&header).await?;
    writer.write_all(data).await?;
    Ok(())
}

pub(crate) async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Frame> {
    let mut header = [0; 5];
    reader.read_exact(&mut header).await?;
    let kind = header[0];
    let length = u32::from_be_bytes(header[1..5].try_into().expect("4 bytes")) as usize;
    if !(4..=MAX_FRAME_SIZE).contains(&length) {
        return Err(invalid(format!("invalid frame length {length}")));
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).await?;
    let id = u32::from_be_bytes(payload[..4].try_into().expect("4 bytes"));
    let rest = payload.split_off(4);
    let frame = match kind {
        OFFER => Frame::Offer {
            id,
            entries: serde_json::from_slice(&rest)
                .map_err(|e| invalid(format!("invalid offer: {e}")))?,
        },
        ACCEPT => Frame::Accept { id },
        DECLINE => Frame::Decline {
            id,
            reason: String::from_utf8_lossy(&rest).into_owned(),
        },
        DATA => Frame::Data { id, bytes: rest },
        CANCEL => Frame::Cancel { id },
        COMPLETE => Frame::Complete { id },
        ABORT => Frame::Abort { id },
        _ => return Err(invalid(format!("unknown frame type {kind}"))),
    };
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn roundtrip(frame: Frame) -> Frame {
        let (mut a, mut b) = tokio::io::duplex(1024 * 1024);
        write_frame(&mut a, &frame).await.expect("write");
        read_frame(&mut b).await.expect("read")
    }

    #[tokio::test]
    async fn frames_roundtrip() {
        let entries = vec![
            Entry {
                path: "Fotos".into(),
                dir: true,
                size: 0,
                modified: Some(1_700_000_000),
            },
            Entry {
                path: "Fotos/praia ç.jpg".into(),
                dir: false,
                size: 123_456,
                modified: None,
            },
        ];
        for frame in [
            Frame::Offer { id: 7, entries },
            Frame::Accept { id: 7 },
            Frame::Decline {
                id: 7,
                reason: "sem espaço".into(),
            },
            Frame::Data {
                id: u32::MAX,
                bytes: vec![0xab; CHUNK_SIZE],
            },
            Frame::Data {
                id: 1,
                bytes: vec![],
            },
            Frame::Cancel { id: 7 },
            Frame::Complete { id: 7 },
            Frame::Abort { id: 7 },
        ] {
            assert_eq!(roundtrip(frame.clone()).await, frame);
        }
    }

    #[tokio::test]
    async fn magic_is_checked() {
        let (mut a, mut b) = tokio::io::duplex(64);
        write_magic(&mut a).await.expect("write");
        read_magic(&mut b).await.expect("read");
        a.write_all(b"HTTP").await.expect("write");
        assert!(read_magic(&mut b).await.is_err());
    }

    #[tokio::test]
    async fn invalid_frames_are_rejected() {
        let (mut a, mut b) = tokio::io::duplex(64);
        // unknown type
        a.write_all(&[99, 0, 0, 0, 4, 0, 0, 0, 1])
            .await
            .expect("write");
        assert!(read_frame(&mut b).await.is_err());
        // shorter than the id
        a.write_all(&[ACCEPT, 0, 0, 0, 2, 0, 0])
            .await
            .expect("write");
        assert!(read_frame(&mut b).await.is_err());
        // larger than allowed
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[DATA, 0xff, 0xff, 0xff, 0xff])
            .await
            .expect("write");
        assert!(read_frame(&mut b).await.is_err());
        // an offer that is not a list of entries
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[OFFER, 0, 0, 0, 6, 0, 0, 0, 1, b'{', b'}'])
            .await
            .expect("write");
        assert!(read_frame(&mut b).await.is_err());
    }
}
