use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Display,
    task::{Poll, ready},
};

use async_trait::async_trait;
use futures::StreamExt;
use futures_core::Stream;

use input_event::{
    Event, KeyboardEvent,
    edge::{Edge, Span},
    scancode,
};

pub use error::{CaptureCreationError, CaptureError, InputCaptureError};

pub mod error;

#[cfg(libei)]
mod libei;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(layer_shell)]
mod layer_shell;

#[cfg(windows)]
mod windows;

#[cfg(x11)]
mod x11;

/// fallback input capture (does not produce events)
mod dummy;

pub type CaptureHandle = u64;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum CaptureEvent {
    /// capture on this capture handle is now active
    Begin,
    /// input event coming from capture handle
    Input(Event),
    /// The capture ended without being asked to: the input reaches this
    /// device again. The Windows backend does this when Windows stops
    /// handing the mouse to its hooks while capturing, for instance over a
    /// window run as administrator.
    Released,
    /// The pointer crossed an edge where no client is: a part of the edge
    /// outside the [`Span`] of every client there. The capture must be
    /// released. Only from backends that can not keep to the spans
    /// themselves (see [`InputCapture::create`]).
    Outside,
}

impl Display for CaptureEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureEvent::Begin => write!(f, "begin capture"),
            CaptureEvent::Input(e) => write!(f, "{e}"),
            CaptureEvent::Released => write!(f, "capture released"),
            CaptureEvent::Outside => write!(f, "crossed outside of every client"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Position {
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Self::Right,
            Position::Right => Self::Left,
            Position::Top => Self::Bottom,
            Position::Bottom => Self::Top,
        }
    }
}

impl From<Position> for Edge {
    fn from(pos: Position) -> Self {
        match pos {
            Position::Left => Edge::Left,
            Position::Right => Edge::Right,
            Position::Top => Edge::Top,
            Position::Bottom => Edge::Bottom,
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    #[cfg(libei)]
    InputCapturePortal,
    #[cfg(layer_shell)]
    LayerShell,
    #[cfg(x11)]
    X11,
    #[cfg(windows)]
    Windows,
    #[cfg(target_os = "macos")]
    MacOs,
    Dummy,
}

impl Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(libei)]
            Backend::InputCapturePortal => write!(f, "input-capture-portal"),
            #[cfg(layer_shell)]
            Backend::LayerShell => write!(f, "layer-shell"),
            #[cfg(x11)]
            Backend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            Backend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            Backend::MacOs => write!(f, "MacOS"),
            Backend::Dummy => write!(f, "dummy"),
        }
    }
}

pub struct InputCapture {
    /// capture backend
    capture: Box<dyn Capture>,
    /// keys pressed by active capture
    pressed_keys: HashSet<scancode::Linux>,
    /// map from position to ids
    position_map: HashMap<Position, Vec<CaptureHandle>>,
    /// map from id to position
    id_map: HashMap<CaptureHandle, Position>,
    /// the part of its edge that leads to each client
    spans: HashMap<CaptureHandle, Span>,
    /// the clients the current capture goes to
    active: Vec<CaptureHandle>,
    /// the client [`InputCapture::enter_client`] asked for
    entering: Option<CaptureHandle>,
    /// pending events
    pending: VecDeque<(CaptureHandle, CaptureEvent)>,
}

impl InputCapture {
    /// Creates a client at `pos`, reached through `span` of that edge.
    /// Several clients may share an edge: the capture goes to the one whose
    /// span the pointer crossed (to all of them where spans overlap, as when
    /// they all have the whole edge).
    pub async fn create(
        &mut self,
        id: CaptureHandle,
        pos: Position,
        span: Span,
    ) -> Result<(), CaptureError> {
        assert!(!self.id_map.contains_key(&id));

        self.id_map.insert(id, pos);
        self.spans.insert(id, span);

        if let Some(v) = self.position_map.get_mut(&pos) {
            v.push(id);
        } else {
            self.position_map.insert(pos, vec![id]);
            self.capture.create(pos).await?;
        }
        self.update_spans(pos);
        Ok(())
    }

