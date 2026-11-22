use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use input_capture::{
    CaptureError, CaptureEvent, CaptureHandle, InputCapture, InputCaptureError, Position,
};
use input_event::{
    Event, KeyboardEvent,
    edge::{Edge, EdgeLink},
    scancode,
};
use local_channel::mpsc::{Receiver, Sender, channel};
use rustinput_proto::{CAP_CURSOR_POSITION, CAP_EDGE_SPAN, ProtoEvent};
use tokio::task::{JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

use crate::connect::RustInputConnection;

pub(crate) struct Capture {
    cancellation_token: CancellationToken,
    request_tx: Sender<CaptureRequest>,
    task: JoinHandle<()>,
    event_rx: Receiver<ICaptureEvent>,
}

pub(crate) enum ICaptureEvent {
    /// a client was entered, with where along its edge the pointer left
    /// this device (if known)
    CaptureBegin(CaptureHandle, Option<f64>),
    /// capture disabled
    CaptureDisabled,
    /// capture disabled
    CaptureEnabled,
    /// A (new) client was entered.
    /// In contrast to [`ICaptureEvent::CaptureBegin`] this
    /// event is only triggered when the capture was
    /// explicitly released in the meantime by
    /// either the remote client leaving its device region,
    /// a new device entering the screen or the release bind.
    ClientEntered(u64),
    /// The previously active client was left, i.e. capture
    /// was released for the given handle. Mirrors
    /// [`ICaptureEvent::ClientEntered`] for the leave side
    /// and fires on every release path (release-bind chord,
    /// remote `Leave`, explicit `Release` request, send
    /// failure, or destroy of the active capture).
    ClientLeft(u64),
    /// The client entered confirmed that it is now being controlled,
    /// from here on input events reach it.
    ClientAcknowledged(u64),
    /// The capture is released after another device entered this one: the
    /// pointer can be placed where that device entered.
    PlacePointer(Edge, f64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureType {
    /// a normal input capture
    Default,
    /// A capture only interested in [`CaptureEvent::Begin`] events.
    /// The capture is released immediately, if there is no
    /// Default capture at the same position.
    EnterOnly,
}

#[derive(Clone, Debug)]
enum CaptureRequest {
    /// capture must release the mouse
    Release,
    /// another device entered at `fraction` along `edge`: release the mouse,
    /// then have the pointer placed there
    ReleaseAt(Edge, f64),
    /// go to the client without crossing the edge
    Enter(CaptureHandle),
    /// keep the pointer on this device: crossing an edge does nothing
    Lock(bool),
    /// paused: the pointer comes back and stays here, also on request
    Pause(bool),
    /// add a capture client, reached through the part of the edge in the link
    Create(CaptureHandle, Position, CaptureType, EdgeLink),
    /// where a capture client meets this device changed
    UpdateLink(CaptureHandle, EdgeLink),
    /// destory a capture client
    Destroy(CaptureHandle),
    /// reenable input capture
    Reenable,
    /// set release bind
    SetReleaseBind(Vec<scancode::Linux>),
}

impl Capture {
    pub(crate) fn new(
        backend: Option<input_capture::Backend>,
        conn: RustInputConnection,
        release_bind: Vec<scancode::Linux>,
    ) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let cancellation_token = CancellationToken::new();
        let capture_task = CaptureTask {
            active_client: None,
            backend,
            cancellation_token: cancellation_token.clone(),
            captures: Default::default(),
            conn,
            event_tx,
            request_rx,
            release_bind: Rc::new(RefCell::new(release_bind)),
            state: Default::default(),
            entry_fraction: None,
            locked: false,
            paused: false,
            entering: false,
        };
        let task = spawn_local(capture_task.run());
        Self {
            cancellation_token,
            request_tx,
            task,
            event_rx,
        }
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(CaptureRequest::Reenable)
            .expect("channel closed");
    }

    pub(crate) async fn terminate(&mut self) {
        self.cancellation_token.cancel();
        log::debug!("terminating capture");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }

    /// a capture client at `pos`, reached through `link.own` of that edge
    /// (see [`EdgeLink`])
    pub(crate) fn create(
        &self,
        handle: CaptureHandle,
        pos: rustinput_ipc::Position,
        capture_type: CaptureType,
        link: EdgeLink,
    ) {
        let pos = to_capture_pos(pos);
        self.request_tx
            .send(CaptureRequest::Create(handle, pos, capture_type, link))
            .expect("channel closed");
    }

    /// where the capture client `handle` meets this device changed
    pub(crate) fn update_link(&self, handle: CaptureHandle, link: EdgeLink) {
        self.request_tx
            .send(CaptureRequest::UpdateLink(handle, link))
            .expect("channel closed");
    }

    pub(crate) fn destroy(&self, handle: CaptureHandle) {
        self.request_tx
            .send(CaptureRequest::Destroy(handle))
            .expect("channel closed");
    }

    pub(crate) fn release(&self) {
        self.request_tx
            .send(CaptureRequest::Release)
            .expect("channel closed");
    }

    /// moves the pointer to the client as if it crossed the edge
    pub(crate) fn enter(&self, handle: CaptureHandle) {
        self.request_tx
            .send(CaptureRequest::Enter(handle))
            .expect("channel closed");
    }

    /// keeps the pointer on this device (or lets it go again)
    pub(crate) fn lock(&self, locked: bool) {
        self.request_tx
            .send(CaptureRequest::Lock(locked))
            .expect("channel closed");
    }

    /// Paused, the mouse and keyboard stay on this device: a client being
    /// controlled lets go, crossing an edge does nothing and neither does
    /// going to a client on request.
    pub(crate) fn pause(&self, paused: bool) {
        self.request_tx
            .send(CaptureRequest::Pause(paused))
            .expect("channel closed");
    }

    /// releases the mouse because another device entered at `fraction` along
    /// `edge`, answered by [`ICaptureEvent::PlacePointer`]
    pub(crate) fn release_at(&self, edge: Edge, fraction: f64) {
        self.request_tx
            .send(CaptureRequest::ReleaseAt(edge, fraction))
            .expect("channel closed");
    }

    pub(crate) async fn event(&mut self) -> ICaptureEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    pub(crate) fn set_release_bind(&mut self, bind: Vec<scancode::Linux>) {
        let _ = self.request_tx.send(CaptureRequest::SetReleaseBind(bind));
    }
}

/// debounce a statement `$st`, i.e. the statement is executed only if the
/// time since the previous execution is at least `$dur`.
/// `$prev` is used to keep track of this timestamp
macro_rules! debounce {
    ($prev:ident, $dur:expr, $st:stmt) => {
        let exec = match $prev.get() {
            None => true,
            Some(instant) if instant.elapsed() > $dur => true,
            _ => false,
        };
        if exec {
            $prev.replace(Some(Instant::now()));
            $st
        }
    };
}

struct CaptureTask {
    active_client: Option<CaptureHandle>,
    backend: Option<input_capture::Backend>,
    cancellation_token: CancellationToken,
    captures: Vec<(CaptureHandle, Position, CaptureType, EdgeLink)>,
    conn: RustInputConnection,
    event_tx: Sender<ICaptureEvent>,
    release_bind: Rc<RefCell<Vec<scancode::Linux>>>,
    request_rx: Receiver<CaptureRequest>,
    state: State,
    /// where along its edge the pointer left this device for the active client
    entry_fraction: Option<f64>,
    /// the pointer stays on this device
    locked: bool,
    /// paused by the user: the pointer stays on this device, also on request
    paused: bool,
    /// a client is entered on request, which goes through when locked
    entering: bool,
}

impl CaptureTask {
    fn add_capture(
        &mut self,
        handle: CaptureHandle,
        pos: Position,
        capture_type: CaptureType,
        link: EdgeLink,
    ) {
        self.captures.push((handle, pos, capture_type, link));
    }

    fn set_link(&mut self, handle: CaptureHandle, link: EdgeLink) {
        if let Some(capture) = self.captures.iter_mut().find(|(h, ..)| *h == handle) {
            capture.3 = link;
        }
    }

    fn get_link(&self, handle: CaptureHandle) -> EdgeLink {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .map(|c| c.3)
            .unwrap_or_default()
    }

    fn remove_capture(&mut self, handle: CaptureHandle) {
        self.captures.retain(|&(h, ..)| handle != h);
    }

    fn is_default_capture_at(&self, pos: Position) -> bool {
        self.captures
            .iter()
            .any(|&(_, p, t, _)| p == pos && t == CaptureType::Default)
    }

    fn get_pos(&self, handle: CaptureHandle) -> Position {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .expect("no such capture")
            .1
    }

    fn get_type(&self, handle: CaptureHandle) -> CaptureType {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .expect("no such capture")
            .2
    }

    async fn run(mut self) {
        loop {
            if let Err(e) = self.do_capture().await {
                log::warn!("input capture exited: {e}");
            }
            loop {
                tokio::select! {
                    r = self.request_rx.recv() => match r.expect("channel closed") {
                        CaptureRequest::Reenable => break,
                        CaptureRequest::Create(h, p, t, l) => self.add_capture(h, p, t, l),
                        CaptureRequest::UpdateLink(h, l) => self.set_link(h, l),
                        CaptureRequest::Destroy(h) => self.remove_capture(h),
                        CaptureRequest::Release => { /* nothing to do */ }
                        CaptureRequest::ReleaseAt(edge, fraction) => {
                            // nothing is grabbed
                            self.place_pointer(edge, fraction);
                        }
                        CaptureRequest::Enter(_) => { /* no capture to enter with */ }
                        CaptureRequest::Lock(locked) => self.locked = locked,
                        CaptureRequest::Pause(paused) => self.paused = paused,
                        CaptureRequest::SetReleaseBind(bind) => {
                            self.release_bind.borrow_mut().clone_from(&bind);
                        }
                    },
                    _ = self.cancellation_token.cancelled() => return,
                }
            }
        }
    }

    async fn do_capture(&mut self) -> Result<(), InputCaptureError> {
        /* allow cancelling capture request */
        let mut capture = tokio::select! {
            r = InputCapture::new(self.backend) => r?,
            _ = self.cancellation_token.cancelled() => return Ok(()),
        };

        let _capture_guard = DropGuard::new(
            self.event_tx.clone(),
            ICaptureEvent::CaptureEnabled,
            ICaptureEvent::CaptureDisabled,
        );

        /* create barriers for active clients */
        let r = self.create_captures(&mut capture).await;
        if let Err(e) = r {
            capture.terminate().await?;
            return Err(e.into());
        }

        let r = self.do_capture_session(&mut capture).await;

        // FIXME replace with async drop when stabilized
        capture.terminate().await?;

        r
    }

    async fn create_captures(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        let captures = self.captures.clone();
        for (handle, pos, _type, link) in captures {
            tokio::select! {
                r = capture.create(handle, pos, link.own) => r?,
                _ = self.cancellation_token.cancelled() => return Ok(()),
            }
        }
        Ok(())
    }

    async fn do_capture_session(
        &mut self,
        capture: &mut InputCapture,
    ) -> Result<(), InputCaptureError> {
        loop {
            tokio::select! {
                event = capture.next() => match event {
                    Some(event) => self.handle_capture_event(capture, event?).await?,
                    None => return Ok(()),
                },
                (handle, event) = self.conn.recv() => {
                    if let Some(active) = self.active_client {
                        if handle != active {
                            // we only care about events coming from the client we are currently connected to
                            // only `Ack` and `Leave` are relevant
                            continue
                        }
                    }

                    match event {
                        // connection acknowlegded => set state to Sending
                        ProtoEvent::Ack(_) => {
                            log::info!("client {handle} acknowledged the connection!");
                            if self.state != State::Sending && self.active_client == Some(handle) {
                                self.event_tx
                                    .send(ICaptureEvent::ClientAcknowledged(handle))
                                    .expect("channel closed");
                            }
                            self.state = State::Sending;
                        }
                        // client disconnected
                        ProtoEvent::Leave(_) => {
                            log::info!("releasing capture: left remote client device region");
                            self.release_capture(capture).await?;
                        },
                        // the same, telling where the pointer left the client,
                        // along its edge: the same place along this one
                        ProtoEvent::LeaveAt(fraction) => {
                            let here = self.get_link(handle).to_own(fraction);
                            log::info!("releasing capture: left remote client device region at {fraction:.3} ({here:.3} here)");
                            self.release_capture_to(capture, Some(here)).await?;
                        },
                        _ => {}
                    }
                },
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    CaptureRequest::Reenable => { /* already active */ },
                    CaptureRequest::Release => self.release_capture(capture).await?,
                    CaptureRequest::ReleaseAt(edge, fraction) => {
                        self.release_capture_at(capture, edge, fraction).await?
                    }
                    CaptureRequest::Enter(handle) => {
                        let default = self
                            .captures
                            .iter()
                            .any(|(h, _, t, _)| *h == handle && *t == CaptureType::Default);
                        if default && self.active_client.is_none() && !self.paused {
                            // an explicit request goes through even when locked
                            self.entering = true;
                            capture.enter_client(handle).await?;
                        }
                    }
                    CaptureRequest::Lock(locked) => {
                        log::info!("pointer {}", if locked { "kept on this device" } else { "free to move on" });
                        self.locked = locked;
                    }
                    CaptureRequest::Pause(paused) => {
                        log::info!("{}", if paused { "paused" } else { "going on" });
                        self.paused = paused;
                        if paused && self.active_client.is_some() {
                            log::info!("releasing capture: paused");
                            self.release_capture(capture).await?;
                        }
                    }
                    CaptureRequest::Create(h, p, t, l) => {
                        self.add_capture(h, p, t, l);
                        capture.create(h, p, l.own).await?;
                    }
                    CaptureRequest::UpdateLink(h, l) => {
                        self.set_link(h, l);
                        capture.set_span(h, l.own);
                    }
                    CaptureRequest::Destroy(h) => {
                        // If the capture we're tearing down is the
                        // currently-active one, treat this as a
                        // release for hook purposes. The release_capture
                        // path also clears active_client and flushes
                        // pressed-key state to the peer; without this,
                        // `cli deactivate` (or a hostname change
                        // re-creating the client) would skip leave_hook.
                        if self.active_client == Some(h) {
                            self.release_capture(capture).await?;
                        }
                        self.remove_capture(h);
                        capture.destroy(h).await?;
                    }
                    CaptureRequest::SetReleaseBind(bind) => {
                        self.release_bind.borrow_mut().clone_from(&bind);
                    }
                },
                _ = self.cancellation_token.cancelled() => break,
            }
        }
        Ok(())
    }

    async fn handle_capture_event(
        &mut self,
        capture: &mut InputCapture,
        event: (CaptureHandle, CaptureEvent),
    ) -> Result<(), CaptureError> {
        let (handle, event) = event;
        log::trace!("({handle}): {event:?}");

        // the backend lost hold of the input (see `CaptureEvent::Released`):
        // the other device lets go of its keys and the pointer
        if event == CaptureEvent::Released {
            if self.active_client.is_some() {
                log::info!("releasing capture: the input reaches this device again");
                self.release_capture(capture).await?;
            }
            return Ok(());
        }

        // crossed where no client is: the edge is a wall there
        if event == CaptureEvent::Outside {
            const DUR: Duration = Duration::from_secs(5);
            debounce!(
                PREV_OUTSIDE_LOG,
                DUR,
                log::debug!("no client on this part of the edge, releasing")
            );
            self.entering = false;
            return capture.release().await;
        }

        if capture.keys_pressed(&self.release_bind.borrow()) {
            log::info!("releasing capture: release-bind pressed");
            return self.release_capture(capture).await;
        }

        if event == CaptureEvent::Begin && self.locked && !std::mem::take(&mut self.entering) {
            // kept on this device: the edge is a wall
            return capture.release().await;
        }
        if event == CaptureEvent::Begin && self.paused {
            self.entering = false;
            return capture.release().await;
        }
        // a client that answers but takes no input (paused there, or
        // without input emulation): the edge is a wall, rather than the
        // pointer going there and coming back at once
        if event == CaptureEvent::Begin
            && self.active_client != Some(handle)
            && self.get_type(handle) == CaptureType::Default
            && self.conn.refuses(handle)
        {
            self.entering = false;
            const DUR: Duration = Duration::from_secs(5);
            debounce!(
                PREV_REFUSED_LOG,
                DUR,
                log::info!(
                    "client {handle} takes no input now (paused there?), the edge is a wall"
                )
            );
            return capture.release().await;
        }
        if event == CaptureEvent::Begin {
            self.entering = false;
            self.entry_fraction = capture.entry_fraction();
            self.event_tx
                .send(ICaptureEvent::CaptureBegin(handle, self.entry_fraction))
                .expect("channel closed");
        }

        // enter only capture (for incoming connections)
        if self.get_type(handle) == CaptureType::EnterOnly {
            // if there is no active outgoing connection at the current capture,
            // we release the capture
            if !self.is_default_capture_at(self.get_pos(handle)) {
                log::info!("releasing capture: no active client at this position");
                capture.release().await?;
            }
            // we dont care about events from incoming handles except for releasing the capture
            return Ok(());
        }

        // activated a new client
        if event == CaptureEvent::Begin && Some(handle) != self.active_client {
            self.state = State::WaitingForAck;
            self.active_client.replace(handle);
            self.event_tx
                .send(ICaptureEvent::ClientEntered(handle))
                .expect("channel closed");
        }

        let opposite_pos = to_proto_pos(self.get_pos(handle).opposite());
        // tell clients that can place their pointer where it left this
        // device, the same place along their part of the edge, and which
        // part of their edge leads back here
        let link = self.get_link(handle);
        let capabilities = self.conn.peer_capabilities(handle);
        let enter = match self.entry_fraction.map(|f| link.to_peer(f)) {
            Some(at) if capabilities & CAP_EDGE_SPAN != 0 => ProtoEvent::EnterSpan {
                pos: opposite_pos,
                at,
                lo: link.peer.lo as f32,
                hi: link.peer.hi as f32,
            },
            Some(at) if capabilities & CAP_CURSOR_POSITION != 0 => {
                ProtoEvent::EnterAt(opposite_pos, at)
            }
            _ => ProtoEvent::Enter(opposite_pos),
        };

        let event = match event {
            CaptureEvent::Begin => enter,
            CaptureEvent::Input(e) => match self.state {
                // connection not acknowledged, repeat `Enter` event
                State::WaitingForAck => enter,
                State::Sending => ProtoEvent::Input(e),
            },
            // handled above
            CaptureEvent::Released | CaptureEvent::Outside => return Ok(()),
        };

        if let Err(e) = self.conn.send(event, handle).await {
            const DUR: Duration = Duration::from_millis(500);
            debounce!(PREV_LOG, DUR, log::warn!("releasing capture: {e}"));
            // Funnel through release_capture so the leave_hook
            // fires and active_client is cleared (without this the
            // active_client field would stay stale until the next
            // Begin from a different handle).
            self.release_capture(capture).await?;
        }
        Ok(())
    }

    async fn release_capture(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        self.release_capture_to(capture, None).await
    }

    /// Releases the capture because another device entered this one at
    /// `fraction` along `edge`, then has the pointer placed there.
    ///
    /// The capture puts the pointer there itself when that is the edge it
    /// captured at, so it does not show up at the old place for a moment.
    /// Placing it again afterwards does no harm and covers backends that
    /// can not place the pointer.
    async fn release_capture_at(
        &mut self,
        capture: &mut InputCapture,
        edge: Edge,
        fraction: f64,
    ) -> Result<(), CaptureError> {
        match self.active_client {
            Some(handle) if Edge::from(self.get_pos(handle)) == edge => {
                self.release_capture_to(capture, Some(fraction)).await?
            }
            Some(_) => self.release_capture(capture).await?,
            None => {}
        }
        self.place_pointer(edge, fraction);
        Ok(())
    }

    fn place_pointer(&self, edge: Edge, fraction: f64) {
        self.event_tx
            .send(ICaptureEvent::PlacePointer(edge, fraction))
            .expect("channel closed");
    }

    /// releases the capture, putting the pointer at `fraction` along the edge
    /// of the capture if given (where it left the client) and back where it
    /// left this device otherwise
    async fn release_capture_to(
        &mut self,
        capture: &mut InputCapture,
        fraction: Option<f64>,
    ) -> Result<(), CaptureError> {
        self.entry_fraction = None;
        // If we have an active client, notify them we're leaving
        if let Some(handle) = self.active_client.take() {
            // Surface the leave to the service layer so it can fire
            // the per-client leave_hook. Sent before the network
            // teardown below so we never race against the peer
            // disappearing.
            self.event_tx
                .send(ICaptureEvent::ClientLeft(handle))
                .expect("channel closed");
            // Synthesize key-up events for every key still held in the
            // capture's pressed_keys set BEFORE sending Leave. Without
            // this, pressing the release-bind chord (typically all four
            // modifiers) leaves the peer with phantom held modifiers:
            // the down events were forwarded while capture was active,
            // but the matching up events arrive after the local tap
            // flips to passthrough and never reach the peer. The peer
            // then runs every subsequent keystroke through those held
            // mods until its watchdog times out (1+ s) or our Leave
            // arrives — and Leave can be lost over UDP/DTLS.
            for key in capture.take_pressed_keys() {
                let key_up = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: key as u32,
                    state: 0,
                }));
                if let Err(e) = self.conn.send(key_up, handle).await {
                    log::warn!("failed to send key-up to client {handle}: {e}");
                }
            }
            // Reset the modifier mask too. The peer's input-emulation
            // layer keeps a separate XKB-style modifier state that's
            // updated by KeyboardEvent::Modifiers, distinct from the
            // pressed_keys set drained above. Without this, an
            // already-locked CapsLock would survive the release.
            let mods_zero = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            }));
            if let Err(e) = self.conn.send(mods_zero, handle).await {
                log::warn!("failed to reset modifiers on client {handle}: {e}");
            }

            log::info!("sending Leave event to client {handle}");
            if let Err(e) = self.conn.send(ProtoEvent::Leave(0), handle).await {
                log::warn!("failed to send Leave to client {handle}: {e}");
            }
        }
        match fraction {
            Some(fraction) => capture.release_to(fraction).await,
            None => capture.release().await,
        }
    }
}

