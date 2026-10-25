use crate::listen::{LanMouseListener, ListenEvent, ListenerCreationError};
use futures::StreamExt;
use input_emulation::{EmulationHandle, InputEmulation, InputEmulationError};
use input_event::Event;
use lan_mouse_proto::{Position, ProtoEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    net::SocketAddr,
    rc::Rc,
    time::{Duration, Instant},
};
use tokio::{
    select,
    task::{JoinHandle, spawn_local},
};

/// emulation handling events received from a listener
pub(crate) struct Emulation {
    task: JoinHandle<()>,
    request_tx: Sender<EmulationRequest>,
    event_rx: Receiver<EmulationEvent>,
}

pub(crate) enum EmulationEvent {
    RemoteControlActive,
    IncomingControlInactive,
    LocalOwnershipTaken,
    Connected {
        addr: SocketAddr,
        fingerprint: String,
    },
    ConnectionAttempt {
        fingerprint: String,
    },
    /// new connection
    Entered {
        /// address of the connection
        addr: SocketAddr,
        /// position of the connection
        pos: lan_mouse_ipc::Position,
        /// certificate fingerprint of the connection
        fingerprint: String,
    },
    /// connection closed
    Disconnected {
        addr: SocketAddr,
    },
    /// the port of the listener has changed
    PortChanged(Result<u16, ListenerCreationError>),
    /// emulation was disabled
    EmulationDisabled,
    /// emulation was enabled
    EmulationEnabled,
    /// capture should be released; a fraction requests a geometric
    /// release placing the pointer along the crossed edge
    ReleaseNotify(Option<f64>),
}

enum EmulationRequest {
    Reenable,
    TakeLocalOwnership,
    Release(SocketAddr, Option<f64>),
    ChangePort(u16),
    Terminate,
}

impl Emulation {
    pub(crate) fn new(
        backend: Option<input_emulation::Backend>,
        listener: LanMouseListener,
        outgoing_capture_active: Rc<Cell<bool>>,
    ) -> Self {
        let emulation_proxy = EmulationProxy::new(backend);
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let emulation_task = ListenTask {
            listener,
            emulation_proxy,
            outgoing_capture_active,
            request_rx,
            event_tx,
        };
        let task = spawn_local(emulation_task.run());
        Self {
            task,
            request_tx,
            event_rx,
        }
    }

    pub(crate) fn send_leave_event(&self, addr: SocketAddr, fraction: Option<f64>) {
        self.request_tx
            .send(EmulationRequest::Release(addr, fraction))
            .expect("channel closed");
    }

    pub(crate) fn take_local_ownership(&self) {
        self.request_tx
            .send(EmulationRequest::TakeLocalOwnership)
            .expect("channel closed");
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(EmulationRequest::Reenable)
            .expect("channel closed");
    }

    pub(crate) fn request_port_change(&self, port: u16) {
        self.request_tx
            .send(EmulationRequest::ChangePort(port))
            .expect("channel closed")
    }

    pub(crate) async fn event(&mut self) -> EmulationEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    /// wait for termination
    pub(crate) async fn terminate(&mut self) {
        log::debug!("terminating emulation");
        self.request_tx
            .send(EmulationRequest::Terminate)
            .expect("channel closed");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }
}

struct ListenTask {
    listener: LanMouseListener,
    emulation_proxy: EmulationProxy,
    /// true while this host's pointer is captured by a peer (outgoing
    /// handoff); shared with the service, which learns the state from
    /// capture events
    outgoing_capture_active: Rc<Cell<bool>>,
    request_rx: Receiver<EmulationRequest>,
    event_tx: Sender<EmulationEvent>,
}

