use std::io::Write;

use anyhow::Result;
use rm_display_protocol::envelope;
use rm_display_protocol::Envelope;
use serde_json::json;

pub fn write_event_jsonl(envelope: &Envelope, output: &mut dyn Write) -> Result<bool> {
    let value = match envelope.body.as_ref() {
        Some(envelope::Body::InputBatch(batch)) => json!({
            "type": "pointer_batch",
            "surface_id": batch.surface_id,
            "generation": batch.generation,
            "sequence": batch.sequence,
            "monotonic_us": batch.monotonic_us,
            "records": batch.records.iter().map(|record| json!({
                "device": record.device,
                "phase": record.phase,
                "flags": record.flags,
                "contact_id": record.contact_id,
                "x": f64::from(record.x_16_16) / 65536.0,
                "y": f64::from(record.y_16_16) / 65536.0,
                "pressure": record.pressure,
                "buttons": record.buttons,
                "tilt_x": record.tilt_x,
                "tilt_y": record.tilt_y,
            })).collect::<Vec<_>>()
        }),
        Some(envelope::Body::KeyInput(key)) => json!({
            "type": "key",
            "surface_id": key.surface_id,
            "generation": key.generation,
            "sequence": key.sequence,
            "monotonic_us": key.monotonic_us,
            "phase": key.phase,
            "modifiers": key.modifiers,
            "usage_page": key.usage_page,
            "usage": key.usage,
        }),
        Some(envelope::Body::TextInput(text)) => json!({
            "type": "text",
            "surface_id": text.surface_id,
            "generation": text.generation,
            "sequence": text.sequence,
            "monotonic_us": text.monotonic_us,
            "operation": text.operation,
            "text": text.text,
        }),
        Some(envelope::Body::ActionInvoke(action)) => json!({
            "type": "action",
            "surface_id": action.surface_id,
            "generation": action.generation,
            "invocation_id": action.invocation_id,
            "action": action.action,
            "argument_hex": hex::encode(&action.argument),
            "result": "ok_after_jsonl_flush",
        }),
        _ => return Ok(false),
    };
    serde_json::to_writer(&mut *output, &value)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use rm_display_protocol::{ActionId, ActionInvoke};

    use super::*;

    #[test]
    fn action_is_stable_jsonl() {
        let envelope = Envelope {
            session_id: 7,
            message_id: 3,
            body: Some(envelope::Body::ActionInvoke(ActionInvoke {
                surface_id: 1,
                generation: 2,
                invocation_id: 9,
                action: ActionId::Back as i32,
                argument: Bytes::from_static(b"x"),
            })),
        };
        let mut output = Vec::new();
        assert!(write_event_jsonl(&envelope, &mut output).unwrap());
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["type"], "action");
        assert_eq!(value["argument_hex"], "78");
    }
}
