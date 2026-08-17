//! Generated rm-display v2 messages, stream framing, and semantic validation.

pub mod generated {
    include!(concat!(env!("OUT_DIR"), "/rm_display.v2.rs"));
}

pub mod semantic;
pub mod wire;

pub use generated::*;