impl ListenTask {
    async fn run(mut self) {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        let mut last_response = HashMap::new();
        let mut rejected_connections = HashMap::new();
        let mut ownership = PointerOwnership::default();
        loop {
            select! {
                e = self.listener.next() => {match e {
                    Some(ListenEvent::Msg { event, addr }) => {
                        log::trace!("{event} <-<-<-<-<- {addr}");
                        last_response.insert(addr, Instant::now());
                        match event {
                            ProtoEvent::Enter(pos, fraction) => {
                                if let Some(fingerprint) = self.listener.get_certificate_fingerprint(addr).await {
                                    let disposition = ownership.enter(addr);
                                    if should_warp_on_enter(&disposition, fraction, self.outgoing_capture_active.get()) {
                                        // place the pointer at the crossing point before any
                                        // subsequent input events of this handoff are consumed
                                        let (x, y) = entry_coordinates(pos, fraction.expect("warp checked"));
                                        self.emulation_proxy.warp(addr, x, y);
                                    }
                                    match disposition {
                                        EnterDisposition::New => {
                                            log::info!("releasing capture: {addr} entered this device");
                                            self.event_tx.send(EmulationEvent::ReleaseNotify(fraction)).expect("channel closed");
                                            self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                                            self.event_tx.send(EmulationEvent::Entered{addr, pos: to_ipc_pos(pos), fingerprint}).expect("channel closed");
                                            self.event_tx.send(EmulationEvent::RemoteControlActive).expect("channel closed");
                                        }
                                        EnterDisposition::Repeat => self.listener.reply(addr, ProtoEvent::Ack(0)).await,
                                        EnterDisposition::Revoked => self.listener.reply(addr, ProtoEvent::Leave(0, None)).await,
                                    }
                                }
                            }
                            ProtoEvent::Leave(..) => {
                                ownership.leave(addr);
                                self.emulation_proxy.remove(addr);
                                self.event_tx.send(EmulationEvent::IncomingControlInactive).expect("channel closed");
                                self.event_tx.send(EmulationEvent::Disconnected { addr }).expect("channel closed");
                                self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                            }
                            ProtoEvent::Input(event) => {
                                if ownership.accepts(addr) {
                                    self.emulation_proxy.consume(event, addr);
                                } else {
                                    // Repeat Leave if a datagram was lost; never replay stale keys.
                                    self.listener.reply(addr, ProtoEvent::Leave(0, None)).await;
                                }
                            },
                            ProtoEvent::Ping => self.listener.reply(addr, ProtoEvent::Pong(self.emulation_proxy.emulation_active.get())).await,
                            _ => {}
                        }
                    }
                    Some(ListenEvent::Accept { addr, fingerprint }) => {
                        self.event_tx.send(EmulationEvent::Connected { addr, fingerprint }).expect("channel closed");
                    }
                    Some(ListenEvent::Rejected { fingerprint }) => {
                        if rejected_connections.insert(fingerprint.clone(), Instant::now())
                            .is_none_or(|i| i.elapsed() >= Duration::from_secs(2)) {
                                self.event_tx.send(EmulationEvent::ConnectionAttempt { fingerprint }).expect("channel closed");
                            }
                    }
                    None => break
                }}
                event = self.emulation_proxy.event() => {
                    self.event_tx.send(event).expect("channel closed");
                }
                request = self.request_rx.recv() => match request.expect("channel closed") {
                    // reenable emulation
                    EmulationRequest::Reenable => self.emulation_proxy.reenable(),
                    // notify the other end that we hit a barrier (should release capture)
                    EmulationRequest::Release(addr, fraction) => {
                        ownership.leave(addr);
                        self.emulation_proxy.remove(addr);
                        self.event_tx.send(EmulationEvent::IncomingControlInactive).expect("channel closed");
                        self.event_tx.send(EmulationEvent::Disconnected { addr }).expect("channel closed");
                        self.listener.reply(addr, ProtoEvent::Leave(0, fraction)).await;
                    },
                    EmulationRequest::TakeLocalOwnership => {
                        let revoked = ownership.take_local();
                        let took_local = !revoked.is_empty();
                        for addr in revoked {
                            log::info!("Glide: physical input takes local ownership from {addr}");
                            self.emulation_proxy.remove(addr);
                            self.event_tx.send(EmulationEvent::IncomingControlInactive).expect("channel closed");
                            self.event_tx.send(EmulationEvent::Disconnected { addr }).expect("channel closed");
                            self.listener.reply(addr, ProtoEvent::Leave(0, None)).await;
                        }
                        if took_local {
                            self.event_tx.send(EmulationEvent::LocalOwnershipTaken).expect("channel closed");
                        }
                    },
                    EmulationRequest::ChangePort(port) => {
                        self.listener.request_port_change(port);
                        let result = self.listener.port_changed().await;
                        self.event_tx.send(EmulationEvent::PortChanged(result)).expect("channel closed");
                    }
                    EmulationRequest::Terminate => break,
                },
                _ = interval.tick() => {
                    last_response.retain(|&addr,instant| {
                        if instant.elapsed() > Duration::from_secs(1) {
                            log::warn!("releasing keys: {addr} not responding!");
                            ownership.leave(addr);
                            self.emulation_proxy.remove(addr);
                            self.event_tx.send(EmulationEvent::Disconnected { addr }).expect("channel closed");
                            false
                        } else {
                            true
                        }
                    });
                }
            }
        }
        self.listener.terminate().await;
        self.emulation_proxy.terminate().await;
    }
}

