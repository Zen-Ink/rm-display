use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use rm_display_protocol::{FrameMetrics, FrameResultCode};
use serde_json::json;

use crate::FrameReport;

#[derive(Debug, Clone)]
struct Sample {
    total_us: u64,
    wait_us: u64,
    queue_us: u64,
    present_us: u64,
}

pub struct StatsReporter {
    human: bool,
    jsonl: Option<Box<dyn Write>>,
    samples: VecDeque<Sample>,
    total_frames: u64,
    total_wire_bytes: u64,
    total_source_wait_us: u64,
    total_prepare_us: u64,
    started: Instant,
}

impl StatsReporter {
    pub fn new(human: bool, jsonl_path: Option<&Path>) -> Result<Self> {
        let jsonl = match jsonl_path {
            None => None,
            Some(path) if path == Path::new("-") => Some(Box::new(io::stdout()) as Box<dyn Write>),
            Some(path) => Some(Box::new(
                OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(path)
                    .with_context(|| format!("cannot open metrics JSONL {}", path.display()))?,
            ) as Box<dyn Write>),
        };
        Ok(Self {
            human,
            jsonl,
            samples: VecDeque::new(),
            total_frames: 0,
            total_wire_bytes: 0,
            total_source_wait_us: 0,
            total_prepare_us: 0,
            started: Instant::now(),
        })
    }

    pub fn record(
        &mut self,
        phase: &str,
        source_wait_us: u64,
        prepare_us: u64,
        report: &FrameReport,
    ) -> Result<()> {
        if !self.human && self.jsonl.is_none() {
            return Ok(());
        }
        let receiver = report.result.metrics.clone().unwrap_or_default();
        let result = FrameResultCode::try_from(report.result.result)
            .map(|code| code.as_str_name())
            .unwrap_or("UNKNOWN");
        let producer = report.producer;
        if self.human {
            eprintln!(
                "frame={} phase={} result={} source_wait={}us prepare={}us build={}us wire_encode={}us socket_write={}us result_wait={}us producer_total={}us wire={}B receiver_decode={}us queue={}us compose={}us native_convert={}us backend_submit={}us receiver_present={}us damage={}px/{} waveform={} full={}",
                report.result.frame_id,
                phase,
                result,
                source_wait_us,
                prepare_us,
                producer.build_us,
                producer.wire_encode_us,
                producer.write_us,
                producer.wait_us,
                producer.total_us,
                producer.wire_bytes,
                receiver.decode_us,
                receiver.queue_us,
                receiver.compose_us,
                receiver.convert_us,
                receiver.submit_us,
                receiver.present_us,
                receiver.damage_pixels,
                receiver.damage_regions,
                receiver.waveform,
                receiver.complete_refresh,
            );
        }
        self.write_json(&json!({
            "kind": "frame",
            "frame_id": report.result.frame_id,
            "phase": phase,
            "result": result,
            "producer": {
                "source_wait_us": source_wait_us,
                "prepare_us": prepare_us,
                "attempts": producer.attempts,
                "build_us": producer.build_us,
                "wire_encode_us": producer.wire_encode_us,
                "socket_write_us": producer.write_us,
                "result_wait_us": producer.wait_us,
                "total_us": producer.total_us,
                "wire_bytes": producer.wire_bytes,
            },
            "receiver": receiver_json(&receiver),
            "physical_ink_settle_us": null,
        }))?;
        self.total_frames = self.total_frames.saturating_add(1);
        self.total_wire_bytes = self.total_wire_bytes.saturating_add(producer.wire_bytes);
        self.total_source_wait_us = self.total_source_wait_us.saturating_add(source_wait_us);
        self.total_prepare_us = self.total_prepare_us.saturating_add(prepare_us);
        if self.samples.len() == SUMMARY_WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(Sample {
            total_us: producer.total_us,
            wait_us: producer.wait_us,
            queue_us: u64::from(receiver.queue_us),
            present_us: u64::from(receiver.present_us),
        });
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        if self.samples.is_empty() {
            return Ok(());
        }
        let elapsed_us = self.started.elapsed().as_micros().max(1) as u64;
        let count = self.total_frames;
        let total = values(&self.samples, |sample| sample.total_us);
        let wait = values(&self.samples, |sample| sample.wait_us);
        let queue = values(&self.samples, |sample| sample.queue_us);
        let present = values(&self.samples, |sample| sample.present_us);
        let source_wait = self.total_source_wait_us;
        let prepare = self.total_prepare_us;
        let fps = count as f64 * 1_000_000.0 / elapsed_us as f64;
        let mib_s =
            self.total_wire_bytes as f64 * 1_000_000.0 / elapsed_us as f64 / (1024.0 * 1024.0);
        if self.human {
            eprintln!(
                "summary frames={} window={} wall={:.3}s fps={:.2} wire={:.2}MiB/s source_wait={:.3}s prepare={:.3}s producer_total_us[p50/p95/max]={}/{}/{} result_wait_us[p50/p95/max]={}/{}/{} receiver_queue_us[p50/p95/max]={}/{}/{} receiver_present_us[p50/p95/max]={}/{}/{} physical_ink_settle=unavailable",
                count,
                self.samples.len(),
                elapsed_us as f64 / 1_000_000.0,
                fps,
                mib_s,
                source_wait as f64 / 1_000_000.0,
                prepare as f64 / 1_000_000.0,
                percentile(&total, 50),
                percentile(&total, 95),
                percentile(&total, 100),
                percentile(&wait, 50),
                percentile(&wait, 95),
                percentile(&wait, 100),
                percentile(&queue, 50),
                percentile(&queue, 95),
                percentile(&queue, 100),
                percentile(&present, 50),
                percentile(&present, 95),
                percentile(&present, 100),
            );
        }
        self.write_json(&json!({
            "kind": "summary",
            "frames": count,
            "window_frames": self.samples.len(),
            "wall_us": elapsed_us,
            "fps": fps,
            "wire_bytes": self.total_wire_bytes,
            "wire_mib_s": mib_s,
            "source_wait_us": source_wait,
            "prepare_us": prepare,
            "producer_total_us": distribution_json(&total),
            "result_wait_us": distribution_json(&wait),
            "receiver_queue_us": distribution_json(&queue),
            "receiver_present_us": distribution_json(&present),
            "physical_ink_settle_us": null,
        }))?;
        if let Some(output) = self.jsonl.as_mut() {
            output.flush()?;
        }
        Ok(())
    }

