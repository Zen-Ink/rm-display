use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use rm_display_core::{RefreshConfigError, RefreshPolicyConfig};
use rm_display_transport::Psk;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct ReceiverLimits {
    pub max_payload: u32,
    pub max_frame_bytes: u32,
    pub max_regions: u32,
    pub max_inflight: u32,
    pub max_inflight_bytes: u64,
    pub max_fps_x100: u32,
    pub settled_deadline_ms: u32,
}

impl Default for ReceiverLimits {
    fn default() -> Self {
        Self {
            max_payload: 8 * 1024 * 1024,
            max_frame_bytes: 8 * 1024 * 1024,
            max_regions: 512,
            max_inflight: 2,
            max_inflight_bytes: 16 * 1024 * 1024,
            max_fps_x100: 400,
            settled_deadline_ms: 300,
        }
    }
}

pub trait TokenVerifier: Send + Sync {
    fn verify(&self, client_id: &[u8], token: &[u8]) -> bool;
}

/// Version 2 reserves `ClientHello.token`; authentication belongs to the
/// selected transport. Accept exactly the protocol-mandated 32 zero bytes.
#[derive(Clone)]
pub struct ReservedZeroToken;

impl TokenVerifier for ReservedZeroToken {
    fn verify(&self, _client_id: &[u8], token: &[u8]) -> bool {
        token.len() == 32 && token.iter().fold(0_u8, |value, byte| value | byte) == 0
    }
}

#[derive(Debug, Clone)]
pub enum SecurityMode {
    /// Unencrypted TCP. This is an explicit configuration property and is
    /// never auto-detected or used as a fallback from a failed PSK handshake.
    Plaintext,
    /// TLS 1.3 external PSK with the protocol's fixed AES-128-GCM suite.
    Psk(Psk),
}

#[derive(Clone)]
pub struct ReceiverConfig {
    pub listen: SocketAddr,
    pub security: SecurityMode,
    pub token_verifier: Arc<dyn TokenVerifier>,
    pub server_id: [u8; 16],
    pub name: String,
    pub limits: ReceiverLimits,
    /// Receiver baseline for e-paper damage, semantic waveform, and cleanup
    /// policy. A v2.2 producer may atomically replace it for its connection
    /// after negotiating custom-profile control; native Quill values and the
    /// decision to perform a complete refresh remain receiver-owned.
    pub refresh_policy: RefreshPolicyConfig,
    /// Explicit Type-B touchscreen override. If absent, production
    /// reMarkable Quill builds safely auto-discover a capable evdev device.
    /// `ReceiverServer::bind` replaces absence with the selected path only
    /// after the device has been successfully opened and grabbed.
    pub input_device: Option<PathBuf>,
}

impl ReceiverConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.server_id == [0; 16] {
            return Err(ConfigError::ZeroServerId);
        }
        if self.limits.max_inflight == 0
            || self.limits.max_inflight_bytes == 0
            || self.limits.max_payload == 0
            || self.limits.max_frame_bytes == 0
            || self.limits.max_regions == 0
        {
            return Err(ConfigError::InvalidLimits);
        }
        self.refresh_policy.validate()?;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("server_id must be nonzero")]
    ZeroServerId,
    #[error("receiver limits must be nonzero")]
    InvalidLimits,
    #[error(transparent)]
    InvalidRefreshPolicy(#[from] RefreshConfigError),
}
