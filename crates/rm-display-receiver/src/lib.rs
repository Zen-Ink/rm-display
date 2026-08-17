//! rm-display v2 receiver session, transports, and device adapters.

pub mod config;
pub mod evdev;
mod local_menu;
#[cfg(any(
    test,
    all(feature = "quill", target_os = "linux", target_arch = "aarch64")
))]
mod native_pixels;
mod pairing;
#[cfg(all(feature = "quill", target_os = "linux", target_arch = "aarch64"))]
pub mod quill;
pub mod server;
pub mod session;

pub use config::{ReceiverConfig, ReceiverLimits, ReservedZeroToken, SecurityMode, TokenVerifier};
pub use server::ReceiverServer;
pub use session::{Session, SessionError};
