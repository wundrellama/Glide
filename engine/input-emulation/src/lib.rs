use async_trait::async_trait;
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
};

use input_event::{Event, KeyboardEvent};

pub use self::error::{EmulationCreationError, EmulationError, InputEmulationError};

#[cfg(windows)]
mod windows;

#[cfg(x11)]
mod x11;

#[cfg(wlroots)]
mod wlroots;

#[cfg(rdp)]
mod xdg_desktop_portal;

#[cfg(libei)]
mod libei;

#[cfg(target_os = "macos")]
mod macos;

/// fallback input emulation (logs events)
mod dummy;
mod error;

pub type EmulationHandle = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    #[cfg(wlroots)]
    Wlroots,
    #[cfg(libei)]
    Libei,
    #[cfg(rdp)]
    Xdp,
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
            #[cfg(wlroots)]
            Backend::Wlroots => write!(f, "wlroots"),
            #[cfg(libei)]
            Backend::Libei => write!(f, "libei"),
            #[cfg(rdp)]
            Backend::Xdp => write!(f, "xdg-desktop-portal"),
            #[cfg(x11)]
            Backend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            Backend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            Backend::MacOs => write!(f, "macos"),
            Backend::Dummy => write!(f, "dummy"),
        }
    }
}

pub struct InputEmulation {
    emulation: Box<dyn Emulation>,
    handles: HashSet<EmulationHandle>,
    pressed_keys: HashMap<EmulationHandle, HashSet<u32>>,
}

impl InputEmulation {
    async fn with_backend(backend: Backend) -> Result<InputEmulation, EmulationCreationError> {
        let emulation: Box<dyn Emulation> = match backend {
            #[cfg(wlroots)]
            Backend::Wlroots => Box::new(wlroots::WlrootsEmulation::new()?),
            #[cfg(libei)]
            Backend::Libei => Box::new(libei::LibeiEmulation::new().await?),
            #[cfg(x11)]
            Backend::X11 => Box::new(x11::X11Emulation::new()?),
            #[cfg(rdp)]
            Backend::Xdp => Box::new(xdg_desktop_portal::DesktopPortalEmulation::new().await?),
            #[cfg(windows)]
            Backend::Windows => Box::new(windows::WindowsEmulation::new()?),
            #[cfg(target_os = "macos")]
            Backend::MacOs => Box::new(macos::MacOSEmulation::new()?),
            Backend::Dummy => Box::new(dummy::DummyEmulation::new()),
        };
        Ok(Self {
            emulation,
            handles: HashSet::new(),
            pressed_keys: HashMap::new(),
        })
    }

    pub async fn new(backend: Option<Backend>) -> Result<InputEmulation, EmulationCreationError> {
        if let Some(backend) = backend {
            let b = Self::with_backend(backend).await;
            if b.is_ok() {
                log::info!("using emulation backend: {backend}");
            }
            return b;
        }

        for backend in [
            #[cfg(wlroots)]
            Backend::Wlroots,
            #[cfg(libei)]
            Backend::Libei,
            #[cfg(rdp)]
            Backend::Xdp,
            #[cfg(x11)]
            Backend::X11,
            #[cfg(windows)]
            Backend::Windows,
            #[cfg(target_os = "macos")]
            Backend::MacOs,
            Backend::Dummy,
        ] {
            match Self::with_backend(backend).await {
                Ok(b) => {
                    log::info!("using emulation backend: {backend}");
                    return Ok(b);
                }
                Err(e) if e.cancelled_by_user() => return Err(e),
                Err(e) => log::warn!("{e}"),
            }
        }

        Err(EmulationCreationError::NoAvailableBackend)
    }

