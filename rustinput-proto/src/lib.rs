use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};
use num_enum::{IntoPrimitive, TryFromPrimitive, TryFromPrimitiveError};
use paste::paste;
use std::{
    fmt::{Debug, Display, Formatter},
    mem::size_of,
};
use thiserror::Error;

/// defines the maximum size an encoded event can take up
/// this is currently the pointer motion event
/// type: u8, time: u32, dx: f64, dy: f64
pub const MAX_EVENT_SIZE: usize = size_of::<u8>() + size_of::<u32>() + 2 * size_of::<f64>();

pub mod clipboard;

pub use clipboard::{ClipboardFrame, ClipboardKind};

/// largest datagram a peer may send, receive buffers are sized after it
pub const MAX_DATAGRAM_SIZE: usize = 1200;

/// capability flag: the peer synchronizes the clipboard
/// (see [`ProtoEvent::Capabilities`])
pub const CAP_CLIPBOARD: u32 = 1 << 0;

/// capability flag: the peer understands [`ProtoEvent::EnterAt`] and
/// [`ProtoEvent::LeaveAt`], which keep the pointer at the same relative
/// position along the edge it crosses
pub const CAP_CURSOR_POSITION: u32 = 1 << 1;

/// capability flag: the peer exchanges copied files over its file channel, a
/// TLS connection over TCP on the port number of the input channel (see
/// the RustInput documentation); implies [`CAP_CLIPBOARD`]
pub const CAP_FILES: u32 = 1 << 2;

/// Serial of the [`ProtoEvent::Ack`] the connect side sends right after
/// connecting to find out whether the peer knows the newer events.
///
/// Lan Mouse up to 0.11.0 closes the connection when it receives an event
/// type it does not know, so [`ProtoEvent::Hello`] and
/// [`ProtoEvent::Capabilities`] can not be sent blindly. An `Ack` coming
/// from the connect side on the other hand is ignored by every version. A
/// listener that understands the probe answers with its `Hello` and
/// `Capabilities`; only then does the connect side send its own.
pub const HANDSHAKE_PROBE: u32 = 0x5249_4e50;

/// error type for protocol violations
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// event type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidEventId(#[from] TryFromPrimitiveError<EventType>),
    /// position type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidPosition(#[from] TryFromPrimitiveError<Position>),
    /// clipboard content type does not exist
    #[error("invalid clipboard kind: `{0}`")]
    InvalidClipboardKind(#[from] TryFromPrimitiveError<ClipboardKind>),
    /// the datagram is empty, cut short or longer than its type allows
    #[error("malformed datagram of {0} bytes")]
    Malformed(usize),
}

/// Position of a client
#[derive(Clone, Copy, Debug, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Display for Position {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

/// main rustinput protocol event type
#[derive(Clone, Copy, Debug)]
pub enum ProtoEvent {
    /// notify a client that the cursor entered its region at the given position
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Enter(Position),
    /// notify a client that the cursor left its region
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Leave(u32),
    /// acknowledge of an [`ProtoEvent::Enter`] or [`ProtoEvent::Leave`] event
    Ack(u32),
    /// Input event
    Input(InputEvent),
    /// Ping event for tracking unresponsive clients.
    /// A client has to respond with [`ProtoEvent::Pong`].
    Ping,
    /// Response to [`ProtoEvent::Ping`], true if emulation is enabled / available
    Pong(bool),
    /// Build identification for the sending peer. Sent by the
    /// connect side once after the connection authenticates, and
    /// echoed back by the listen side in reply, so each end can
    /// display the peer's build hash and warn (soft) on mismatch.
    /// `commit` is the 8-byte ASCII short commit hash from
    /// `shadow_rs`'s `SHORT_COMMIT`. Old peers that don't
    /// recognize the event type silently skip it per the
    /// forward-compat handling in the receive loop.
    Hello { commit: [u8; 8] },
    /// Optional features of the sending peer as a set of `CAP_*` flags.
    /// The listen side sends its flags in answer to [`HANDSHAKE_PROBE`],
    /// the connect side follows with its own. A peer that never sends
    /// this event has no optional features: datagrams larger than
    /// [`MAX_EVENT_SIZE`] must only be sent to peers that announced the
    /// feature they belong to.
    Capabilities(u32),
    /// [`ProtoEvent::Enter`] telling where along the edge the pointer
    /// crossed, from 0.0 (top or left end of the sending device's desktop)
    /// to 1.0 (bottom or right end). The receiver puts its pointer at the
    /// same relative position of its own edge. Only sent to peers with
    /// [`CAP_CURSOR_POSITION`].
    EnterAt(Position, f64),
    /// [`ProtoEvent::Leave`] sent by the controlled device when the pointer
    /// leaves it, telling where along its edge it left (see
    /// [`ProtoEvent::EnterAt`]), so the controlling device can put its
    /// pointer there. Only sent to peers with [`CAP_CURSOR_POSITION`].
    LeaveAt(f64),
}