/// proxy handling the actual input emulation,
/// discarding events when it is disabled
pub(crate) struct EmulationProxy {
    emulation_active: Rc<Cell<bool>>,
    exit_requested: Rc<Cell<bool>>,
    request_tx: Sender<ProxyRequest>,
    event_rx: Receiver<EmulationEvent>,
    task: JoinHandle<()>,
}

enum ProxyRequest {
    Input(Event, SocketAddr),
    /// place the pointer at normalized screen coordinates for the given
    /// peer's virtual input before any of its subsequent input events
    Warp(SocketAddr, f64, f64),
    Remove(SocketAddr),
    Terminate,
    Reenable,
}

impl EmulationProxy {
    fn new(backend: Option<input_emulation::Backend>) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let emulation_active = Rc::new(Cell::new(false));
        let exit_requested = Rc::new(Cell::new(false));
        let emulation_task = EmulationTask {
            backend,
            exit_requested: exit_requested.clone(),
            request_rx,
            event_tx,
            handles: Default::default(),
            next_id: 0,
        };
        let task = spawn_local(emulation_task.run());
        Self {
            emulation_active,
            exit_requested,
            request_tx,
            task,
            event_rx,
        }
    }

    async fn event(&mut self) -> EmulationEvent {
        let event = self.event_rx.recv().await.expect("channel closed");
        if let EmulationEvent::EmulationEnabled = event {
            self.emulation_active.replace(true);
        }
        if let EmulationEvent::EmulationDisabled = event {
            self.emulation_active.replace(false);
        }
        event
    }

    fn consume(&self, event: Event, addr: SocketAddr) {
        // ignore events if emulation is currently disabled
        if self.emulation_active.get() {
            self.request_tx
                .send(ProxyRequest::Input(event, addr))
                .expect("channel closed");
        }
    }

    fn warp(&self, addr: SocketAddr, x: f64, y: f64) {
        // ignore warps if emulation is currently disabled
        if self.emulation_active.get() {
            self.request_tx
                .send(ProxyRequest::Warp(addr, x, y))
                .expect("channel closed");
        }
    }

    fn remove(&self, addr: SocketAddr) {
        self.request_tx
            .send(ProxyRequest::Remove(addr))
            .expect("channel closed");
    }

    fn reenable(&self) {
        self.request_tx
            .send(ProxyRequest::Reenable)
            .expect("channel closed");
    }

    async fn terminate(&mut self) {
        self.exit_requested.replace(true);
        self.request_tx
            .send(ProxyRequest::Terminate)
            .expect("channel closed");
        let _ = (&mut self.task).await;
    }
}

struct EmulationTask {
    backend: Option<input_emulation::Backend>,
    exit_requested: Rc<Cell<bool>>,
    request_rx: Receiver<ProxyRequest>,
    event_tx: Sender<EmulationEvent>,
    handles: HashMap<SocketAddr, EmulationHandle>,
    next_id: EmulationHandle,
}

impl EmulationTask {
    async fn get_or_create_handle(
        &mut self,
        emulation: &mut InputEmulation,
        addr: SocketAddr,
    ) -> EmulationHandle {
        match self.handles.get(&addr) {
            Some(&handle) => handle,
            None => {
                let handle = self.next_id;
                self.next_id += 1;
                emulation.create(handle).await;
                self.handles.insert(addr, handle);
                handle
            }
        }
    }

    async fn run(mut self) {
        loop {
            if let Err(e) = self.do_emulation().await {
                log::warn!("input emulation exited: {e}");
            }
            if self.exit_requested.get() {
                break;
            }
            // wait for reenable request
            loop {
                match self.request_rx.recv().await.expect("channel closed") {
                    ProxyRequest::Reenable => break,
                    ProxyRequest::Terminate => return,
                    ProxyRequest::Input(..) => { /* emulation inactive => ignore */ }
                    ProxyRequest::Warp(..) => { /* emulation inactive => ignore */ }
                    ProxyRequest::Remove(..) => { /* emulation inactive => ignore */ }
                }
            }
        }
    }