    /// changes the part of its edge that leads to the client `id`
    pub fn set_span(&mut self, id: CaptureHandle, span: Span) {
        if let Some(&pos) = self.id_map.get(&id) {
            self.spans.insert(id, span);
            self.update_spans(pos);
        }
    }

    /// tells the backend the parts of the edge at `pos` that lead somewhere:
    /// none for the whole edge
    fn update_spans(&mut self, pos: Position) {
        let spans: Vec<Span> = self
            .position_map
            .get(&pos)
            .map(|ids| ids.iter().map(|id| self.span(*id)).collect())
            .unwrap_or_default();
        let spans = if spans.iter().any(Span::is_full) {
            Vec::new()
        } else {
            spans
        };
        self.capture.set_spans(pos, spans);
    }

    fn span(&self, id: CaptureHandle) -> Span {
        self.spans.get(&id).copied().unwrap_or_default()
    }

    /// destroy the client with the given id, if it exists
    pub async fn destroy(&mut self, id: CaptureHandle) -> Result<(), CaptureError> {
        let pos = self
            .id_map
            .remove(&id)
            .expect("no position for this handle");

        log::debug!("destroying capture {id} @ {pos}");
        self.spans.remove(&id);
        self.active.retain(|&i| i != id);
        let remaining = self.position_map.get_mut(&pos).expect("id vector");
        remaining.retain(|&i| i != id);

        log::debug!("remaining ids @ {pos}: {remaining:?}");
        if remaining.is_empty() {
            log::debug!("destroying capture @ {pos} - no remaining ids");
            self.position_map.remove(&pos);
            self.capture.destroy(pos).await?;
        } else {
            self.update_spans(pos);
        }
        Ok(())
    }

    /// release mouse
    pub async fn release(&mut self) -> Result<(), CaptureError> {
        self.pressed_keys.clear();
        self.capture.release().await
    }

    /// Release the mouse and put the pointer at `fraction` along the edge of
    /// the capture, where the pointer left the other device (see
    /// [`input_event::edge`]). Backends that can not place the pointer
    /// release it like [`InputCapture::release`].
    pub async fn release_to(&mut self, fraction: f64) -> Result<(), CaptureError> {
        self.pressed_keys.clear();
        self.capture.release_to(fraction).await
    }

    /// Starts capturing for the client `id` as if the pointer crossed its
    /// edge where it is now (see [`Capture::enter`]), wherever its span is.
    pub async fn enter_client(&mut self, id: CaptureHandle) -> Result<(), CaptureError> {
        let Some(&pos) = self.id_map.get(&id) else {
            return Ok(());
        };
        self.entering = Some(id);
        self.capture.enter(pos).await
    }

    /// Where along its edge the pointer left this device when the current
    /// capture began, from 0.0 (top or left end of the desktop) to 1.0.
    /// `None` if the backend does not know.
    pub fn entry_fraction(&self) -> Option<f64> {
        self.capture.entry_fraction()
    }

    /// Drain and return every key the capture has forwarded as
    /// down-but-not-up. The caller is expected to synthesize key-up
    /// events to the remote peer for each — otherwise the peer
    /// retains phantom-held keys after capture is released. The
    /// canonical case is the release-bind chord
    /// (Ctrl+Shift+Alt+Meta): the down events were sent while
    /// capture was active, but the matching up events arrive after
    /// the local tap has flipped to passthrough and never reach
    /// the peer.
    pub fn take_pressed_keys(&mut self) -> HashSet<scancode::Linux> {
        std::mem::take(&mut self.pressed_keys)
    }

