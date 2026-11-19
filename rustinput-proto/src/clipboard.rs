//! Clipboard transfer frames.
//!
//! Clipboard contents do not fit into the fixed size input events, so they are
//! moved in frames of their own over the same connection. The connection is
//! datagram based and may drop or reorder frames; a transfer therefore works
//! like a tiny reliable stream:
//!
//! 1. the sender announces the content with [`ClipboardFrame::Offer`],
//! 2. the receiver accepts with [`ClipboardFrame::Ack`] (zero bytes received)
//!    or declines with [`ClipboardFrame::Cancel`],
//! 3. the sender streams [`ClipboardFrame::Data`] in ascending order,
//! 4. the receiver keeps acknowledging the number of contiguous bytes it has,
//!    and the sender resends from there when no progress is reported.
//!
//! These frames must only be sent to peers that announced
//! [`CAP_CLIPBOARD`](crate::CAP_CLIPBOARD): they exceed the size of an input
//! event, which makes older peers drop the connection.

use num_enum::{IntoPrimitive, TryFromPrimitive};

use crate::ProtocolError;

/// payload bytes carried by one [`ClipboardFrame::Data`]
pub const CLIPBOARD_CHUNK_SIZE: usize = 1024;

const OFFER: u8 = 0x81;
const DATA: u8 = 0x82;
const ACK: u8 = 0x83;
const CANCEL: u8 = 0x84;

/// type byte, transfer id, offset
const DATA_HEADER_SIZE: usize = 1 + 4 + 4;

/// whether a datagram starting with `event_type` is a [`ClipboardFrame`]
pub fn is_clipboard_frame(event_type: u8) -> bool {
    (OFFER..=CANCEL).contains(&event_type)
}

/// what a clipboard transfer carries
#[derive(Clone, Copy, Debug, Eq, PartialEq, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum ClipboardKind {
    /// UTF-8 text
    Text,
    /// an image, encoded as PNG (8 bit RGBA)
    Image,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipboardFrame {
    /// announces a transfer of `size` bytes
    Offer {
        /// identifies the transfer in all further frames
        transfer: u32,
        kind: ClipboardKind,
        size: u32,
        /// hash of the content, lets the receiver recognize content it
        /// already has and verify what it received
        hash: u64,
    },
    /// a piece of the content starting at byte `offset`
    Data {
        transfer: u32,
        offset: u32,
        data: Vec<u8>,
    },
    /// the receiver holds the first `received` bytes of the transfer
    Ack { transfer: u32, received: u32 },
    /// the transfer is abandoned, by either side
    Cancel { transfer: u32 },
}

impl ClipboardFrame {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ClipboardFrame::Offer {
                transfer,
                kind,
                size,
                hash,
            } => {
                let mut buf = Vec::with_capacity(18);
                buf.push(OFFER);
                buf.extend_from_slice(&transfer.to_be_bytes());
                buf.push((*kind).into());
                buf.extend_from_slice(&size.to_be_bytes());
                buf.extend_from_slice(&hash.to_be_bytes());
                buf
            }
            ClipboardFrame::Data {
                transfer,
                offset,
                data,
            } => {
                let mut buf = Vec::with_capacity(DATA_HEADER_SIZE + data.len());
                buf.push(DATA);
                buf.extend_from_slice(&transfer.to_be_bytes());
                buf.extend_from_slice(&offset.to_be_bytes());
                buf.extend_from_slice(data);
                buf
            }
            ClipboardFrame::Ack { transfer, received } => {
                let mut buf = Vec::with_capacity(9);
                buf.push(ACK);
                buf.extend_from_slice(&transfer.to_be_bytes());
                buf.extend_from_slice(&received.to_be_bytes());
                buf
            }
            ClipboardFrame::Cancel { transfer } => {
                let mut buf = Vec::with_capacity(5);
                buf.push(CANCEL);
                buf.extend_from_slice(&transfer.to_be_bytes());
                buf
            }
        }
    }

    pub fn decode(datagram: &[u8]) -> Result<Self, ProtocolError> {
        let malformed = || ProtocolError::Malformed(datagram.len());
        let (&frame_type, mut rest) = datagram.split_first().ok_or_else(malformed)?;
        let frame = match frame_type {
            OFFER => {
                let transfer = take_u32(&mut rest).ok_or_else(malformed)?;
                let kind = take(&mut rest, 1).ok_or_else(malformed)?[0];
                let size = take_u32(&mut rest).ok_or_else(malformed)?;
                let hash = take(&mut rest, 8).ok_or_else(malformed)?;
                ClipboardFrame::Offer {
                    transfer,
                    kind: ClipboardKind::try_from(kind)?,
                    size,
                    hash: u64::from_be_bytes(hash.try_into().expect("8 bytes")),
                }
            }
            DATA => {
                let transfer = take_u32(&mut rest).ok_or_else(malformed)?;
                let offset = take_u32(&mut rest).ok_or_else(malformed)?;
                if rest.is_empty() || rest.len() > CLIPBOARD_CHUNK_SIZE {
                    return Err(malformed());
                }
                let data = std::mem::take(&mut rest).to_vec();
                ClipboardFrame::Data {
                    transfer,
                    offset,
                    data,
                }
            }
            ACK => ClipboardFrame::Ack {
                transfer: take_u32(&mut rest).ok_or_else(malformed)?,
                received: take_u32(&mut rest).ok_or_else(malformed)?,
            },
            CANCEL => ClipboardFrame::Cancel {
                transfer: take_u32(&mut rest).ok_or_else(malformed)?,
            },
            _ => return Err(malformed()),
        };
        if !rest.is_empty() {
            return Err(malformed());
        }
        Ok(frame)
    }
}

