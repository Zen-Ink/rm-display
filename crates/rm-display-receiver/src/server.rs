use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use rm_display_core::{
    FullRefreshReason, GraySurface, PanelBackend, PanelError, RefreshDecision, Waveform,
};
use rm_display_protocol::wire::{WireCodec, WireError};
use rm_display_protocol::Rect;
use rm_display_transport::{Psk, PskServerConfig};
use thiserror::Error;

use crate::config::{ConfigError, ReceiverConfig, SecurityMode};
#[cfg(all(target_os = "linux", target_arch = "aarch64", feature = "quill"))]
use crate::evdev::discover_remarkable_touch_device;
#[cfg(target_os = "linux")]
use crate::evdev::EvdevTouchDevice;
#[cfg(target_os = "linux")]
use crate::evdev::PowerKeyDevice;
use crate::pairing::{pairing_uri, render_pairing_frame, PairingError};
use crate::session::{Session, SessionError};

pub struct ReceiverServer {
    config: ReceiverConfig,
    listener: TcpListener,
    psk: Option<PskServerConfig>,
    managed_psk_path: Option<PathBuf>,
    panel: Box<dyn PanelBackend>,
    pairing_frame: GraySurface,
    pairing_qr_enabled: bool,
    input_status: String,
    #[cfg(target_os = "linux")]
    input: Option<EvdevTouchDevice>,
    #[cfg(target_os = "linux")]
    power_key: Option<PowerKeyDevice>,
}