    /// destroy the input capture
    pub async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.capture.terminate().await
    }

    /// creates a new [`InputCapture`]
    pub async fn new(backend: Option<Backend>) -> Result<Self, CaptureCreationError> {
        let capture = create(backend).await?;
        Ok(Self {
            capture,
            id_map: Default::default(),
            spans: Default::default(),
            active: Vec::new(),
            entering: None,
            pending: Default::default(),
            position_map: Default::default(),
            pressed_keys: HashSet::new(),
        })
    }

    /// check whether the given keys are pressed
    pub fn keys_pressed(&self, keys: &[scancode::Linux]) -> bool {
        keys.iter().all(|k| self.pressed_keys.contains(k))
    }

    fn update_pressed_keys(&mut self, key: u32, state: u8) {
        if let Ok(scancode) = scancode::Linux::try_from(key) {
            log::debug!("key: {key}, state: {state}, scancode: {scancode:?}");
            match state {
                1 => self.pressed_keys.insert(scancode),
                _ => self.pressed_keys.remove(&scancode),
            };
        }
    }

    /// The clients a capture beginning at `pos` goes to: the one asked for by
    /// [`InputCapture::enter_client`], else those whose span the pointer
    /// crossed, all of them when the backend does not say where.
    fn route(&mut self, pos: Position, ids: &[CaptureHandle]) -> Vec<CaptureHandle> {
        if let Some(id) = self.entering.take().filter(|id| ids.contains(id)) {
            return vec![id];
        }
        match self.capture.entry_fraction() {
            Some(fraction) => {
                let chosen: Vec<CaptureHandle> = ids
                    .iter()
                    .copied()
                    .filter(|&id| self.span(id).contains(fraction))
                    .collect();
                if chosen.is_empty() {
                    log::debug!("capture at {pos}, {fraction:.3} along the edge: no client there");
                }
                chosen
            }
            None => ids.to_vec(),
        }
    }
}

impl Stream for InputCapture {
    type Item = Result<(CaptureHandle, CaptureEvent), CaptureError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if let Some(e) = self.pending.pop_front() {
            return Poll::Ready(Some(Ok(e)));
        }

        // ready
        let event = ready!(self.capture.poll_next_unpin(cx));

        // stream closed
        let event = match event {
            Some(e) => e,
            None => return Poll::Ready(None),
        };

        // error occurred
        let (pos, event) = match event {
            Ok(e) => e,
            Err(e) => return Poll::Ready(Some(Err(e))),
        };

        // handle key presses
        if let CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key { key, state, .. })) = event {
            self.update_pressed_keys(key, state);
        }

        let ids = self.position_map.get(&pos).cloned().unwrap_or_default();
        let Some(&first) = ids.first() else {
            // no client at that edge (any more): look at the next event
            cx.waker().wake_by_ref();
            return Poll::Pending;
        };

        let targets = match event {
            CaptureEvent::Begin => {
                let chosen = self.route(pos, &ids);
                self.active.clone_from(&chosen);
                if chosen.is_empty() {
                    return Poll::Ready(Some(Ok((first, CaptureEvent::Outside))));
                }
                chosen
            }
            _ => {
                let active: Vec<CaptureHandle> = self
                    .active
                    .iter()
                    .copied()
                    .filter(|id| ids.contains(id))
                    .collect();
                if active.is_empty() { ids } else { active }
            }
        };
        for &id in &targets[1..] {
            self.pending.push_back((id, event));
        }
        Poll::Ready(Some(Ok((targets[0], event))))
    }
}

#[async_trait]
trait Capture: Stream<Item = Result<(Position, CaptureEvent), CaptureError>> + Unpin {
    /// create a new client with the given id
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError>;

    /// destroy the client with the given id, if it exists
    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError>;

    /// The parts of the edge at `pos` that lead to a client, all of it when
    /// `spans` is empty: crossing elsewhere must not begin a capture.
    /// Backends that can not keep to them begin anyway, and
    /// [`InputCapture`] then reports [`CaptureEvent::Outside`].
    fn set_spans(&mut self, _pos: Position, _spans: Vec<Span>) {}

    /// release mouse
    async fn release(&mut self) -> Result<(), CaptureError>;

    /// release mouse, putting the pointer at `fraction` along the edge of the
    /// capture; backends that can not place the pointer just release it
    async fn release_to(&mut self, fraction: f64) -> Result<(), CaptureError>;