impl Display for ProtoEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoEvent::Enter(s) => write!(f, "Enter({s})"),
            ProtoEvent::Leave(s) => write!(f, "Leave({s})"),
            ProtoEvent::Ack(s) => write!(f, "Ack({s})"),
            ProtoEvent::Input(e) => write!(f, "{e}"),
            ProtoEvent::Ping => write!(f, "ping"),
            ProtoEvent::Pong(alive) => {
                write!(
                    f,
                    "pong: {}",
                    if *alive { "alive" } else { "not available" }
                )
            }
            ProtoEvent::Hello { commit } => {
                let s = std::str::from_utf8(commit).unwrap_or("????????");
                write!(f, "Hello({s})")
            }
            ProtoEvent::Capabilities(flags) => write!(f, "Capabilities({flags:#x})"),
            ProtoEvent::EnterAt(pos, at) => write!(f, "EnterAt({pos}, {at:.3})"),
            ProtoEvent::LeaveAt(at) => write!(f, "LeaveAt({at:.3})"),
        }
    }
}

#[derive(TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum EventType {
    PointerMotion,
    PointerButton,
    PointerAxis,
    PointerAxisValue120,
    KeyboardKey,
    KeyboardModifiers,
    Ping,
    Pong,
    Enter,
    Leave,
    Ack,
    Hello,
    // RustInput extensions start at 0x80 to stay clear of event types
    // added upstream; 0x81 to 0x84 are the clipboard frames
    Capabilities = 0x80,
    EnterAt = 0x85,
    LeaveAt = 0x86,
}

impl ProtoEvent {
    fn event_type(&self) -> EventType {
        match self {
            ProtoEvent::Input(e) => match e {
                InputEvent::Pointer(p) => match p {
                    PointerEvent::Motion { .. } => EventType::PointerMotion,
                    PointerEvent::Button { .. } => EventType::PointerButton,
                    PointerEvent::Axis { .. } => EventType::PointerAxis,
                    PointerEvent::AxisDiscrete120 { .. } => EventType::PointerAxisValue120,
                },
                InputEvent::Keyboard(k) => match k {
                    KeyboardEvent::Key { .. } => EventType::KeyboardKey,
                    KeyboardEvent::Modifiers { .. } => EventType::KeyboardModifiers,
                },
            },
            ProtoEvent::Ping => EventType::Ping,
            ProtoEvent::Pong(_) => EventType::Pong,
            ProtoEvent::Enter(_) => EventType::Enter,
            ProtoEvent::Leave(_) => EventType::Leave,
            ProtoEvent::Ack(_) => EventType::Ack,
            ProtoEvent::Hello { .. } => EventType::Hello,
            ProtoEvent::Capabilities(_) => EventType::Capabilities,
            ProtoEvent::EnterAt(..) => EventType::EnterAt,
            ProtoEvent::LeaveAt(_) => EventType::LeaveAt,
        }
    }
}

impl TryFrom<[u8; MAX_EVENT_SIZE]> for ProtoEvent {
    type Error = ProtocolError;

