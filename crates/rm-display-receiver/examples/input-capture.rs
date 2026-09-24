//! Operational bounded-reader transport exercise; no device, deployment, or unit tests.
use rm_display_protocol::{PointerDevice, PointerPhase as WirePhase, PointerRecord};
use rm_display_receiver::evdev::{LinuxInputEvent, PointerPhase, TypeBParser};
use rm_display_receiver::input_capture::{CapturedReport, InputCapture, Report};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn main() {
    let capture = InputCapture::new().unwrap();
    let finished = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut parser = TypeBParser::new(100, 100, 101, 101);
            let pen = PointerRecord {
                device: PointerDevice::Pen as i32,
                phase: WirePhase::Cancel as i32,
                ..Default::default()
            };
            assert!(capture.push(CapturedReport {
                time: Duration::from_millis(1),
                report: Report::Pen(vec![pen])
            }));
            for (time, down) in [(10, true), (60, false), (110, true), (160, false)] {
                for slot in 0..2 {
                    parser.push(LinuxInputEvent {
                        event_type: 3,
                        code: 47,
                        value: slot,
                    });
                    parser.push(LinuxInputEvent {
                        event_type: 3,
                        code: 57,
                        value: if down { slot + 1 } else { -1 },
                    });
                    if down {
                        parser.push(LinuxInputEvent {
                            event_type: 3,
                            code: 53,
                            value: 30 + slot,
                        });
                        parser.push(LinuxInputEvent {
                            event_type: 3,
                            code: 54,
                            value: 40,
                        });
                    }
                }
                let records = parser.push(LinuxInputEvent {
                    event_type: 0,
                    code: 0,
                    value: 0,
                });
                assert_eq!(records.len(), 2);
                assert!(records.iter().all(|r| r.phase
                    == if down {
                        PointerPhase::Down
                    } else {
                        PointerPhase::Up
                    }));
                assert!(capture.push(CapturedReport {
                    time: Duration::from_millis(time),
                    report: Report::Touch(records)
                }));
            }
            finished.store(true, Ordering::Release);
        });
        // Model a slow panel/network consumer. Producer must finish independently.
        std::thread::sleep(Duration::from_millis(250));
        assert!(finished.load(Ordering::Acquire));
        let reports = capture.drain();
        assert_eq!(reports.len(), 5);
        assert!(matches!(reports[0].report, Report::Pen(_)));
        assert_eq!(
            reports
                .iter()
                .map(|r| r.time.as_millis())
                .collect::<Vec<_>>(),
            vec![1, 10, 60, 110, 160]
        );
        assert!(capture.drain().is_empty());
    });
    for _ in 0..8192 {
        assert!(capture.push(CapturedReport {
            time: Duration::from_secs(1),
            report: Report::Touch(Vec::new())
        }));
    }
    assert!(!capture.push(CapturedReport {
        time: Duration::from_secs(2),
        report: Report::Touch(Vec::new())
    }));
    let overflow = capture.drain();
    assert_eq!(overflow.len(), 8193);
    assert!(matches!(overflow.back().unwrap().report, Report::Fault(_)));
    assert!(!capture.push(CapturedReport {
        time: Duration::from_secs(3),
        report: Report::Touch(Vec::new())
    }));
    println!("stalled consumer: two rapid two-finger taps and pen cancellation retained with capture timestamps; bounded overflow fault and shutdown passed");
}