    /// Starts capturing for the client at `pos` without the pointer
    /// crossing the edge, entering it at the height (or width) of the pointer.
    /// Backends that can not do it log that and do nothing.
    async fn enter(&mut self, pos: Position) -> Result<(), CaptureError>;

    /// where along its edge the pointer was when the current capture began
    fn entry_fraction(&self) -> Option<f64> {
        None
    }

    /// destroy the input capture
    async fn terminate(&mut self) -> Result<(), CaptureError>;
}

async fn create_backend(
    backend: Backend,
) -> Result<
    Box<dyn Capture<Item = Result<(Position, CaptureEvent), CaptureError>>>,
    CaptureCreationError,
> {
    match backend {
        #[cfg(libei)]
        Backend::InputCapturePortal => Ok(Box::new(libei::LibeiInputCapture::new().await?)),
        #[cfg(layer_shell)]
        Backend::LayerShell => Ok(Box::new(layer_shell::LayerShellInputCapture::new()?)),
        #[cfg(x11)]
        Backend::X11 => Ok(Box::new(x11::X11InputCapture::new()?)),
        #[cfg(windows)]
        Backend::Windows => Ok(Box::new(windows::WindowsInputCapture::new())),
        #[cfg(target_os = "macos")]
        Backend::MacOs => Ok(Box::new(macos::MacOSInputCapture::new().await?)),
        Backend::Dummy => Ok(Box::new(dummy::DummyInputCapture::new())),
    }
}

async fn create(
    backend: Option<Backend>,
) -> Result<
    Box<dyn Capture<Item = Result<(Position, CaptureEvent), CaptureError>>>,
    CaptureCreationError,