    async fn do_emulation(&mut self) -> Result<(), InputEmulationError> {
        log::info!("creating input emulation ...");
        let mut emulation = tokio::select! {
            r = InputEmulation::new(self.backend) => r?,
            // allow termination event while requesting input emulation
            _ = wait_for_termination(&mut self.request_rx) => return Ok(()),
        };

        // used to send enabled and disabled events
        let _emulation_guard = DropGuard::new(
            self.event_tx.clone(),
            EmulationEvent::EmulationEnabled,
            EmulationEvent::EmulationDisabled,
        );

        // create active handles
        if let Err(e) = self.create_clients(&mut emulation).await {
            emulation.terminate().await;
            return Err(e);
        }

        let res = self.do_emulation_session(&mut emulation).await;
        // FIXME replace with async drop when stabilized
        emulation.terminate().await;
        res
    }

    async fn create_clients(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<(), InputEmulationError> {
        for handle in self.handles.values() {
            tokio::select! {
                _ = emulation.create(*handle) => {},
                _ = wait_for_termination(&mut self.request_rx) => return Ok(()),
            }
        }
        Ok(())
    }

    async fn do_emulation_session(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<(), InputEmulationError> {
        loop {
            tokio::select! {
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    ProxyRequest::Input(event, addr) => {
                        let handle = self.get_or_create_handle(emulation, addr).await;
                        emulation.consume(event, handle).await?;
                    },
                    ProxyRequest::Warp(addr, x, y) => {
                        let handle = self.get_or_create_handle(emulation, addr).await;
                        emulation.warp(handle, x, y).await?;
                    },
                    ProxyRequest::Remove(addr) => {
                        if let Some(handle) = self.handles.remove(&addr) {
                            emulation.destroy(handle).await;
                        }
                    }
                    ProxyRequest::Terminate => break Ok(()),
                    ProxyRequest::Reenable => continue,
                },
            }
        }
    }
}

fn to_ipc_pos(pos: Position) -> lan_mouse_ipc::Position {
    match pos {
        Position::Left => lan_mouse_ipc::Position::Left,
        Position::Right => lan_mouse_ipc::Position::Right,
        Position::Top => lan_mouse_ipc::Position::Top,
        Position::Bottom => lan_mouse_ipc::Position::Bottom,
    }
}

async fn wait_for_termination(rx: &mut Receiver<ProxyRequest>) {
    loop {
        match rx.recv().await.expect("channel closed") {
            ProxyRequest::Terminate => return,
            ProxyRequest::Input(_, _) => continue,
            ProxyRequest::Warp(..) => continue,
            ProxyRequest::Remove(_) => continue,
            ProxyRequest::Reenable => continue,
        }
    }
}

/// normalized screen coordinates of the point a cursor enters this
/// device: the entered edge at the given fraction along it
fn entry_coordinates(pos: Position, fraction: f64) -> (f64, f64) {
    // Stay inside the display, not on its one-pixel capture barriers.
    // At the supported 2400x1350 logical size this leaves 1.35–2.4px
    // clearance; an exact edge/corner placement immediately recaptures.
    const INSET: f64 = 0.001;
    let fraction = fraction.clamp(INSET, 1.0 - INSET);
    match pos {
        Position::Left => (INSET, fraction),
        Position::Right => (1.0 - INSET, fraction),
        Position::Top => (fraction, INSET),
        Position::Bottom => (fraction, 1.0 - INSET),
    }
}

/// an incoming handoff places the pointer via emulation only when it is
/// a fresh ownership transfer with crossing geometry; while an outgoing
/// capture is active the capture-side release warp is authoritative and
/// an emulation warp would be a spurious second warp
fn should_warp_on_enter(
    disposition: &EnterDisposition,
    fraction: Option<f64>,
    outgoing_capture_active: bool,
) -> bool {
    matches!(disposition, EnterDisposition::New) && fraction.is_some() && !outgoing_capture_active
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

/// Only a fresh authenticated Enter can transfer ownership away from a local mouse.
/// Connection liveness (Ping/Pong) is deliberately independent of input ownership.
#[derive(Default)]
struct PointerOwnership {
    incoming: HashSet<SocketAddr>,
    revoked: HashMap<SocketAddr, Instant>,
}

#[derive(Debug, PartialEq, Eq)]
enum EnterDisposition {
    New,
    Repeat,
    Revoked,
}

impl PointerOwnership {
    fn enter(&mut self, addr: SocketAddr) -> EnterDisposition {
        // Queued Enter retries must not undo a physical takeover. A short
        // quiet period also covers a delayed retry arriving after Leave.
        self.revoked
            .retain(|_, when| when.elapsed() < Duration::from_millis(500));
        if self.revoked.contains_key(&addr) {
            EnterDisposition::Revoked
        } else if self.incoming.insert(addr) {
            EnterDisposition::New
        } else {
            EnterDisposition::Repeat
        }
    }
    fn leave(&mut self, addr: SocketAddr) {
        self.incoming.remove(&addr);
    }
    fn accepts(&self, addr: SocketAddr) -> bool {
        self.incoming.contains(&addr)
    }
    fn take_local(&mut self) -> Vec<SocketAddr> {
        let peers: Vec<_> = self.incoming.drain().collect();
        for &addr in &peers {
            self.revoked.insert(addr, Instant::now());
        }
        peers
    }
}

#[cfg(test)]
mod warp_tests {
    use super::*;

    #[test]
    fn entry_coordinates_map_position_and_fraction_to_normalized_edge_point() {
        assert_eq!(entry_coordinates(Position::Left, 0.25), (0.001, 0.25));
        assert_eq!(entry_coordinates(Position::Right, 0.75), (0.999, 0.75));
        assert_eq!(entry_coordinates(Position::Top, 0.5), (0.5, 0.001));
        assert_eq!(entry_coordinates(Position::Bottom, 0.5), (0.5, 0.999));
    }

    #[test]
    fn entry_placement_is_inside_screen_not_on_a_recapture_barrier() {
        for pos in [
            Position::Left,
            Position::Right,
            Position::Top,
            Position::Bottom,
        ] {
            for fraction in [0.0, 0.5, 1.0] {
                let (x, y) = entry_coordinates(pos, fraction);
                assert!(
                    x > 0.0 && x < 1.0 && y > 0.0 && y < 1.0,
                    "entry must be inside screen, not an edge barrier: {pos:?} ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn warp_only_for_new_ownership_with_geometry_and_no_outgoing_capture() {
        assert!(should_warp_on_enter(
            &EnterDisposition::New,
            Some(0.5),
            false
        ));
        // a repeated Enter (retry waiting for Ack) must not warp again
        assert!(!should_warp_on_enter(
            &EnterDisposition::Repeat,
            Some(0.5),
            false
        ));
        assert!(!should_warp_on_enter(
            &EnterDisposition::Revoked,
            Some(0.5),
            false
        ));
        // no crossing geometry: fail closed
        assert!(!should_warp_on_enter(&EnterDisposition::New, None, false));
        // outgoing capture active: the capture-side warp on release is
        // authoritative, an emulation warp would be a spurious second warp
        assert!(!should_warp_on_enter(
            &EnterDisposition::New,
            Some(0.5),
            true
        ));
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    fn peer() -> SocketAddr {
        "127.0.0.1:4242".parse().unwrap()
    }
    #[test]
    fn physical_mouse_revokes_remote_input_until_fresh_enter() {
        let mut o = PointerOwnership::default();
        assert_eq!(o.enter(peer()), EnterDisposition::New);
        assert!(o.accepts(peer()));
        assert_eq!(o.take_local(), vec![peer()]);
        assert!(!o.accepts(peer())); // also rejects queued key-downs after takeover
        assert!(o.take_local().is_empty()); // continued source motion is a no-op
        assert_eq!(o.enter(peer()), EnterDisposition::Revoked);
        o.leave(peer());
        assert_eq!(o.enter(peer()), EnterDisposition::Revoked);
        o.revoked
            .insert(peer(), Instant::now() - Duration::from_secs(1));
        assert_eq!(o.enter(peer()), EnterDisposition::New);
        assert!(o.accepts(peer())); // normal edge crossing still works
    }
    #[test]
    fn normal_leave_and_disconnect_do_not_leave_stale_ownership() {
        let mut o = PointerOwnership::default();
        o.enter(peer());
        o.leave(peer());
        assert!(o.take_local().is_empty());
        assert!(!o.accepts(peer()));
    }
    #[test]
    fn local_mouse_revokes_all_senders() {
        let mut o = PointerOwnership::default();
        o.enter(peer());
        o.enter("127.0.0.2:4242".parse().unwrap());
        assert_eq!(o.take_local().len(), 2);
        assert!(!o.accepts(peer()));
    }

    #[test]
    fn enter_retries_are_idempotent() {
        let mut o = PointerOwnership::default();
        assert_eq!(o.enter(peer()), EnterDisposition::New);
        assert_eq!(o.enter(peer()), EnterDisposition::Repeat);
        o.leave(peer());
        assert_eq!(o.enter(peer()), EnterDisposition::New);
    }
}