thread_local! {
    static PREV_LOG: Cell<Option<Instant>> = const { Cell::new(None) };
    static PREV_OUTSIDE_LOG: Cell<Option<Instant>> = const { Cell::new(None) };
    static PREV_REFUSED_LOG: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    WaitingForAck,
    Sending,
}

fn to_capture_pos(pos: rustinput_ipc::Position) -> input_capture::Position {
    match pos {
        rustinput_ipc::Position::Left => input_capture::Position::Left,
        rustinput_ipc::Position::Right => input_capture::Position::Right,
        rustinput_ipc::Position::Top => input_capture::Position::Top,
        rustinput_ipc::Position::Bottom => input_capture::Position::Bottom,
    }
}

fn to_proto_pos(pos: input_capture::Position) -> rustinput_proto::Position {
    match pos {
        input_capture::Position::Left => rustinput_proto::Position::Left,
        input_capture::Position::Right => rustinput_proto::Position::Right,
        input_capture::Position::Top => rustinput_proto::Position::Top,
        input_capture::Position::Bottom => rustinput_proto::Position::Bottom,
    }
}

struct DropGuard<T> {
    tx: Sender<T>,
    on_drop: Option<T>,
}

impl<T> DropGuard<T> {
    fn new(tx: Sender<T>, on_new: T, on_drop: T) -> Self {
        tx.send(on_new).expect("channel closed");
        let on_drop = Some(on_drop);
        Self { tx, on_drop }
    }
}

impl<T> Drop for DropGuard<T> {
    fn drop(&mut self) {
        self.tx
            .send(self.on_drop.take().expect("item"))
            .expect("channel closed");
    }
}