    fn write_json(&mut self, value: &serde_json::Value) -> Result<()> {
        if let Some(output) = self.jsonl.as_mut() {
            serde_json::to_writer(&mut *output, value)?;
            output.write_all(b"\n")?;
        }
        Ok(())
    }
}

fn receiver_json(metrics: &FrameMetrics) -> serde_json::Value {
    json!({
        "decode_us": metrics.decode_us,
        "queue_us": metrics.queue_us,
        "compose_us": metrics.compose_us,
        "native_convert_us": metrics.convert_us,
        "backend_submit_us": metrics.submit_us,
        "present_us": metrics.present_us,
        "damage_pixels": metrics.damage_pixels,
        "damage_regions": metrics.damage_regions,
        "waveform": metrics.waveform,
        "complete_refresh": metrics.complete_refresh,
        "full_refresh_reason": metrics.full_refresh_reason,
    })
}

fn values(samples: &VecDeque<Sample>, field: impl Fn(&Sample) -> u64) -> Vec<u64> {
    let mut values: Vec<_> = samples.iter().map(field).collect();
    values.sort_unstable();
    values
}

const SUMMARY_WINDOW: usize = 10_000;

fn percentile(values: &[u64], percentile: usize) -> u64 {
    let index = values
        .len()
        .saturating_mul(percentile)
        .div_ceil(100)
        .saturating_sub(1)
        .min(values.len() - 1);
    values[index]
}

fn distribution_json(values: &[u64]) -> serde_json::Value {
    json!({
        "p50": percentile(values, 50),
        "p95": percentile(values, 95),
        "max": percentile(values, 100),
    })
}

#[cfg(test)]
mod tests {
    use super::percentile;

    #[test]
    fn percentile_uses_nearest_rank() {
        let values = [10, 20, 30];
        assert_eq!(percentile(&values, 50), 20);
        assert_eq!(percentile(&values, 95), 30);
        assert_eq!(percentile(&values, 100), 30);
    }
}
