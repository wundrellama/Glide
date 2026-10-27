use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use input_capture::{
    CaptureError, CaptureEvent, CaptureHandle, InputCapture, InputCaptureError, Position,
};
use input_event::{Event, KeyboardEvent, scancode};
use lan_mouse_proto::ProtoEvent;
use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::task::{JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

use crate::{connect::LanMouseConnection, idle_release};

pub(crate) struct Capture {
    cancellation_token: CancellationToken,
    request_tx: Sender<CaptureRequest>,
    task: JoinHandle<()>,
    event_rx: Receiver<ICaptureEvent>,
}

pub(crate) enum ICaptureEvent {
    /// A remote pointer is actively being driven by this host's physical input.
    RemoteControlActive,
    /// The outgoing remote pointer handoff ended.
    RemoteControlInactive,
    /// A physical input request released an outgoing handoff.
    LocalOwnershipTaken,
    /// a client was entered; carries the normalized fraction along the
    /// crossed edge when the capture backend knows the crossing point
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
    TakeLocalOwnership,
    /// capture must release the mouse; a fraction requests a geometric
    /// release placing the pointer along the crossed edge
    Release(Option<f64>),
    /// add a capture client
    Create(CaptureHandle, Position, CaptureType),
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
        conn: LanMouseConnection,
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
            enter_fraction: None,
            event_tx,
            idle_release_after: None,
            last_forwarded: Instant::now(),
            request_rx,
            release_bind: Rc::new(RefCell::new(release_bind)),
            state: Default::default(),
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

    pub(crate) fn create(
        &self,
        handle: CaptureHandle,
        pos: lan_mouse_ipc::Position,
        capture_type: CaptureType,
    ) {
        let pos = to_capture_pos(pos);
        self.request_tx
            .send(CaptureRequest::Create(handle, pos, capture_type))
            .expect("channel closed");
    }

    pub(crate) fn destroy(&self, handle: CaptureHandle) {
        self.request_tx
            .send(CaptureRequest::Destroy(handle))
            .expect("channel closed");
    }

    pub(crate) fn release(&self, fraction: Option<f64>) {
        self.request_tx
            .send(CaptureRequest::Release(fraction))
            .expect("channel closed");
    }

    pub(crate) fn take_local_ownership(&self) {
        self.request_tx
            .send(CaptureRequest::TakeLocalOwnership)
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
    captures: Vec<(CaptureHandle, Position, CaptureType)>,
    conn: LanMouseConnection,
    /// edge fraction of the current (unacknowledged) handoff, kept so
    /// repeated `Enter` events while [`State::WaitingForAck`] carry the
    /// same crossing point as the initial one
    enter_fraction: Option<f64>,
    event_tx: Sender<ICaptureEvent>,
    /// release the outgoing capture after this long without forwarded
    /// input, read from the Omarchy idle config when a handoff begins
    idle_release_after: Option<Duration>,
    /// when this host last forwarded physical input to the active client
    last_forwarded: Instant,
    release_bind: Rc<RefCell<Vec<scancode::Linux>>>,
    request_rx: Receiver<CaptureRequest>,
    state: State,
}

impl CaptureTask {
    fn add_capture(&mut self, handle: CaptureHandle, pos: Position, capture_type: CaptureType) {
        self.captures.push((handle, pos, capture_type));
    }

    fn remove_capture(&mut self, handle: CaptureHandle) {
        self.captures.retain(|&(h, ..)| handle != h);
    }

    fn is_default_capture_at(&self, pos: Position) -> bool {
        self.captures
            .iter()
            .any(|&(_, p, t)| p == pos && t == CaptureType::Default)
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
                        CaptureRequest::Create(h, p, t) => self.add_capture(h, p, t),
                        CaptureRequest::Destroy(h) => self.remove_capture(h),
                        CaptureRequest::Release(..) | CaptureRequest::TakeLocalOwnership => { /* nothing to do */ }
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
        for (handle, pos, _type) in captures {
            tokio::select! {
                r = capture.create(handle, pos) => r?,
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
                    // Late replies to a finished handoff must not unlock a
                    // newly received remote pointer or change our idle state.
                    if !reply_is_current(self.active_client, handle) {
                        continue;
                    }

                    match event {
                        // connection acknowlegded => set state to Sending
                        ProtoEvent::Ack(_) => {
                            log::info!("client {handle} acknowledged the connection!");
                            self.state = State::Sending;
                            self.event_tx.send(ICaptureEvent::RemoteControlActive).expect("channel closed");
                        }
                        // client disconnected
                        ProtoEvent::Leave(_, fraction) => {
                            log::info!("releasing capture: left remote client device region");
                            self.release_capture(capture, fraction).await?;
                        },
                        _ => {}
                    }
                },
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    CaptureRequest::Reenable => { /* already active */ },
                    CaptureRequest::Release(fraction) => self.release_capture(capture, fraction).await?,
                    CaptureRequest::TakeLocalOwnership => self.take_local_ownership(capture).await?,
                    CaptureRequest::Create(h, p, t) => {
                        self.add_capture(h, p, t);
                        capture.create(h, p).await?;
                    }
                    CaptureRequest::Destroy(h) => self.destroy_capture(capture, h).await?,
                    CaptureRequest::SetReleaseBind(bind) => {
                        self.release_bind.borrow_mut().clone_from(&bind);
                    }
                },
                _ = idle_deadline(self.active_client, self.idle_release_after, self.last_forwarded) => {
                    self.release_for_idle(capture).await?;
                }
                _ = self.cancellation_token.cancelled() => break,
            }
        }
        Ok(())
    }

    /// The sender's own compositor is about to start its idle screensaver.
    /// Hand keyboard focus back to the desktop so that screensaver can run
    /// and the lock that follows it is not cancelled.
    async fn release_for_idle(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        if idle_release::stay_awake() {
            // Omarchy will not idle; keep the handoff and check again later.
            self.last_forwarded = Instant::now();
            return Ok(());
        }
        log::info!(
            "releasing capture: no input for {:?}, ahead of the Omarchy screensaver",
            self.idle_release_after.unwrap_or_default()
        );
        // non-geometric release: idle has no edge context
        self.release_capture(capture, None).await
    }

    async fn handle_capture_event(
        &mut self,
        capture: &mut InputCapture,
        event: (CaptureHandle, CaptureEvent),
    ) -> Result<(), CaptureError> {
        let (handle, event) = event;
        log::trace!("({handle}): {event:?}");

        /* `InputCapture` fans an event out to every handle sharing a
         * position and queues the extra copies; a `Destroy` processed in
         * the meantime cannot purge that queue, so events for an already
         * retired handle (e.g. an EnterOnly trigger replaced during a
         * rapid re-handoff) can still be delivered here. Drop them:
         * the capture is gone, and the active capture of an unrelated
         * handle must not be released. */
        if !self.captures.iter().any(|&(h, ..)| h == handle) {
            log::debug!("ignoring event for retired capture handle {handle}: {event:?}");
            return Ok(());
        }

        if capture.keys_pressed(&self.release_bind.borrow()) {
            log::info!("releasing capture: release-bind pressed");
            // non-geometric release: the release bind has no edge context
            return self.release_capture(capture, None).await;
        }

        if let CaptureEvent::Begin(fraction) = event {
            self.event_tx
                .send(ICaptureEvent::CaptureBegin(handle, fraction))
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

        self.last_forwarded = Instant::now();

        // activated a new client
        if let CaptureEvent::Begin(fraction) = event {
            if Some(handle) != self.active_client {
                // Read per handoff so edits to the Omarchy idle timeouts apply
                // without restarting Glide.
                self.idle_release_after = idle_release::release_after();
                self.state = State::WaitingForAck;
                self.enter_fraction = fraction;
                self.active_client.replace(handle);
                self.event_tx
                    .send(ICaptureEvent::ClientEntered(handle))
                    .expect("channel closed");
            }
        }

        let event =
            outgoing_proto_event(self.state, self.get_pos(handle), event, self.enter_fraction);

        if let Err(e) = self.conn.send(event, handle).await {
            const DUR: Duration = Duration::from_millis(500);
            debounce!(PREV_LOG, DUR, log::warn!("releasing capture: {e}"));
            capture.release().await?;
        }
        Ok(())
    }

    async fn destroy_capture(
        &mut self,
        capture: &mut InputCapture,
        handle: CaptureHandle,
    ) -> Result<(), CaptureError> {
        /* If the destroyed handle currently owns the capture, end the
         * handoff properly (key-ups + Leave to the peer, local release);
         * an unrelated active capture must not be released. */
        if self.active_client == Some(handle) {
            // non-geometric release: the client is gone, there is no edge to return at
            self.release_capture(capture, None).await?;
        }
        self.remove_capture(handle);
        capture.destroy(handle).await?;
        Ok(())
    }

    async fn release_capture(
        &mut self,
        capture: &mut InputCapture,
        fraction: Option<f64>,
    ) -> Result<(), CaptureError> {
        self.enter_fraction = None;
        // If we have an active client, notify them we're leaving
        if let Some(handle) = self.active_client.take() {
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
            if let Err(e) = self.conn.send(ProtoEvent::Leave(0, fraction), handle).await {
                log::warn!("failed to send Leave to client {handle}: {e}");
            }
        }
        let result = capture.release_at(fraction).await;
        self.event_tx
            .send(ICaptureEvent::RemoteControlInactive)
            .expect("channel closed");
        result
    }

    async fn take_local_ownership(
        &mut self,
        capture: &mut InputCapture,
    ) -> Result<(), CaptureError> {
        // Local motion now explicitly returns this host even when it is the
        // sender. Idle local activity must not keep warping the pointer.
        if self.active_client.is_some() {
            // non-geometric release: local takeover has no edge context
            self.release_capture(capture, None).await?;
            log::info!("Glide: physical input returns outgoing control to this host");
            self.event_tx
                .send(ICaptureEvent::LocalOwnershipTaken)
                .expect("channel closed");
        }
        Ok(())
    }
}

/// Resolves once an outgoing handoff has gone `after` without forwarded
/// input; never resolves while nothing is being sent or the release is off.
async fn idle_deadline(active: Option<CaptureHandle>, after: Option<Duration>, last: Instant) {
    match (active, after) {
        (Some(_), Some(after)) => tokio::time::sleep_until((last + after).into()).await,
        _ => std::future::pending().await,
    }
}

fn reply_is_current(active: Option<CaptureHandle>, handle: CaptureHandle) -> bool {
    active == Some(handle)
}

/// protocol event to send for a capture event; an unacknowledged handoff
/// repeats the `Enter` (with the fraction of the initial crossing) until
/// the peer acknowledges it
fn outgoing_proto_event(
    state: State,
    pos: Position,
    event: CaptureEvent,
    enter_fraction: Option<f64>,
) -> ProtoEvent {
    let opposite_pos = to_proto_pos(pos.opposite());
    match event {
        CaptureEvent::Begin(fraction) => ProtoEvent::Enter(opposite_pos, fraction),
        CaptureEvent::Input(e) => match state {
            State::WaitingForAck => ProtoEvent::Enter(opposite_pos, enter_fraction),
            State::Sending => ProtoEvent::Input(e),
        },
    }
}

#[cfg(test)]
mod handoff_tests {
    use super::reply_is_current;

    #[test]
    fn late_replies_cannot_release_idle_or_another_peer() {
        assert!(!reply_is_current(None, 0));
        assert!(!reply_is_current(Some(1), 0));
        assert!(reply_is_current(Some(0), 0));
    }
}

#[cfg(test)]
mod fraction_tests {
    use super::*;
    use input_event::{Event, PointerEvent};

    fn motion() -> Event {
        Event::Pointer(PointerEvent::Motion {
            time: 0,
            dx: 1.0,
            dy: 1.0,
        })
    }

    fn expect_enter(event: ProtoEvent) -> (lan_mouse_proto::Position, Option<f64>) {
        match event {
            ProtoEvent::Enter(pos, fraction) => (pos, fraction),
            other => panic!("expected Enter, got {other:?}"),
        }
    }

    #[test]
    fn begin_sends_enter_with_the_crossing_fraction() {
        let (pos, fraction) = expect_enter(outgoing_proto_event(
            State::WaitingForAck,
            Position::Right,
            CaptureEvent::Begin(Some(0.25)),
            None,
        ));
        assert!(matches!(pos, lan_mouse_proto::Position::Left)); // opposite side
        assert_eq!(fraction, Some(0.25));
    }

    #[test]
    fn begin_without_geometry_sends_enter_without_fraction() {
        let (_, fraction) = expect_enter(outgoing_proto_event(
            State::WaitingForAck,
            Position::Right,
            CaptureEvent::Begin(None),
            None,
        ));
        assert_eq!(fraction, None);
    }

    #[test]
    fn enter_retries_while_waiting_for_ack_keep_the_initial_fraction() {
        let (_, fraction) = expect_enter(outgoing_proto_event(
            State::WaitingForAck,
            Position::Right,
            CaptureEvent::Input(motion()),
            Some(0.75),
        ));
        assert_eq!(fraction, Some(0.75));
    }

    #[test]
    fn acknowledged_capture_forwards_input_events() {
        match outgoing_proto_event(
            State::Sending,
            Position::Right,
            CaptureEvent::Input(motion()),
            Some(0.75),
        ) {
            ProtoEvent::Input(e) => assert_eq!(e, motion()),
            other => panic!("expected Input, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod stale_capture_tests {
    use super::*;
    use crate::client::ClientManager;
    use input_capture::Backend;
    use std::collections::HashMap;
    use std::net::{IpAddr, Ipv4Addr};
    use tokio::task::LocalSet;
    use webrtc_dtls::crypto::Certificate;

    const DEFAULT_HANDLE: CaptureHandle = 0;
    /// EnterOnly trigger handle for an incoming connection (cf. service.rs
    /// ENTER_HANDLE_BEGIN).
    const ENTER_HANDLE: CaptureHandle = u64::MAX / 2 + 1;

    fn test_task() -> (CaptureTask, Receiver<ICaptureEvent>) {
        let (_request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let cert = Certificate::generate_self_signed(["ignored".to_owned()]).expect("cert");
        let conn = LanMouseConnection::new(
            cert,
            ClientManager::default(),
            HashMap::new(),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        );
        let task = CaptureTask {
            active_client: None,
            backend: None,
            cancellation_token: CancellationToken::new(),
            captures: vec![],
            conn,
            enter_fraction: None,
            event_tx,
            idle_release_after: None,
            last_forwarded: Instant::now(),
            request_rx,
            release_bind: Rc::new(RefCell::new(vec![scancode::Linux::KeyLeftCtrl])),
            state: Default::default(),
        };
        (task, event_rx)
    }

    async fn dummy_capture() -> InputCapture {
        InputCapture::new(Some(Backend::Dummy))
            .await
            .expect("dummy capture backend")
    }

    /// The dummy backend emits `Begin` for `Position::Left`; with both a
    /// Default client and an EnterOnly trigger registered at that position,
    /// `InputCapture` fans the event out and queues the second copy in its
    /// `pending` queue. Destroying the trigger handle cannot purge that
    /// queue, so the retired handle's event is delivered afterwards.
    async fn setup_with_queued_stale_begin(
        task: &mut CaptureTask,
        capture: &mut InputCapture,
    ) -> (CaptureHandle, CaptureEvent) {
        task.add_capture(DEFAULT_HANDLE, Position::Left, CaptureType::Default);
        task.add_capture(ENTER_HANDLE, Position::Left, CaptureType::EnterOnly);
        capture
            .create(DEFAULT_HANDLE, Position::Left)
            .await
            .expect("create default");
        capture
            .create(ENTER_HANDLE, Position::Left)
            .await
            .expect("create enter-only");

        let first = capture.next().await.expect("stream open").expect("event");
        assert_eq!(first, (DEFAULT_HANDLE, CaptureEvent::Begin(None)));
        task.handle_capture_event(capture, first)
            .await
            .expect("begin event");
        assert_eq!(task.active_client, Some(DEFAULT_HANDLE));

        /* retire the trigger handle while its Begin is still queued */
        task.remove_capture(ENTER_HANDLE);
        capture.destroy(ENTER_HANDLE).await.expect("destroy");

        let stale = capture.next().await.expect("stream open").expect("event");
        assert_eq!(stale, (ENTER_HANDLE, CaptureEvent::Begin(None)));
        stale
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queued_event_for_retired_handle_is_ignored() {
        LocalSet::new()
            .run_until(async {
                let (mut task, _event_rx) = test_task();
                let mut capture = dummy_capture().await;
                let stale = setup_with_queued_stale_begin(&mut task, &mut capture).await;

                /* must not panic and must not release the unrelated,
                 * still-registered active capture */
                task.handle_capture_event(&mut capture, stale)
                    .await
                    .expect("stale event must be ignored");
                assert_eq!(task.active_client, Some(DEFAULT_HANDLE));
            })
            .await;
    }

    use futures::FutureExt;

    fn drain_events(event_rx: &mut Receiver<ICaptureEvent>) -> Vec<ICaptureEvent> {
        let mut events = vec![];
        while let Some(Some(event)) = event_rx.recv().now_or_never() {
            events.push(event);
        }
        events
    }

    #[tokio::test(flavor = "current_thread")]
    async fn destroying_inactive_handle_keeps_active_capture() {
        LocalSet::new()
            .run_until(async {
                let (mut task, mut event_rx) = test_task();
                let mut capture = dummy_capture().await;
                let _ = setup_with_queued_stale_begin(&mut task, &mut capture).await;

                /* re-register the trigger handle so it can be destroyed */
                task.add_capture(ENTER_HANDLE, Position::Left, CaptureType::EnterOnly);
                capture
                    .create(ENTER_HANDLE, Position::Left)
                    .await
                    .expect("re-create enter-only");

                task.destroy_capture(&mut capture, ENTER_HANDLE)
                    .await
                    .expect("destroy inactive handle");

                assert_eq!(task.active_client, Some(DEFAULT_HANDLE));
                let events = drain_events(&mut event_rx);
                assert!(
                    !events
                        .iter()
                        .any(|e| matches!(e, ICaptureEvent::RemoteControlInactive)),
                    "destroying an inactive handle must not release the active capture"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn idle_deadline_only_runs_during_an_outgoing_handoff() {
        let long_ago = Instant::now() - Duration::from_secs(60);
        let after = Some(Duration::from_secs(1));
        assert!(
            idle_deadline(None, after, long_ago)
                .now_or_never()
                .is_none()
        );
        assert!(
            idle_deadline(Some(DEFAULT_HANDLE), None, long_ago)
                .now_or_never()
                .is_none()
        );
        assert!(
            idle_deadline(Some(DEFAULT_HANDLE), after, Instant::now())
                .now_or_never()
                .is_none()
        );
        // tokio's sleep_until needs a timer tick even for a past deadline
        tokio::time::timeout(
            Duration::from_secs(1),
            idle_deadline(Some(DEFAULT_HANDLE), after, long_ago),
        )
        .await
        .expect("an expired idle deadline resolves");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn idle_release_ends_the_outgoing_handoff() {
        LocalSet::new()
            .run_until(async {
                let (mut task, mut event_rx) = test_task();
                let mut capture = dummy_capture().await;
                let _ = setup_with_queued_stale_begin(&mut task, &mut capture).await;
                let _ = drain_events(&mut event_rx);

                task.release_for_idle(&mut capture)
                    .await
                    .expect("idle release");

                if idle_release::stay_awake() {
                    // a developer machine with Stay Awake on keeps the handoff
                    assert_eq!(task.active_client, Some(DEFAULT_HANDLE));
                    return;
                }
                assert_eq!(task.active_client, None);
                assert!(
                    drain_events(&mut event_rx)
                        .iter()
                        .any(|e| matches!(e, ICaptureEvent::RemoteControlInactive)),
                    "idle release must end remote control"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn destroying_active_handle_releases_capture_state() {
        LocalSet::new()
            .run_until(async {
                let (mut task, mut event_rx) = test_task();
                let mut capture = dummy_capture().await;
                let _ = setup_with_queued_stale_begin(&mut task, &mut capture).await;

                task.destroy_capture(&mut capture, DEFAULT_HANDLE)
                    .await
                    .expect("destroy active handle");

                assert_eq!(task.active_client, None);
                let events = drain_events(&mut event_rx);
                assert!(
                    events
                        .iter()
                        .any(|e| matches!(e, ICaptureEvent::RemoteControlInactive)),
                    "destroying the active handle must release the capture"
                );
            })
            .await;
    }
}

thread_local! {
    static PREV_LOG: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    WaitingForAck,
    Sending,
}

fn to_capture_pos(pos: lan_mouse_ipc::Position) -> input_capture::Position {
    match pos {
        lan_mouse_ipc::Position::Left => input_capture::Position::Left,
        lan_mouse_ipc::Position::Right => input_capture::Position::Right,
        lan_mouse_ipc::Position::Top => input_capture::Position::Top,
        lan_mouse_ipc::Position::Bottom => input_capture::Position::Bottom,
    }
}

fn to_proto_pos(pos: input_capture::Position) -> lan_mouse_proto::Position {
    match pos {
        input_capture::Position::Left => lan_mouse_proto::Position::Left,
        input_capture::Position::Right => lan_mouse_proto::Position::Right,
        input_capture::Position::Top => lan_mouse_proto::Position::Top,
        input_capture::Position::Bottom => lan_mouse_proto::Position::Bottom,
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
