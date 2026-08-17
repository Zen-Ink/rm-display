use bytes::{Buf, BufMut, BytesMut};
use prost::Message;
use thiserror::Error;

use crate::Envelope;

pub const MAGIC: [u8; 4] = *b"RMD2";
pub const HEADER_LEN: usize = 8;
pub const PRE_HANDSHAKE_MAX_PAYLOAD: usize = 64 * 1024;
pub const HARD_MAX_PAYLOAD: usize = 32 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum WireError {
    #[error("invalid rm-display magic")]
    InvalidMagic,
    #[error("protobuf payload length {actual} exceeds limit {limit}")]
    PayloadTooLarge { actual: usize, limit: usize },
    #[error("protobuf envelope is malformed: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("protobuf envelope is too large to encode")]
    EncodeTooLarge,
}

#[derive(Debug, Clone)]
pub struct WireCodec {
    max_payload: usize,
}

impl WireCodec {
    pub fn new(max_payload: usize) -> Self {
        Self {
            max_payload: max_payload.min(HARD_MAX_PAYLOAD),
        }
    }

    pub fn pre_handshake() -> Self {
        Self::new(PRE_HANDSHAKE_MAX_PAYLOAD)
    }

    pub fn max_payload(&self) -> usize {
        self.max_payload
    }

    pub fn set_max_payload(&mut self, max_payload: usize) {
        self.max_payload = max_payload.min(HARD_MAX_PAYLOAD);
    }

    pub fn encode(&self, envelope: &Envelope, dst: &mut BytesMut) -> Result<(), WireError> {
        let encoded_len = envelope.encoded_len();
        if encoded_len > self.max_payload {
            return Err(WireError::PayloadTooLarge {
                actual: encoded_len,
                limit: self.max_payload,
            });
        }
        let len = u32::try_from(encoded_len).map_err(|_| WireError::EncodeTooLarge)?;
        dst.reserve(HEADER_LEN + encoded_len);
        dst.extend_from_slice(&MAGIC);
        dst.put_u32(len);
        envelope
            .encode(dst)
            .expect("BytesMut was reserved for the encoded protobuf envelope");
        Ok(())
    }

    pub fn decode(&self, src: &mut BytesMut) -> Result<Option<Envelope>, WireError> {
        if src.len() < HEADER_LEN {
            return Ok(None);
        }
        if src[..4] != MAGIC {
            return Err(WireError::InvalidMagic);
        }
        let payload_len = u32::from_be_bytes([src[4], src[5], src[6], src[7]]) as usize;
        if payload_len > self.max_payload {
            return Err(WireError::PayloadTooLarge {
                actual: payload_len,
                limit: self.max_payload,
            });
        }
        if src.len() < HEADER_LEN + payload_len {
            return Ok(None);
        }
        src.advance(HEADER_LEN);
        let payload = src.split_to(payload_len).freeze();
        Ok(Some(Envelope::decode(payload)?))
    }
}