fn take<'a>(buf: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
    if buf.len() < len {
        return None;
    }
    let (head, tail) = buf.split_at(len);
    *buf = tail;
    Some(head)
}

fn take_u32(buf: &mut &[u8]) -> Option<u32> {
    take(buf, 4).map(|b| u32::from_be_bytes(b.try_into().expect("4 bytes")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Datagram, MAX_DATAGRAM_SIZE};

    fn roundtrip(frame: ClipboardFrame) {
        let encoded = frame.encode();
        assert!(encoded.len() <= MAX_DATAGRAM_SIZE);
        assert!(is_clipboard_frame(encoded[0]));
        assert_eq!(ClipboardFrame::decode(&encoded).expect("decode"), frame);
        // and through the generic entry point
        match Datagram::decode(&encoded).expect("decode") {
            Datagram::Clipboard(decoded) => assert_eq!(decoded, frame),
            Datagram::Event(event) => panic!("decoded as event {event}"),
        }
    }

    #[test]
    fn frames_roundtrip() {
        roundtrip(ClipboardFrame::Offer {
            transfer: 7,
            kind: ClipboardKind::Image,
            size: 3_000_000,
            hash: 0x0123_4567_89ab_cdef,
        });
        roundtrip(ClipboardFrame::Data {
            transfer: 7,
            offset: 2048,
            data: vec![0xab; CLIPBOARD_CHUNK_SIZE],
        });
        roundtrip(ClipboardFrame::Data {
            transfer: u32::MAX,
            offset: u32::MAX,
            data: vec![1],
        });
        roundtrip(ClipboardFrame::Ack {
            transfer: 7,
            received: 4096,
        });
        roundtrip(ClipboardFrame::Cancel { transfer: 7 });
    }

    #[test]
    fn input_events_are_not_clipboard_frames() {
        for event_type in 0..=0x80u8 {
            assert!(!is_clipboard_frame(event_type));
        }
    }

    #[test]
    fn truncated_frames_are_rejected() {
        let offer = ClipboardFrame::Offer {
            transfer: 1,
            kind: ClipboardKind::Text,
            size: 10,
            hash: 1,
        }
        .encode();
        for len in 0..offer.len() {
            assert!(ClipboardFrame::decode(&offer[..len]).is_err(), "len {len}");
        }
        let ack = ClipboardFrame::Ack {
            transfer: 1,
            received: 2,
        }
        .encode();
        for len in 0..ack.len() {
            assert!(ClipboardFrame::decode(&ack[..len]).is_err(), "len {len}");
        }
    }

    #[test]
    fn invalid_frames_are_rejected() {
        // trailing bytes
        let mut cancel = ClipboardFrame::Cancel { transfer: 1 }.encode();
        cancel.push(0);
        assert!(ClipboardFrame::decode(&cancel).is_err());
        // data without payload
        let empty = [DATA, 0, 0, 0, 1, 0, 0, 0, 0];
        assert!(ClipboardFrame::decode(&empty).is_err());
        // data with an oversized payload
        let mut oversized = empty.to_vec();
        oversized.extend(vec![0; CLIPBOARD_CHUNK_SIZE + 1]);
        assert!(ClipboardFrame::decode(&oversized).is_err());
        // unknown content kind
        let mut offer = ClipboardFrame::Offer {
            transfer: 1,
            kind: ClipboardKind::Text,
            size: 10,
            hash: 1,
        }
        .encode();
        offer[5] = 9;
        assert!(matches!(
            ClipboardFrame::decode(&offer),
            Err(ProtocolError::InvalidClipboardKind(_))
        ));
    }
}