    pub async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        match event {
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                // prevent double pressed / released keys
                if self.update_pressed_keys(handle, key, state) {
                    self.emulation.consume(event, handle).await?;
                }
                Ok(())
            }
            _ => self.emulation.consume(event, handle).await,
        }
    }

    /// place the pointer at normalized (`0.0..=1.0`) screen coordinates
    pub async fn warp(
        &mut self,
        handle: EmulationHandle,
        x: f64,
        y: f64,
    ) -> Result<(), EmulationError> {
        self.emulation.warp(handle, x, y).await
    }

    pub async fn create(&mut self, handle: EmulationHandle) -> bool {
        if self.handles.insert(handle) {
            self.pressed_keys.insert(handle, HashSet::new());
            self.emulation.create(handle).await;
            true
        } else {
            false
        }
    }

    pub async fn destroy(&mut self, handle: EmulationHandle) {
        let _ = self.release_keys(handle).await;
        if self.handles.remove(&handle) {
            self.pressed_keys.remove(&handle);
            self.emulation.destroy(handle).await
        }
    }

    pub async fn terminate(&mut self) {
        for handle in self.handles.iter().cloned().collect::<Vec<_>>() {
            self.destroy(handle).await
        }
        self.emulation.terminate().await
    }

    pub async fn release_keys(&mut self, handle: EmulationHandle) -> Result<(), EmulationError> {
        if let Some(keys) = self.pressed_keys.get_mut(&handle) {
            let keys = keys.drain().collect::<Vec<_>>();
            for key in keys {
                let event = Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key,
                    state: 0,
                });
                self.emulation.consume(event, handle).await?;
                if let Ok(key) = input_event::scancode::Linux::try_from(key) {
                    log::warn!("releasing stuck key: {key:?}");
                }
            }
        }

        let event = Event::Keyboard(KeyboardEvent::Modifiers {
            depressed: 0,
            latched: 0,
            locked: 0,
            group: 0,
        });
        self.emulation.consume(event, handle).await?;
        Ok(())
    }

    pub fn has_pressed_keys(&self, handle: EmulationHandle) -> bool {
        self.pressed_keys
            .get(&handle)
            .is_some_and(|p| !p.is_empty())
    }

    /// update the pressed_keys for the given handle
    /// returns whether the event should be processed
    fn update_pressed_keys(&mut self, handle: EmulationHandle, key: u32, state: u8) -> bool {
        let Some(pressed_keys) = self.pressed_keys.get_mut(&handle) else {
            return false;
        };

        if state == 0 {
            // currently pressed => can release
            pressed_keys.remove(&key)
        } else {
            // currently not pressed => can press
            pressed_keys.insert(key)
        }
    }
}

#[async_trait]
trait Emulation: Send {
    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError>;
    /// place the pointer at normalized (`0.0..=1.0`) screen coordinates,
    /// e.g. the point a peer's cursor entered this device; backends
    /// without pointer placement ignore the request
    async fn warp(
        &mut self,
        handle: EmulationHandle,
        x: f64,
        y: f64,
    ) -> Result<(), EmulationError> {
        let _ = (handle, x, y);
        Ok(())
    }
    async fn create(&mut self, handle: EmulationHandle);
    async fn destroy(&mut self, handle: EmulationHandle);
    async fn terminate(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct WarpLog {
        warps: Vec<(EmulationHandle, f64, f64)>,
    }

    struct WarpStub(Arc<Mutex<WarpLog>>);

    #[async_trait]
    impl Emulation for WarpStub {
        async fn consume(
            &mut self,
            _event: Event,
            _handle: EmulationHandle,
        ) -> Result<(), EmulationError> {
            Ok(())
        }
        async fn warp(
            &mut self,
            handle: EmulationHandle,
            x: f64,
            y: f64,
        ) -> Result<(), EmulationError> {
            self.0.lock().unwrap().warps.push((handle, x, y));
            Ok(())
        }
        async fn create(&mut self, _handle: EmulationHandle) {}
        async fn destroy(&mut self, _handle: EmulationHandle) {}
        async fn terminate(&mut self) {}
    }

    struct NoopStub;

    #[async_trait]
    impl Emulation for NoopStub {
        async fn consume(
            &mut self,
            _event: Event,
            _handle: EmulationHandle,
        ) -> Result<(), EmulationError> {
            Ok(())
        }
        async fn create(&mut self, _handle: EmulationHandle) {}
        async fn destroy(&mut self, _handle: EmulationHandle) {}
        async fn terminate(&mut self) {}
    }

    #[tokio::test(flavor = "current_thread")]
    async fn warp_forwards_normalized_coordinates_to_backend() {
        let log = Arc::new(Mutex::new(WarpLog::default()));
        let mut emulation = InputEmulation {
            emulation: Box::new(WarpStub(log.clone())),
            handles: HashSet::new(),
            pressed_keys: HashMap::new(),
        };
        emulation.warp(7, 0.25, 0.75).await.expect("warp");
        assert_eq!(log.lock().unwrap().warps, vec![(7, 0.25, 0.75)]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn warp_defaults_to_noop() {
        let mut emulation = InputEmulation {
            emulation: Box::new(NoopStub),
            handles: HashSet::new(),
            pressed_keys: HashMap::new(),
        };
        emulation.warp(0, 0.5, 0.5).await.expect("warp");
    }
}
