//! The datagrams of the shared sound (0.52.0): 5 ms of 48 kHz stereo 16-bit
//! sound each, sealed with ChaCha20-Poly1305 under the key of its stream,
//! which the listening computer handed to the sending one over the
//! authenticated channel of the arrangement.
//!
//! `RIS1` | stream (8 bytes) | number (4 bytes, big endian) | sealed sound
//! and its tag. The head is authenticated too, and the nonce is the stream
//! and the number: a key belongs to one stream, whose numbers only grow.

use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};

pub(crate) const RATE: u32 = 48_000;
pub(crate) const CHANNELS: usize = 2;
/// 5 ms
pub(crate) const FRAMES: usize = 240;
/// the samples of a datagram, both channels
pub(crate) const SAMPLES: usize = FRAMES * CHANNELS;
pub(crate) const PCM_BYTES: usize = SAMPLES * 2;
const MAGIC: &[u8; 4] = b"RIS1";
const HEAD: usize = 16;
const TAG: usize = 16;
/// the size of a datagram
pub(crate) const DATAGRAM: usize = HEAD + PCM_BYTES + TAG;

/// the id of a stream, which the listening computer picks
pub(crate) type StreamId = [u8; 8];

/// what seals and opens the datagrams of one stream
pub(crate) struct Seal {
    stream: StreamId,
    key: LessSafeKey,
}

impl Seal {
    pub(crate) fn new(stream: StreamId, key: &[u8; 32]) -> Self {
        let key = UnboundKey::new(&CHACHA20_POLY1305, key).expect("a key of 32 bytes");
        Self {
            stream,
            key: LessSafeKey::new(key),
        }
    }

    fn nonce(&self, number: u32) -> Nonce {
        let mut nonce = [0u8; 12];
        nonce[..8].copy_from_slice(&self.stream);
        nonce[8..].copy_from_slice(&number.to_be_bytes());
        Nonce::assume_unique_for_key(nonce)
    }

    /// the datagram `number` with `pcm` (little-endian samples)
    pub(crate) fn seal(&self, number: u32, pcm: &[u8; PCM_BYTES]) -> Vec<u8> {
        let mut out = Vec::with_capacity(DATAGRAM);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.stream);
        out.extend_from_slice(&number.to_be_bytes());
        let mut body = pcm.to_vec();
        self.key
            .seal_in_place_append_tag(self.nonce(number), Aad::from(&out[..HEAD]), &mut body)
            .expect("sealing a datagram");
        out.extend_from_slice(&body);
        out
    }

    /// the number and the sound of a datagram of this stream, `None` when
    /// it is not one or was not sealed with this key
    pub(crate) fn open(&self, datagram: &[u8]) -> Option<(u32, [u8; PCM_BYTES])> {
        let (head, body) = (datagram.get(..HEAD)?, datagram.get(HEAD..)?);
        if datagram.len() != DATAGRAM || &head[..4] != MAGIC || head[4..12] != self.stream {
            return None;
        }
        let number = u32::from_be_bytes(head[12..16].try_into().ok()?);
        let mut body = body.to_vec();
        let pcm = self
            .key
            .open_in_place(self.nonce(number), Aad::from(head), &mut body)
            .ok()?;
        Some((number, pcm.try_into().ok()?))
    }
}

/// the stream a datagram says it belongs to
pub(crate) fn stream_of(datagram: &[u8]) -> Option<StreamId> {
    (datagram.len() == DATAGRAM && &datagram[..4] == MAGIC)
        .then(|| datagram[4..12].try_into().ok())
        .flatten()
}

/// the samples of a datagram
pub(crate) fn samples(pcm: &[u8; PCM_BYTES]) -> [i16; SAMPLES] {
    let mut out = [0i16; SAMPLES];
    for (i, pair) in pcm.as_chunks::<2>().0.iter().enumerate() {
        out[i] = i16::from_le_bytes(*pair);
    }
    out
}

/// samples as the bytes of a datagram
pub(crate) fn bytes(samples: &[i16; SAMPLES]) -> [u8; PCM_BYTES] {
    let mut out = [0u8; PCM_BYTES];
    for (i, s) in samples.iter().enumerate() {
        out[2 * i..2 * i + 2].copy_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_and_opened() {
        let seal = Seal::new(*b"stream01", &[7; 32]);
        let mut pcm = [0u8; PCM_BYTES];
        pcm[..4].copy_from_slice(&[1, 2, 3, 4]);
        let datagram = seal.seal(42, &pcm);
        assert_eq!(datagram.len(), DATAGRAM);
        assert_eq!(stream_of(&datagram), Some(*b"stream01"));
        assert_eq!(seal.open(&datagram), Some((42, pcm)));
        // another key, another stream, a changed byte, a changed number
        assert_eq!(Seal::new(*b"stream01", &[8; 32]).open(&datagram), None);
        assert_eq!(Seal::new(*b"stream02", &[7; 32]).open(&datagram), None);
        let mut bad = datagram.clone();
        bad[HEAD + 10] ^= 1;
        assert_eq!(seal.open(&bad), None);
        let mut renumbered = datagram;
        renumbered[15] ^= 1;
        assert_eq!(seal.open(&renumbered), None);
    }

    #[test]
    fn samples_and_bytes() {
        let mut s = [0i16; SAMPLES];
        s[0] = -2;
        s[SAMPLES - 1] = 300;
        assert_eq!(samples(&bytes(&s)), s);
    }
}