> {
    if let Some(backend) = backend {
        let b = create_backend(backend).await;
        if b.is_ok() {
            log::info!("using capture backend: {backend}");
        }
        return b;
    }

    for backend in [
        #[cfg(libei)]
        Backend::InputCapturePortal,
        #[cfg(layer_shell)]
        Backend::LayerShell,
        #[cfg(x11)]
        Backend::X11,
        #[cfg(windows)]
        Backend::Windows,
        #[cfg(target_os = "macos")]
        Backend::MacOs,
    ] {
        match create_backend(backend).await {
            Ok(b) => {
                log::info!("using capture backend: {backend}");
                return Ok(b);
            }
            Err(e) if e.cancelled_by_user() => return Err(e),
            Err(e) => log::warn!("{backend} input capture backend unavailable: {e}"),
        }
    }
    Err(CaptureCreationError::NoAvailableBackend)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context, Poll},
    };

    use futures::StreamExt;

    use super::*;

    /// the spans the backend was told, per edge
    type Told = Arc<Mutex<HashMap<Position, Vec<Span>>>>;

    /// a backend that reports the events of `script`, each capture beginning
    /// at the fraction given with it
    #[derive(Default)]
    struct Script {
        events: VecDeque<(Position, CaptureEvent, Option<f64>)>,
        fraction: Option<f64>,
        spans: Told,
    }

    impl Stream for Script {
        type Item = Result<(Position, CaptureEvent), CaptureError>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.events.pop_front().map(|(pos, event, fraction)| {
                if fraction.is_some() {
                    self.fraction = fraction;
                }
                Ok((pos, event))
            }))
        }
    }

    #[async_trait]
    impl Capture for Script {
        async fn create(&mut self, _pos: Position) -> Result<(), CaptureError> {
            Ok(())
        }
        async fn destroy(&mut self, _pos: Position) -> Result<(), CaptureError> {
            Ok(())
        }
        fn set_spans(&mut self, pos: Position, spans: Vec<Span>) {
            self.spans.lock().unwrap().insert(pos, spans);
        }
        async fn release(&mut self) -> Result<(), CaptureError> {
            Ok(())
        }
        async fn release_to(&mut self, _fraction: f64) -> Result<(), CaptureError> {
            Ok(())
        }
        async fn enter(&mut self, _pos: Position) -> Result<(), CaptureError> {
            Ok(())
        }
        fn entry_fraction(&self) -> Option<f64> {
            self.fraction
        }
        async fn terminate(&mut self) -> Result<(), CaptureError> {
            Ok(())
        }
    }

    fn capture(events: Vec<(Position, CaptureEvent, Option<f64>)>) -> (InputCapture, Told) {
        let spans = Arc::default();
        let script = Script {
            events: events.into(),
            fraction: None,
            spans: Arc::clone(&spans),
        };
        let capture = InputCapture {
            capture: Box::new(script),
            pressed_keys: HashSet::new(),
            position_map: Default::default(),
            id_map: Default::default(),
            spans: Default::default(),
            active: Vec::new(),
            entering: None,
            pending: Default::default(),
        };
        (capture, spans)
    }

    const MOTION: CaptureEvent =
        CaptureEvent::Input(Event::Pointer(input_event::PointerEvent::Motion {
            time: 0,
            dx: 1.0,
            dy: 0.0,
        }));

    async fn collect(capture: &mut InputCapture) -> Vec<(CaptureHandle, CaptureEvent)> {
        let mut seen = Vec::new();
        while let Some(event) = capture.next().await {
            seen.push(event.expect("event"));
        }
        seen
    }

    #[tokio::test]
    async fn the_part_of_the_edge_picks_the_client() {
        let (mut capture, spans) = capture(vec![
            (Position::Right, CaptureEvent::Begin, Some(0.75)),
            (Position::Right, MOTION, None),
            (Position::Right, CaptureEvent::Begin, Some(0.25)),
            (Position::Right, MOTION, None),
        ]);
        capture
            .create(1, Position::Right, Span::new(0.0, 0.5))
            .await
            .unwrap();
        capture
            .create(2, Position::Right, Span::new(0.5, 1.0))
            .await
            .unwrap();
        // the backend knows where the edge leads somewhere
        assert_eq!(
            spans.lock().unwrap()[&Position::Right],
            vec![Span::new(0.0, 0.5), Span::new(0.5, 1.0)]
        );
        assert_eq!(
            collect(&mut capture).await,
            vec![
                (2, CaptureEvent::Begin),
                (2, MOTION),
                (1, CaptureEvent::Begin),
                (1, MOTION)
            ]
        );
    }

    #[tokio::test]
    async fn crossing_between_the_parts_is_outside() {
        let (mut capture, _) = capture(vec![(Position::Left, CaptureEvent::Begin, Some(0.5))]);
        capture
            .create(1, Position::Left, Span::new(0.0, 0.4))
            .await
            .unwrap();
        capture
            .create(2, Position::Left, Span::new(0.6, 1.0))
            .await
            .unwrap();
        assert_eq!(
            collect(&mut capture).await,
            vec![(1, CaptureEvent::Outside)]
        );
    }

    #[tokio::test]
    async fn whole_edges_reach_every_client_there() {
        let (mut capture, spans) = capture(vec![
            (Position::Top, CaptureEvent::Begin, Some(0.5)),
            (Position::Top, MOTION, None),
        ]);
        capture.create(1, Position::Top, Span::FULL).await.unwrap();
        capture
            .create(2, Position::Top, Span::new(0.0, 0.2))
            .await
            .unwrap();
        // one client has the whole edge: the backend keeps to nothing
        assert!(spans.lock().unwrap()[&Position::Top].is_empty());
        assert_eq!(
            collect(&mut capture).await,
            vec![(1, CaptureEvent::Begin), (1, MOTION)]
        );
    }

    #[tokio::test]
    async fn entering_a_client_goes_to_it_wherever_the_pointer_is() {
        let (mut capture, _) = capture(vec![(Position::Right, CaptureEvent::Begin, Some(0.1))]);
        capture
            .create(1, Position::Right, Span::new(0.0, 0.5))
            .await
            .unwrap();
        capture
            .create(2, Position::Right, Span::new(0.5, 1.0))
            .await
            .unwrap();
        capture.enter_client(2).await.unwrap();
        assert_eq!(collect(&mut capture).await, vec![(2, CaptureEvent::Begin)]);
    }
}
