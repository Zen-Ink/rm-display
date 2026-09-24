//! Bounded evdev reader, independent of panel refresh and network writes.
use crate::evdev::{
    monotonic_now, use_monotonic_clock, EvdevPenDevice, EvdevTouchDevice, PhysicalPointerEvent,
};
use rm_display_protocol::PointerRecord;
use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};
use std::time::Duration;

const CAPACITY: usize = 8192;
#[derive(Debug)]
pub enum Report {
    Touch(Vec<PhysicalPointerEvent>),
    Pen(Vec<PointerRecord>),
    Fault(String),
}
#[derive(Debug)]
pub struct CapturedReport {
    pub time: Duration,
    pub report: Report,
}
pub struct InputCapture {
    wake: OwnedFd,
    stop: AtomicBool,
    failed: AtomicBool,
    queue: Mutex<VecDeque<CapturedReport>>,
}
impl InputCapture {
    pub fn new() -> io::Result<Self> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            wake: unsafe { OwnedFd::from_raw_fd(fd) },
            stop: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            queue: Mutex::new(VecDeque::new()),
        })
    }
    pub fn event_fd(&self) -> i32 {
        self.wake.as_raw_fd()
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
    pub(crate) fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
    fn signal(&self) {
        let value = 1_u64;
        unsafe { libc::write(self.event_fd(), (&value as *const u64).cast(), 8) };
    }
    // The extra reserved fault entry makes overflow explicit without removing any accepted edge.
    pub fn push(&self, report: CapturedReport) -> bool {
        let mut queue = self.queue.lock().unwrap();
        if self.stop.load(Ordering::Acquire) {
            return false;
        }
        if queue.len() == CAPACITY {
            self.failed.store(true, Ordering::Release);
            queue.push_back(CapturedReport { time: report.time, report: Report::Fault("input queue exhausted; input disabled, existing ink retained; submit via host then reconnect".into()) });
            self.stop();
            self.signal();
            return false;
        }
        queue.push_back(report);
        self.signal();
        true
    }
    pub fn drain(&self) -> VecDeque<CapturedReport> {
        // Lock across notification reset and drain so a new report cannot lose its wakeup.
        let mut queue = self.queue.lock().unwrap();
        let mut value = 0_u64;
        unsafe { libc::read(self.event_fd(), (&mut value as *mut u64).cast(), 8) };
        std::mem::take(&mut *queue)
    }
    pub(crate) fn run(
        &self,
        mut touch: Option<&mut EvdevTouchDevice>,
        mut pen: Option<&mut EvdevPenDevice>,
        origin: Duration,
    ) {
        let result = (|| -> io::Result<()> {
            for fd in [
                touch.as_ref().map(|d| d.event_fd()),
                pen.as_ref().map(|d| d.event_fd()),
            ]
            .into_iter()
            .flatten()
            {
                use_monotonic_clock(fd)?;
            }
            // No prior connection's contact may leak into the new session.
            if let Some(device) = pen.as_deref_mut() {
                device.drain_timed_reports()?;
                device.cancel();
            }
            if let Some(device) = touch.as_deref_mut() {
                device.drain_timed_reports()?;
                device.reset_contacts();
            }
            if pen.is_none() && touch.is_none() {
                return Ok(());
            }
            let mut pending = Vec::new();
            while !self.stop.load(Ordering::Acquire) {
                let watermark = monotonic_now();
                if let Some(device) = pen.as_deref_mut() {
                    pending.extend(device.drain_timed_reports()?.into_iter().map(
                        |(time, records)| CapturedReport {
                            time,
                            report: Report::Pen(records),
                        },
                    ));
                }
                if let Some(device) = touch.as_deref_mut() {
                    pending.extend(device.drain_timed_reports()?.into_iter().map(
                        |(time, records)| CapturedReport {
                            time,
                            report: Report::Touch(records),
                        },
                    ));
                }
                if pending.len() > CAPACITY {
                    return Err(io::Error::other("input capture ordering buffer exhausted"));
                }
                // Records arriving during the two reads wait for the next drain. That
                // prevents a newer touch report overtaking a pen report not read yet.
                pending.sort_by_key(|report| report.time);
                let ready = pending.partition_point(|report| report.time <= watermark);
                for mut report in pending.drain(..ready) {
                    if report.time < origin {
                        continue;
                    }
                    report.time = report.time.saturating_sub(origin);
                    if !self.push(report) {
                        return Ok(());
                    }
                }
                let mut fds: Vec<_> = [
                    touch.as_ref().map(|d| d.event_fd()),
                    pen.as_ref().map(|d| d.event_fd()),
                ]
                .into_iter()
                .flatten()
                .map(|fd| libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                })
                .collect();
                let result = unsafe {
                    libc::poll(
                        fds.as_mut_ptr(),
                        fds.len() as _,
                        if pending.is_empty() { 5 } else { 0 },
                    )
                };
                if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.failed.store(true, Ordering::Release);
            self.push(CapturedReport {
                time: monotonic_now().saturating_sub(origin),
                report: Report::Fault(error.to_string()),
            });
            self.stop();
        }
    }
}