    fn try_from(buf: [u8; MAX_EVENT_SIZE]) -> Result<Self, Self::Error> {
        let mut buf = &buf[..];
        let event_type = decode_u8(&mut buf)?;
        match EventType::try_from(event_type)? {
            EventType::PointerMotion => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Motion {
                    time: decode_u32(&mut buf)?,
                    dx: decode_f64(&mut buf)?,
                    dy: decode_f64(&mut buf)?,
                })))
            }
            EventType::PointerButton => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Button {
                    time: decode_u32(&mut buf)?,
                    button: decode_u32(&mut buf)?,
                    state: decode_u32(&mut buf)?,
                })))
            }
            EventType::PointerAxis => Ok(Self::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: decode_u32(&mut buf)?,
                axis: decode_u8(&mut buf)?,
                value: decode_f64(&mut buf)?,
            }))),
            EventType::PointerAxisValue120 => Ok(Self::Input(InputEvent::Pointer(
                PointerEvent::AxisDiscrete120 {
                    axis: decode_u8(&mut buf)?,
                    value: decode_i32(&mut buf)?,
                },
            ))),
            EventType::KeyboardKey => Ok(Self::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: decode_u32(&mut buf)?,
                key: decode_u32(&mut buf)?,
                state: decode_u8(&mut buf)?,
            }))),
            EventType::KeyboardModifiers => Ok(Self::Input(InputEvent::Keyboard(
                KeyboardEvent::Modifiers {
                    depressed: decode_u32(&mut buf)?,
                    latched: decode_u32(&mut buf)?,
                    locked: decode_u32(&mut buf)?,
                    group: decode_u32(&mut buf)?,
                },
            ))),
            EventType::Ping => Ok(Self::Ping),
            EventType::Pong => Ok(Self::Pong(decode_u8(&mut buf)? != 0)),
            EventType::Enter => Ok(Self::Enter(decode_u8(&mut buf)?.try_into()?)),
            EventType::Leave => Ok(Self::Leave(decode_u32(&mut buf)?)),
            EventType::Ack => Ok(Self::Ack(decode_u32(&mut buf)?)),
            EventType::Hello => {
                let mut commit = [0u8; 8];
                for b in commit.iter_mut() {
                    *b = decode_u8(&mut buf)?;
                }
                Ok(Self::Hello { commit })
            }
            EventType::Capabilities => Ok(Self::Capabilities(decode_u32(&mut buf)?)),
            EventType::EnterAt => Ok(Self::EnterAt(
                decode_u8(&mut buf)?.try_into()?,
                decode_f64(&mut buf)?,
            )),
            EventType::LeaveAt => Ok(Self::LeaveAt(decode_f64(&mut buf)?)),
        }
    }
}

impl From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize) {
    fn from(event: ProtoEvent) -> Self {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        let mut len = 0usize;
        {
            let mut buf = &mut buf[..];
            let buf = &mut buf;
            let len = &mut len;
            encode_u8(buf, len, event.event_type() as u8);
            match event {
                ProtoEvent::Input(event) => match event {
                    InputEvent::Pointer(p) => match p {
                        PointerEvent::Motion { time, dx, dy } => {
                            encode_u32(buf, len, time);
                            encode_f64(buf, len, dx);
                            encode_f64(buf, len, dy);
                        }
                        PointerEvent::Button {
                            time,
                            button,
                            state,
                        } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, button);
                            encode_u32(buf, len, state);
                        }
                        PointerEvent::Axis { time, axis, value } => {
                            encode_u32(buf, len, time);
                            encode_u8(buf, len, axis);
                            encode_f64(buf, len, value);
                        }
                        PointerEvent::AxisDiscrete120 { axis, value } => {
                            encode_u8(buf, len, axis);
                            encode_i32(buf, len, value);
                        }
                    },
                    InputEvent::Keyboard(k) => match k {
                        KeyboardEvent::Key { time, key, state } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, key);
                            encode_u8(buf, len, state);
                        }
                        KeyboardEvent::Modifiers {
                            depressed,
                            latched,
                            locked,
                            group,
                        } => {
                            encode_u32(buf, len, depressed);
                            encode_u32(buf, len, latched);
                            encode_u32(buf, len, locked);
                            encode_u32(buf, len, group);
                        }
                    },
                },
                ProtoEvent::Ping => {}
                ProtoEvent::Pong(alive) => encode_u8(buf, len, alive as u8),
                ProtoEvent::Enter(pos) => encode_u8(buf, len, pos as u8),
                ProtoEvent::Leave(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Ack(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Hello { commit } => {
                    for b in commit.iter() {
                        encode_u8(buf, len, *b);
                    }
                }
                ProtoEvent::Capabilities(flags) => encode_u32(buf, len, flags),
                ProtoEvent::EnterAt(pos, at) => {
                    encode_u8(buf, len, pos as u8);
                    encode_f64(buf, len, at);
                }
                ProtoEvent::LeaveAt(at) => encode_f64(buf, len, at),
            }
        }
        (buf, len)
    }
}

/// anything a peer can send in one datagram
#[derive(Clone, Debug)]
pub enum Datagram {
    Event(ProtoEvent),
    Clipboard(ClipboardFrame),
}