enum IdleWait {
    Stream(TcpStream),
    NewPair,
    Exit,
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct IdleReady {
    listener: bool,
    touch: bool,
    power: bool,
}

#[cfg(target_os = "linux")]
fn poll_idle_sources(
    listener: &TcpListener,
    input: Option<&EvdevTouchDevice>,
    power_key: Option<&PowerKeyDevice>,
) -> io::Result<IdleReady> {
    let mut descriptors = vec![libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    let touch_index = input.map(|device| {
        let index = descriptors.len();
        descriptors.push(libc::pollfd {
            fd: device.event_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        index
    });
    let power_index = power_key.map(|device| {
        let index = descriptors.len();
        descriptors.push(libc::pollfd {
            fd: device.event_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        index
    });
    loop {
        let result = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                -1,
            )
        };
        if result >= 0 {
            return Ok(IdleReady {
                listener: descriptors[0].revents & libc::POLLIN != 0,
                touch: touch_index
                    .is_some_and(|index| descriptors[index].revents & libc::POLLIN != 0),
                power: power_index
                    .is_some_and(|index| descriptors[index].revents & libc::POLLIN != 0),
            });
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

impl ReceiverServer {
    pub fn bind(
        mut config: ReceiverConfig,
        panel: Box<dyn PanelBackend>,
    ) -> Result<Self, ServerError> {
        config.validate()?;
        let psk = match &config.security {
            SecurityMode::Plaintext => None,
            SecurityMode::Psk(psk) => Some(PskServerConfig::new(psk.clone())?),
        };
        let listener = TcpListener::bind(config.listen)?;
        let panel_info = panel.info();
        let pairing_frame = GraySurface::new(panel_info.width, panel_info.height, 255)?;
        #[cfg(target_os = "linux")]
        let (input, input_status) = match &config.input_device {
            Some(path) => {
                let info = panel.info();
                let device = EvdevTouchDevice::open(path, info.width, info.height)?;
                let status = format!(
                    "touch input enabled: {} ({:?}; explicit --input)",
                    device.path().display(),
                    device.name()
                );
                (Some(device), status)
            }
            None => auto_open_touch(panel.as_ref()),
        };
        #[cfg(target_os = "linux")]
        {
            config.input_device = input.as_ref().map(|device| device.path().to_path_buf());
        }
        #[cfg(target_os = "linux")]
        let (power_key, power_status) = auto_open_power_key();
        #[cfg(target_os = "linux")]
        let input_status = format!("{input_status}; {power_status}");
        #[cfg(not(target_os = "linux"))]
        let input_status = {
            config.input_device = None;
            "touch input disabled: evdev is available only on Linux".into()
        };
        Ok(Self {
            config,
            listener,
            psk,
            managed_psk_path: None,
            panel,
            pairing_frame,
            pairing_qr_enabled: false,
            input_status,
            #[cfg(target_os = "linux")]
            input,
            #[cfg(target_os = "linux")]
            power_key,
        })
    }

    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    pub fn input_status(&self) -> &str {
        &self.input_status
    }

    /// Selects the persistent receiver-owned PSK file updated by NEW PAIR.
    pub fn set_managed_psk_path(&mut self, path: Option<PathBuf>) {
        self.managed_psk_path = path;
    }

    /// Paint the server-authored pairing offer. A managed PSK is intentionally
    /// included in the QR and must never be written to logs.
    pub fn show_pairing_qr(&mut self) -> Result<(), ServerError> {
        let uri = pairing_uri(&self.config, self.local_addr()?);
        let info = self.panel.info();
        let frame = render_pairing_frame(info.width, info.height, &uri)?;
        self.panel.submit(
            &frame,
            &[Rect {
                x: 0,
                y: 0,
                width: info.width,
                height: info.height,
            }],
            RefreshDecision {
                waveform: Waveform::FullQuality,
                complete_refresh: true,
                full_refresh_reason: FullRefreshReason::Forced,
            },
        )?;
        self.pairing_frame = frame;
        self.pairing_qr_enabled = true;
        Ok(())
    }

    fn new_pairing(&mut self) -> Result<(), ServerError> {
        if matches!(self.config.security, SecurityMode::Psk(_)) {
            let psk = Psk::generate();
            if let Some(path) = &self.managed_psk_path {
                psk.store_atomic(path)?;
            }
            self.psk = Some(PskServerConfig::new(psk.clone())?);
            self.config.security = SecurityMode::Psk(psk);
        }
        self.show_pairing_qr()
    }

    /// Serve one producer at a time. Disconnect always discards the session,
    /// active surface, and delta base before the next accept.
    pub fn run(&mut self) -> Result<(), ServerError> {
        loop {
            let stream = match self.wait_for_idle_connection()? {
                IdleWait::Stream(stream) => stream,
                IdleWait::NewPair => {
                    self.new_pairing()?;
                    continue;
                }
                IdleWait::Exit => return Ok(()),
            };
            match self.serve_stream(stream) {
                Err(ServerError::ReceiverExit) => return Ok(()),
                Err(ServerError::NewPair) => {
                    self.new_pairing()?;
                    continue;
                }
                Err(error) => eprintln!("rm-display: connection ended: {error}"),
                Ok(()) => {}
            }
            if self.pairing_qr_enabled {
                self.show_pairing_qr()?;
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn wait_for_idle_connection(&mut self) -> Result<IdleWait, ServerError> {
        let started = Instant::now();
        let mut idle = Session::new_with_fallback(
            self.config.clone(),
            self.panel.as_mut(),
            Some(self.pairing_frame.clone()),
        );
        loop {
            let ready =
                poll_idle_sources(&self.listener, self.input.as_ref(), self.power_key.as_ref())?;
            let now = started.elapsed();
            if ready.power {
                if let Some(device) = self.power_key.as_ref() {
                    for _ in 0..device.drain_presses() {
                        let _ = idle.power_key_pressed(now)?;
                    }
                }
            }
            if ready.touch {
                if let Some(device) = self.input.as_mut() {
                    let reports = device.drain_reports()?;
                    let _ = idle.input_reports(reports, now)?;
                }
            }
            if idle.receiver_exit_requested() {
                self.config.refresh_policy = idle.refresh_policy();
                return Ok(IdleWait::Exit);
            }
            if idle.pairing_reset_requested() {
                self.config.refresh_policy = idle.refresh_policy();
                return Ok(IdleWait::NewPair);
            }
            if ready.listener {
                let _ = idle.close_local_menu(now);
                self.config.refresh_policy = idle.refresh_policy();
                drop(idle);
                let (stream, _) = self.listener.accept()?;
                return Ok(IdleWait::Stream(stream));
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn wait_for_idle_connection(&mut self) -> Result<IdleWait, ServerError> {
        let (stream, _) = self.listener.accept()?;
        Ok(IdleWait::Stream(stream))
    }

    pub fn run_one(&mut self) -> Result<(), ServerError> {
        let (stream, _) = self.listener.accept()?;
        match self.serve_stream(stream) {
            Err(ServerError::ReceiverExit) => Ok(()),
            Err(ServerError::NewPair) => self.new_pairing(),
            result => result,
        }
    }

    fn serve_stream(&mut self, stream: TcpStream) -> Result<(), ServerError> {
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        if let Some(psk) = &self.psk {
            // Bound an unauthenticated peer's opportunity to stall the PSK
            // handshake. Once authenticated, the short timeout drives frame
            // deadlines and physical-input polling while the producer is idle.
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            let mut secured = psk.accept(stream)?;
            secured
                .get_ref()
                .set_read_timeout(Some(Duration::from_millis(20)))?;
            drive_connection(
                &mut secured,
                self.config.clone(),
                self.panel.as_mut(),
                Some(self.pairing_frame.clone()),
                #[cfg(target_os = "linux")]
                self.input.as_mut(),
                #[cfg(target_os = "linux")]
                self.power_key.as_ref(),
            )
        } else {
            let mut plain = stream;
            plain.set_read_timeout(Some(Duration::from_millis(20)))?;
            drive_connection(
                &mut plain,
                self.config.clone(),
                self.panel.as_mut(),
                Some(self.pairing_frame.clone()),
                #[cfg(target_os = "linux")]
                self.input.as_mut(),
                #[cfg(target_os = "linux")]
                self.power_key.as_ref(),
            )
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "aarch64", feature = "quill"))]
fn auto_open_power_key() -> (Option<PowerKeyDevice>, String) {
    match PowerKeyDevice::discover_open() {
        Ok(device) => {
            let status = format!(
                "power-key menu enabled: {} ({:?})",
                device.path().display(),
                device.name()
            );
            (Some(device), status)
        }
        Err(reason) => (None, format!("power-key menu disabled: {reason}")),
    }
}

#[cfg(all(
    target_os = "linux",
    not(all(target_arch = "aarch64", feature = "quill"))
))]
fn auto_open_power_key() -> (Option<PowerKeyDevice>, String) {
    (
        None,
        "power-key menu disabled: only enabled for reMarkable AArch64 Quill builds".into(),
    )
}

#[cfg(all(target_os = "linux", target_arch = "aarch64", feature = "quill"))]
fn auto_open_touch(panel: &dyn PanelBackend) -> (Option<EvdevTouchDevice>, String) {
    let candidate = match discover_remarkable_touch_device() {
        Ok(candidate) => candidate,
        Err(reason) => return (None, format!("touch input disabled: {reason}")),
    };
    let info = panel.info();
    match EvdevTouchDevice::open(&candidate.path, info.width, info.height) {
        Ok(device) => {
            let status = format!(
                "touch input enabled: {} ({:?}; automatic discovery)",
                device.path().display(),
                device.name()
            );
            (Some(device), status)
        }
        Err(error) => (
            None,
            format!(
                "touch input disabled: selected {} ({:?}) but could not open/grab it: {error}",
                candidate.path.display(),
                candidate.name
            ),
        ),
    }
}

#[cfg(all(
    target_os = "linux",
    not(all(target_arch = "aarch64", feature = "quill"))
))]
fn auto_open_touch(_panel: &dyn PanelBackend) -> (Option<EvdevTouchDevice>, String) {
    (
        None,
        "touch input disabled: automatic discovery is limited to reMarkable AArch64 Quill builds"
            .into(),
    )
}

fn drive_connection<T: Read + Write>(
    stream: &mut T,
    config: ReceiverConfig,
    panel: &mut dyn PanelBackend,
    fallback: Option<GraySurface>,
    #[cfg(target_os = "linux")] input_device: Option<&mut EvdevTouchDevice>,
    #[cfg(target_os = "linux")] power_key: Option<&PowerKeyDevice>,
) -> Result<(), ServerError> {
    let started = Instant::now();
    let mut session = Session::new_with_fallback(config.clone(), panel, fallback);
    let mut codec = WireCodec::pre_handshake();
    let mut input = BytesMut::with_capacity(64 * 1024);
    let mut read_buffer = [0_u8; 64 * 1024];
    #[cfg(target_os = "linux")]
    let mut input_device = input_device;

    loop {
        let now = started.elapsed();
        write_envelopes(stream, &codec, session.poll(now)?)?;
        #[cfg(target_os = "linux")]
        if let Some(device) = input_device.as_deref_mut() {
            let reports = device.drain_reports()?;
            let envelopes = session.input_reports(reports, now)?;
            write_envelopes(stream, &codec, envelopes)?;
        }
        #[cfg(target_os = "linux")]
        if let Some(device) = power_key {
            for _ in 0..device.drain_presses() {
                let envelopes = session.power_key_pressed(now)?;
                write_envelopes(stream, &codec, envelopes)?;
            }
        }
        if session.is_closed() {
            return if session.pairing_reset_requested() {
                Err(ServerError::NewPair)
            } else if session.receiver_exit_requested() {
                Err(ServerError::ReceiverExit)
            } else {
                Ok(())
            };
        }

        while let Some(envelope) = codec.decode(&mut input)? {
            let related = envelope.message_id;
            match session.handle(envelope, started.elapsed()) {
                Ok(responses) => write_envelopes(stream, &codec, responses)?,
                Err(error) => {
                    let response = session.protocol_error(&error, related);
                    write_envelopes(stream, &codec, [response])?;
                    return Err(ServerError::Session(error));
                }
            }
            if session.is_established() {
                codec.set_max_payload(config.limits.max_payload as usize);
            }
            if session.is_closed() {
                return if session.pairing_reset_requested() {
                    Err(ServerError::NewPair)
                } else if session.receiver_exit_requested() {
                    Err(ServerError::ReceiverExit)
                } else {
                    Ok(())
                };
            }
        }

        match stream.read(&mut read_buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => input.extend_from_slice(&read_buffer[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(ServerError::Io(error)),
        }
    }
}

fn write_envelopes<T, I>(stream: &mut T, codec: &WireCodec, envelopes: I) -> Result<(), ServerError>
where
    T: Write,
    I: IntoIterator<Item = rm_display_protocol::Envelope>,
{
    let mut output = BytesMut::new();
    for envelope in envelopes {
        codec.encode(&envelope, &mut output)?;
    }
    if !output.is_empty() {
        stream.write_all(&output)?;
        stream.flush()?;
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("receiver exit requested from local menu")]
    ReceiverExit,
    #[error("new pairing requested from local menu")]
    NewPair,
    #[error(transparent)]
    Pairing(#[from] PairingError),
    #[error(transparent)]
    Panel(#[from] PanelError),
    #[error(transparent)]
    Surface(#[from] rm_display_core::SurfaceError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    PskConfig(#[from] rm_display_transport::PskConfigError),
    #[error(transparent)]
    PskTransport(#[from] rm_display_transport::PskTransportError),
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error(transparent)]
    Session(#[from] SessionError),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rm_display_core::{MockPanel, RefreshPolicyConfig};

    use super::*;
    use crate::config::{ReceiverLimits, ReservedZeroToken};

    #[test]
    fn new_pairing_rotates_managed_psk_and_keeps_server_identity() {
        let state_directory = std::env::current_dir()
            .unwrap()
            .join(".cache")
            .join("rm-display-receiver-tests")
            .join(format!("new-pair-{}", std::process::id()));
        let psk_path = state_directory.join("pairing.psk");
        let _ = std::fs::remove_dir_all(&state_directory);
        Psk::from_bytes([0x11; 32]).store_new(&psk_path).unwrap();
        let config = ReceiverConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            security: SecurityMode::Psk(Psk::from_bytes([0x11; 32])),
            token_verifier: Arc::new(ReservedZeroToken),
            server_id: [0x22; 16],
            name: "pairing test".into(),
            limits: ReceiverLimits::default(),
            refresh_policy: RefreshPolicyConfig::default(),
            input_device: None,
        };
        let mut server = ReceiverServer::bind(config, Box::new(MockPanel::new(960, 1696))).unwrap();
        server.set_managed_psk_path(Some(psk_path.clone()));
        let before = pairing_uri(&server.config, server.local_addr().unwrap());
        server.new_pairing().unwrap();
        let after = pairing_uri(&server.config, server.local_addr().unwrap());

        assert_ne!(before, after);
        assert!(before.contains(&format!("psk={}", "11".repeat(32))));
        assert!(!after.contains(&format!("psk={}", "11".repeat(32))));
        assert_ne!(Psk::load(psk_path).unwrap().pairing_hex(), "11".repeat(32));
        assert!(after.ends_with("server=22222222222222222222222222222222"));
        std::fs::remove_dir_all(state_directory).unwrap();
    }
}