impl Datagram {
    /// decodes one received datagram
    pub fn decode(datagram: &[u8]) -> Result<Self, ProtocolError> {
        let Some(&event_type) = datagram.first() else {
            return Err(ProtocolError::Malformed(0));
        };
        if clipboard::is_clipboard_frame(event_type) {
            return ClipboardFrame::decode(datagram).map(Self::Clipboard);
        }
        if datagram.len() > MAX_EVENT_SIZE {
            return Err(ProtocolError::Malformed(datagram.len()));
        }
        // events are decoded from a fixed size buffer
        let mut buf = [0u8; MAX_EVENT_SIZE];
        buf[..datagram.len()].copy_from_slice(datagram);
        ProtoEvent::try_from(buf).map(Self::Event)
    }
}

macro_rules! decode_impl {
    ($t:ty) => {
        paste! {
            fn [<decode_ $t>](data: &mut &[u8]) -> Result<$t, ProtocolError> {
                let (int_bytes, rest) = data.split_at(size_of::<$t>());
                *data = rest;
                Ok($t::from_be_bytes(int_bytes.try_into().unwrap()))
            }
        }
    };
}

decode_impl!(u8);
decode_impl!(u32);
decode_impl!(i32);
decode_impl!(f64);

macro_rules! encode_impl {
    ($t:ty) => {
        paste! {
            fn [<encode_ $t>](buf: &mut &mut [u8], amt: &mut usize, n: $t) {
                let src = n.to_be_bytes();
                let data = std::mem::take(buf);
                let (int_bytes, rest) = data.split_at_mut(size_of::<$t>());
                int_bytes.copy_from_slice(&src);
                *amt += size_of::<$t>();
                *buf = rest
            }
        }
    };
}

encode_impl!(u8);
encode_impl!(u32);
encode_impl!(i32);
encode_impl!(f64);

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(event: ProtoEvent) -> ProtoEvent {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        match Datagram::decode(&buf[..len]).expect("decode") {
            Datagram::Event(event) => event,
            Datagram::Clipboard(frame) => panic!("unexpected clipboard frame {frame:?}"),
        }
    }

    #[test]
    fn capabilities_roundtrip() {
        match roundtrip(ProtoEvent::Capabilities(CAP_CLIPBOARD | 0x8000_0000)) {
            ProtoEvent::Capabilities(flags) => assert_eq!(flags, CAP_CLIPBOARD | 0x8000_0000),
            event => panic!("unexpected event {event}"),
        }
    }

    #[test]
    fn cursor_position_events_roundtrip() {
        match roundtrip(ProtoEvent::EnterAt(Position::Right, 0.25)) {
            ProtoEvent::EnterAt(Position::Right, at) => assert_eq!(at, 0.25),
            event => panic!("unexpected event {event}"),
        }
        match roundtrip(ProtoEvent::LeaveAt(1.0)) {
            ProtoEvent::LeaveAt(at) => assert_eq!(at, 1.0),
            event => panic!("unexpected event {event}"),
        }
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) =
            ProtoEvent::EnterAt(Position::Left, 0.5).into();
        assert_eq!(&buf[..2], &[0x85, 0]);
        assert_eq!(len, 10);
        // the new event types are no clipboard frames
        assert!(!clipboard::is_clipboard_frame(0x85));
        assert!(!clipboard::is_clipboard_frame(0x86));
    }

    #[test]
    fn events_keep_their_wire_format() {
        // the first byte is the event type, upstream peers rely on it
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::Ping.into();
        assert_eq!(&buf[..len], &[6]);
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::Ack(1).into();
        assert_eq!(&buf[..len], &[10, 0, 0, 0, 1]);
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::Capabilities(1).into();
        assert_eq!(&buf[..len], &[0x80, 0, 0, 0, 1]);
    }

    #[test]
    fn input_event_roundtrip() {
        let motion = ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Motion {
            time: 42,
            dx: -1.5,
            dy: 3.0,
        }));
        match roundtrip(motion) {
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Motion { time, dx, dy })) => {
                assert_eq!((time, dx, dy), (42, -1.5, 3.0));
            }
            event => panic!("unexpected event {event}"),
        }
    }

    #[test]
    fn malformed_datagrams_are_rejected() {
        assert!(matches!(
            Datagram::decode(&[]),
            Err(ProtocolError::Malformed(0))
        ));
        // unknown event type
        assert!(matches!(
            Datagram::decode(&[0x7f]),
            Err(ProtocolError::InvalidEventId(_))
        ));
        // an event can not be longer than the largest event
        assert!(matches!(
            Datagram::decode(&[6; MAX_EVENT_SIZE + 1]),
            Err(ProtocolError::Malformed(_))
        ));
    }
}
