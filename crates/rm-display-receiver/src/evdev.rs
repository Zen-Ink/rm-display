//! Linux Type-B multitouch parsing with a host-testable event state machine.

use std::array;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::RawFd;
#[cfg(target_os = "linux")]
use std::path::Path;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const ABS_MT_SLOT: u16 = 47;
const ABS_MT_POSITION_X: u16 = 53;
const ABS_MT_POSITION_Y: u16 = 54;
const ABS_MT_TRACKING_ID: u16 = 57;
const MAX_SLOTS: usize = 16;
const KEY_POWER: u16 = 116;

#[cfg(target_os = "linux")]
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;
#[cfg(target_os = "linux")]
const EVIOCGNAME_256: libc::c_ulong = 0x8100_4506;
#[cfg(target_os = "linux")]
const EVIOCGBIT_ABS_8: libc::c_ulong = 0x8008_4523;
#[cfg(target_os = "linux")]
const EVIOCGBIT_KEY_96: libc::c_ulong = 0x8060_4521;
#[cfg(target_os = "linux")]
const EVIOCGABS_MT_X: libc::c_ulong = 0x8018_4575;
#[cfg(target_os = "linux")]
const EVIOCGABS_MT_Y: libc::c_ulong = 0x8018_4576;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TouchDeviceCandidate {
    pub path: PathBuf,
    pub name: String,
    pub has_type_b_multitouch: bool,
    pub has_valid_axes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PowerKeyCandidate {
    path: PathBuf,
    name: String,
    has_power_key: bool,
}

fn select_power_key_candidate(candidates: &[PowerKeyCandidate]) -> Result<usize, String> {
    let mut supported = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.has_power_key)
        .map(|(index, candidate)| {
            let name = candidate.name.to_ascii_lowercase();
            let score = if name.contains("gpio") || name.contains("power") {
                100
            } else if name.contains("key") || name.contains("button") {
                80
            } else {
                0
            };
            (index, score)
        })
        .collect::<Vec<_>>();
    supported.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
    let Some((index, best)) = supported.first().copied() else {
        return Err("no evdev device advertising KEY_POWER was found".into());
    };
    if supported.get(1).is_some_and(|(_, score)| *score == best) {
        return Err("multiple equally suitable KEY_POWER devices were found".into());
    }
    Ok(index)
}

impl TouchDeviceCandidate {
    fn supported(&self) -> bool {
        self.has_type_b_multitouch && self.has_valid_axes && touch_name_score(&self.name).is_some()
    }
}

/// Selects one unambiguous Type-B touchscreen. Names only rank or reject
/// devices after capability validation; an event node number is never used as
/// a hardware identity.
fn select_touch_candidate(candidates: &[TouchDeviceCandidate]) -> Result<usize, String> {
    let supported = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.supported())
        .map(|(index, candidate)| (index, touch_name_score(&candidate.name).unwrap_or(0)))
        .collect::<Vec<_>>();
    let Some(best_score) = supported.iter().map(|(_, score)| *score).max() else {
        return Err("no Type-B multitouch device with valid X/Y axes was found".into());
    };
    let best = supported
        .iter()
        .filter(|(_, score)| *score == best_score)
        .map(|(index, _)| *index)
        .collect::<Vec<_>>();
    if best.len() != 1 {
        let names = best
            .iter()
            .map(|index| {
                format!(
                    "{} ({})",
                    candidates[*index].path.display(),
                    candidates[*index].name
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "multiple equally suitable touch devices found: {names}"
        ));
    }
    Ok(best[0])
}

fn touch_name_score(name: &str) -> Option<u8> {
    let name = name.trim().to_ascii_lowercase();
    if ["pen", "stylus", "digitizer", "wacom", "eraser", "keyboard"]
        .iter()
        .any(|marker| name.contains(marker))
    {
        return None;
    }
    if name == "ti-tsc" || name.contains("remarkable") {
        return Some(100);
    }
    if name.contains("touchscreen") || name.contains("touch screen") || name.contains("touch") {
        return Some(80);
    }
    if [
        "cyttsp",
        "goodix",
        "elan",
        "raydium",
        "ilitek",
        "atmel",
        "focaltech",
    ]
    .iter()
    .any(|marker| name.contains(marker))
    {
        return Some(60);
    }
    if name.ends_with("_mt") || name.contains("-mt") || name.contains(" mt") {
        return Some(40);
    }
    // On a verified reMarkable host, a single device with the complete Type-B
    // capability set is safer and more future-proof than an eventN table.
    Some(0)
}

fn is_remarkable_machine(machine: &str) -> bool {
    let machine = machine.to_ascii_lowercase();
    machine.contains("remarkable") || machine.contains("ferrari") || machine.contains("chiappa")
}

fn has_required_abs_bits(bits: &[u8]) -> bool {
    [
        ABS_MT_SLOT,
        ABS_MT_POSITION_X,
        ABS_MT_POSITION_Y,
        ABS_MT_TRACKING_ID,
    ]
    .iter()
    .all(|code| {
        let index = *code as usize / 8;
        bits.get(index)
            .is_some_and(|byte| byte & (1 << (*code % 8)) != 0)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinuxInputEvent {
    pub event_type: u16,
    pub code: u16,
    pub value: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerPhase {
    Down,
    Move,
    Up,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalPointerEvent {
    pub phase: PointerPhase,
    pub contact_id: u32,
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct TouchGestureResult {
    pub forward: Vec<PhysicalPointerEvent>,
    pub cancel_forwarded: Vec<PhysicalPointerEvent>,
    pub cleanup_requested: bool,
}

/// Recognizes a receiver-local five-finger chord from synchronized Type-B
/// reports. It triggers once when the active contact count reaches five,
/// cancels contacts already forwarded to the producer, and consumes the
/// gesture until every contact has left the panel.
#[derive(Debug, Default)]
pub struct FiveFingerCleanupGesture {
    active: BTreeMap<u32, (u32, u32)>,
    forwarded: BTreeMap<u32, (u32, u32)>,
    suppress_until_clear: bool,
}

impl FiveFingerCleanupGesture {
    /// Detach producer-visible contacts from an old surface without losing
    /// physical contact state. Any contact still down suppresses reports until
    /// all contacts are released, so MOVE/UP cannot leak into a new surface.
    pub fn surface_transition(&mut self) {
        self.forwarded.clear();
        if !self.active.is_empty() {
            self.suppress_until_clear = true;
        }
    }

    pub fn process(
        &mut self,
        report: Vec<PhysicalPointerEvent>,
        forward_enabled: bool,
        cleanup_enabled: bool,
    ) -> TouchGestureResult {
        for event in &report {
            match event.phase {
                PointerPhase::Down | PointerPhase::Move => {
                    self.active.insert(event.contact_id, (event.x, event.y));
                }
                PointerPhase::Up | PointerPhase::Cancel => {
                    self.active.remove(&event.contact_id);
                }
            }
        }

        if self.suppress_until_clear {
            if self.active.is_empty() {
                self.suppress_until_clear = false;
                self.forwarded.clear();
            }
            return TouchGestureResult::default();
        }

        if self.active.len() >= 5 {
            self.suppress_until_clear = true;
            let cancel_forwarded = self
                .forwarded
                .iter()
                .map(|(&contact_id, &(old_x, old_y))| {
                    let (x, y) = self
                        .active
                        .get(&contact_id)
                        .copied()
                        .unwrap_or((old_x, old_y));
                    PhysicalPointerEvent {
                        phase: PointerPhase::Cancel,
                        contact_id,
                        x,
                        y,
                    }
                })
                .collect();
            self.forwarded.clear();
            return TouchGestureResult {
                forward: Vec::new(),
                cancel_forwarded,
                cleanup_requested: cleanup_enabled,
            };
        }

        if !forward_enabled {
            return TouchGestureResult::default();
        }
        for event in &report {
            match event.phase {
                PointerPhase::Down | PointerPhase::Move => {
                    self.forwarded.insert(event.contact_id, (event.x, event.y));
                }
                PointerPhase::Up | PointerPhase::Cancel => {
                    self.forwarded.remove(&event.contact_id);
                }
            }
        }
        TouchGestureResult {
            forward: report,
            cancel_forwarded: Vec::new(),
            cleanup_requested: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    contact_id: Option<u32>,
    x: Option<i32>,
    y: Option<i32>,
    pending_down: bool,
    pending_up: bool,
    changed: bool,
}

pub struct TypeBParser {
    slots: [Slot; MAX_SLOTS],
    current_slot: usize,
    next_contact_id: u32,
    raw_min_x: i32,
    raw_max_x: i32,
    raw_min_y: i32,
    raw_max_y: i32,
    panel_width: u32,
    panel_height: u32,
    dropped: bool,
}

impl TypeBParser {
    pub fn new(raw_max_x: i32, raw_max_y: i32, panel_width: u32, panel_height: u32) -> Self {
        Self::with_ranges(0, raw_max_x, 0, raw_max_y, panel_width, panel_height)
    }

    pub fn with_ranges(
        raw_min_x: i32,
        raw_max_x: i32,
        raw_min_y: i32,
        raw_max_y: i32,
        panel_width: u32,
        panel_height: u32,
    ) -> Self {
        Self {
            slots: array::from_fn(|_| Slot::default()),
            current_slot: 0,
            next_contact_id: 1,
            raw_min_x,
            raw_max_x: raw_max_x.max(raw_min_x.saturating_add(1)),
            raw_min_y,
            raw_max_y: raw_max_y.max(raw_min_y.saturating_add(1)),
            panel_width: panel_width.max(1),
            panel_height: panel_height.max(1),
            dropped: false,
        }
    }

    pub fn push(&mut self, event: LinuxInputEvent) -> Vec<PhysicalPointerEvent> {
        match (event.event_type, event.code) {
            (EV_SYN, SYN_DROPPED) => {
                self.dropped = true;
                Vec::new()
            }
            (EV_ABS, ABS_MT_SLOT) => {
                self.current_slot = (event.value.max(0) as usize).min(MAX_SLOTS - 1);
                Vec::new()
            }
            (EV_ABS, ABS_MT_TRACKING_ID) if event.value >= 0 => {
                let contact_id = self.allocate_contact_id();
                let slot = &mut self.slots[self.current_slot];
                *slot = Slot {
                    contact_id: Some(contact_id),
                    pending_down: true,
                    ..Slot::default()
                };
                Vec::new()
            }
            (EV_ABS, ABS_MT_TRACKING_ID) => {
                if self.slots[self.current_slot].contact_id.is_some() {
                    self.slots[self.current_slot].pending_up = true;
                }
                Vec::new()
            }
            (EV_ABS, ABS_MT_POSITION_X) => {
                let slot = &mut self.slots[self.current_slot];
                slot.x = Some(event.value);
                slot.changed = true;
                Vec::new()
            }
            (EV_ABS, ABS_MT_POSITION_Y) => {
                let slot = &mut self.slots[self.current_slot];
                slot.y = Some(event.value);
                slot.changed = true;
                Vec::new()
            }
            (EV_SYN, SYN_REPORT) => self.finish_report(),
            _ => Vec::new(),
        }
    }

    fn allocate_contact_id(&mut self) -> u32 {
        let id = self.next_contact_id.max(1);
        self.next_contact_id = id.wrapping_add(1).max(1);
        id
    }

    fn finish_report(&mut self) -> Vec<PhysicalPointerEvent> {
        if self.dropped {
            self.dropped = false;
            let mut cancelled = Vec::new();
            for slot in &mut self.slots {
                if let (Some(contact_id), Some(x), Some(y)) = (slot.contact_id, slot.x, slot.y) {
                    cancelled.push(map_event(
                        PointerPhase::Cancel,
                        contact_id,
                        x,
                        y,
                        self.raw_min_x,
                        self.raw_max_x,
                        self.raw_min_y,
                        self.raw_max_y,
                        self.panel_width,
                        self.panel_height,
                    ));
                }
                *slot = Slot::default();
            }
            return cancelled;
        }

        let mut output = Vec::new();
        for slot in &mut self.slots {
            let (Some(contact_id), Some(x), Some(y)) = (slot.contact_id, slot.x, slot.y) else {
                if slot.pending_up {
                    *slot = Slot::default();
                }
                continue;
            };
            let phase = if slot.pending_down {
                PointerPhase::Down
            } else if slot.pending_up {
                PointerPhase::Up
            } else if slot.changed {
                PointerPhase::Move
            } else {
                continue;
            };
            output.push(map_event(
                phase,
                contact_id,
                x,
                y,
                self.raw_min_x,
                self.raw_max_x,
                self.raw_min_y,
                self.raw_max_y,
                self.panel_width,
                self.panel_height,
            ));
            if slot.pending_up {
                *slot = Slot::default();
            } else {
                slot.pending_down = false;
                slot.changed = false;
            }
        }
        output
    }
}

#[allow(clippy::too_many_arguments)]
fn map_event(
    phase: PointerPhase,
    contact_id: u32,
    raw_x: i32,
    raw_y: i32,
    raw_min_x: i32,
    raw_max_x: i32,
    raw_min_y: i32,
    raw_max_y: i32,
    panel_width: u32,
    panel_height: u32,
) -> PhysicalPointerEvent {
    let x = (raw_x.clamp(raw_min_x, raw_max_x) - raw_min_x) as i64 * (panel_width - 1) as i64
        / (raw_max_x - raw_min_x) as i64;
    let y = (raw_y.clamp(raw_min_y, raw_max_y) - raw_min_y) as i64 * (panel_height - 1) as i64
        / (raw_max_y - raw_min_y) as i64;
    PhysicalPointerEvent {
        phase,
        contact_id,
        x: x as u32,
        y: y as u32,
    }
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct AxisRange {
    minimum: i32,
    maximum: i32,
}

#[cfg(target_os = "linux")]
pub fn discover_remarkable_touch_device() -> Result<TouchDeviceCandidate, String> {
    let machine = std::fs::read_to_string("/sys/devices/soc0/machine")
        .map_err(|error| format!("cannot identify reMarkable hardware: {error}"))?;
    if !is_remarkable_machine(&machine) {
        return Err(format!(
            "automatic touch discovery is disabled on non-reMarkable machine {:?}",
            machine.trim()
        ));
    }

    let entries = std::fs::read_dir("/dev/input")
        .map_err(|error| format!("cannot scan /dev/input: {error}"))?;
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("event"))
                .is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                })
        })
        .collect::<Vec<_>>();
    paths.sort();

    let mut candidates = Vec::new();
    let mut failures = Vec::new();
    for path in paths {
        match inspect_touch_candidate(&path) {
            Ok(candidate) => candidates.push(candidate),
            Err(error) => failures.push(format!("{}: {error}", path.display())),
        }
    }
    let index = select_touch_candidate(&candidates).map_err(|reason| {
        if failures.is_empty() {
            reason
        } else {
            format!(
                "{reason}; {} event nodes could not be inspected",
                failures.len()
            )
        }
    })?;
    Ok(candidates.swap_remove(index))
}

#[cfg(target_os = "linux")]
fn inspect_touch_candidate(path: &Path) -> io::Result<TouchDeviceCandidate> {
    let fd = open_event_node(path)?;
    let candidate = TouchDeviceCandidate {
        path: path.to_path_buf(),
        name: query_device_name(fd),
        has_type_b_multitouch: has_type_b_multitouch(fd),
        has_valid_axes: query_axis_range(fd, EVIOCGABS_MT_X).is_some()
            && query_axis_range(fd, EVIOCGABS_MT_Y).is_some(),
    };
    unsafe { libc::close(fd) };
    Ok(candidate)
}

#[cfg(target_os = "linux")]
fn open_event_node(path: &Path) -> io::Result<RawFd> {
    let path = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "input path contains NUL"))?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}

#[cfg(target_os = "linux")]
fn query_device_name(fd: RawFd) -> String {
    let mut name = [0_u8; 256];
    if unsafe { libc::ioctl(fd, EVIOCGNAME_256, name.as_mut_ptr()) } < 0 {
        return "unknown".into();
    }
    let length = name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(name.len());
    String::from_utf8_lossy(&name[..length]).trim().to_owned()
}

#[cfg(target_os = "linux")]
fn has_type_b_multitouch(fd: RawFd) -> bool {
    let mut bits = [0_u8; 8];
    if unsafe { libc::ioctl(fd, EVIOCGBIT_ABS_8, bits.as_mut_ptr()) } < 0 {
        return false;
    }
    has_required_abs_bits(&bits)
}

#[cfg(target_os = "linux")]
fn has_power_key(fd: RawFd) -> bool {
    let mut bits = [0_u8; 96];
    if unsafe { libc::ioctl(fd, EVIOCGBIT_KEY_96, bits.as_mut_ptr()) } < 0 {
        return false;
    }
    let index = KEY_POWER as usize / 8;
    bits.get(index)
        .is_some_and(|byte| byte & (1 << (KEY_POWER % 8)) != 0)
}

/// Event-driven KEY_POWER source. A blocking poll thread sleeps inside the
/// kernel and sends exactly one notification for each key-down event. There is
/// no timer, sysfs scan, or repeated key-state query.
#[cfg(target_os = "linux")]
pub struct PowerKeyDevice {
    event_fd: RawFd,
    stop_fd: RawFd,
    thread: Option<std::thread::JoinHandle<()>>,
    path: PathBuf,
    name: String,
}

#[cfg(target_os = "linux")]
impl PowerKeyDevice {
    pub fn discover_open() -> Result<Self, String> {
        let machine = std::fs::read_to_string("/sys/devices/soc0/machine")
            .map_err(|error| format!("cannot identify reMarkable hardware: {error}"))?;
        if !is_remarkable_machine(&machine) {
            return Err("automatic power-key discovery is limited to reMarkable hardware".into());
        }
        let entries = std::fs::read_dir("/dev/input")
            .map_err(|error| format!("cannot scan /dev/input: {error}"))?;
        let mut candidates = Vec::new();
        for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
            let is_event = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("event"))
                .is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                });
            if !is_event {
                continue;
            }
            if let Ok(fd) = open_event_node(&path) {
                candidates.push(PowerKeyCandidate {
                    path,
                    name: query_device_name(fd),
                    has_power_key: has_power_key(fd),
                });
                unsafe { libc::close(fd) };
            }
        }
        let index = select_power_key_candidate(&candidates)?;
        let candidate = candidates.swap_remove(index);
        Self::open(candidate.path, candidate.name).map_err(|error| error.to_string())
    }

    fn open(path: PathBuf, name: String) -> io::Result<Self> {
        let fd = open_event_node(&path)?;
        if unsafe { libc::ioctl(fd, EVIOCGRAB, 1_i32) } != 0 {
            let error = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(error);
        }
        let stop_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if stop_fd < 0 {
            let error = io::Error::last_os_error();
            unsafe {
                libc::ioctl(fd, EVIOCGRAB, 0_i32);
                libc::close(fd);
            }
            return Err(error);
        }
        let event_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if event_fd < 0 {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(stop_fd);
                libc::ioctl(fd, EVIOCGRAB, 0_i32);
                libc::close(fd);
            }
            return Err(error);
        }
        let thread = match std::thread::Builder::new()
            .name("rm-display-power-key".into())
            .spawn(move || power_key_loop(fd, stop_fd, event_fd))
        {
            Ok(thread) => thread,
            Err(error) => {
                unsafe {
                    libc::ioctl(fd, EVIOCGRAB, 0_i32);
                    libc::close(fd);
                    libc::close(stop_fd);
                    libc::close(event_fd);
                }
                return Err(error);
            }
        };
        Ok(Self {
            event_fd,
            stop_fd,
            thread: Some(thread),
            path,
            name,
        })
    }

    pub fn drain_presses(&self) -> usize {
        let mut count = 0_u64;
        loop {
            let mut value = 0_u64;
            let read = unsafe {
                libc::read(
                    self.event_fd,
                    (&mut value as *mut u64).cast(),
                    std::mem::size_of::<u64>(),
                )
            };
            if read == std::mem::size_of::<u64>() as isize {
                count = count.saturating_add(value);
                continue;
            }
            if read < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        count.min(usize::MAX as u64) as usize
    }

    pub(crate) fn event_fd(&self) -> RawFd {
        self.event_fd
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(target_os = "linux")]
fn power_key_loop(fd: RawFd, stop_fd: RawFd, event_fd: RawFd) {
    let mut poll_fds = [
        libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: stop_fd,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let mut buffer = [0_u8; 24 * 16];
    'running: loop {
        let result = unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as _, -1) };
        if result < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if poll_fds[1].revents & libc::POLLIN != 0 {
            break;
        }
        if poll_fds[0].revents & libc::POLLIN == 0 {
            continue;
        }
        loop {
            let count = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break 'running;
            }
            if count == 0 {
                break 'running;
            }
            for event in buffer[..count as usize].chunks_exact(24) {
                let event_type = u16::from_ne_bytes(event[16..18].try_into().unwrap());
                let code = u16::from_ne_bytes(event[18..20].try_into().unwrap());
                let value = i32::from_ne_bytes(event[20..24].try_into().unwrap());
                if event_type == EV_KEY && code == KEY_POWER && value == 1 {
                    let one = 1_u64.to_ne_bytes();
                    let written = unsafe { libc::write(event_fd, one.as_ptr().cast(), one.len()) };
                    if written < 0 && io::Error::last_os_error().kind() != io::ErrorKind::WouldBlock
                    {
                        break 'running;
                    }
                }
            }
        }
    }
    unsafe {
        libc::ioctl(fd, EVIOCGRAB, 0_i32);
        libc::close(fd);
    }
}

#[cfg(target_os = "linux")]
impl Drop for PowerKeyDevice {
    fn drop(&mut self) {
        let value = 1_u64.to_ne_bytes();
        unsafe { libc::write(self.stop_fd, value.as_ptr().cast(), value.len()) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        unsafe {
            libc::close(self.stop_fd);
            libc::close(self.event_fd);
        };
    }
}

#[cfg(target_os = "linux")]
fn query_axis_range(fd: RawFd, request: libc::c_ulong) -> Option<AxisRange> {
    let mut info = InputAbsInfo::default();
    if unsafe { libc::ioctl(fd, request, &mut info as *mut InputAbsInfo) } == 0
        && info.maximum > info.minimum
    {
        Some(AxisRange {
            minimum: info.minimum,
            maximum: info.maximum,
        })
    } else {
        None
    }
}

/// Nonblocking evdev reader. Each returned inner vector corresponds to one
/// SYN_REPORT and therefore one protocol InputBatch.
#[cfg(target_os = "linux")]
pub struct EvdevTouchDevice {
    fd: RawFd,
    parser: TypeBParser,
    path: PathBuf,
    name: String,
}

#[cfg(target_os = "linux")]
impl EvdevTouchDevice {
    pub fn open(path: &Path, panel_width: u32, panel_height: u32) -> io::Result<Self> {
        let fd = open_event_node(path)?;
        let name = query_device_name(fd);
        if !has_type_b_multitouch(fd) {
            unsafe { libc::close(fd) };
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "input device lacks required Type-B multitouch capabilities",
            ));
        }
        let Some(mut x) = query_axis_range(fd, EVIOCGABS_MT_X) else {
            unsafe { libc::close(fd) };
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "input device has no valid multitouch X range",
            ));
        };
        let Some(mut y) = query_axis_range(fd, EVIOCGABS_MT_Y) else {
            unsafe { libc::close(fd) };
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "input device has no valid multitouch Y range",
            ));
        };
        if name == "ti-tsc" {
            if x.minimum == 0 && x.maximum == 4095 {
                x = AxisRange {
                    minimum: 165,
                    maximum: 4016,
                };
            }
            if y.minimum == 0 && y.maximum == 4095 {
                y = AxisRange {
                    minimum: 220,
                    maximum: 3907,
                };
            }
        }
        if unsafe { libc::ioctl(fd, EVIOCGRAB, 1_i32) } != 0 {
            let error = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(error);
        }
        Ok(Self {
            fd,
            parser: TypeBParser::with_ranges(
                x.minimum,
                x.maximum,
                y.minimum,
                y.maximum,
                panel_width,
                panel_height,
            ),
            path: path.to_path_buf(),
            name,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn event_fd(&self) -> RawFd {
        self.fd
    }

    pub fn drain_reports(&mut self) -> io::Result<Vec<Vec<PhysicalPointerEvent>>> {
        let mut reports = Vec::new();
        let mut buffer = [0_u8; 24 * 64];
        loop {
            let count = unsafe { libc::read(self.fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if count == 0 {
                break;
            }
            if count as usize % 24 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "evdev read was not aligned to 64-bit input_event",
                ));
            }
            for event in buffer[..count as usize].chunks_exact(24) {
                let output = self.parser.push(LinuxInputEvent {
                    event_type: u16::from_ne_bytes(event[16..18].try_into().unwrap()),
                    code: u16::from_ne_bytes(event[18..20].try_into().unwrap()),
                    value: i32::from_ne_bytes(event[20..24].try_into().unwrap()),
                });
                if !output.is_empty() {
                    reports.push(output);
                }
            }
        }
        Ok(reports)
    }
}

#[cfg(target_os = "linux")]
impl Drop for EvdevTouchDevice {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.fd, EVIOCGRAB, 0_i32);
            libc::close(self.fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(event_type: u16, code: u16, value: i32) -> LinuxInputEvent {
        LinuxInputEvent {
            event_type,
            code,
            value,
        }
    }

    fn candidate(path: &str, name: &str, type_b: bool, axes: bool) -> TouchDeviceCandidate {
        TouchDeviceCandidate {
            path: PathBuf::from(path),
            name: name.into(),
            has_type_b_multitouch: type_b,
            has_valid_axes: axes,
        }
    }

    fn pointer(phase: PointerPhase, contact_id: u32) -> PhysicalPointerEvent {
        PhysicalPointerEvent {
            phase,
            contact_id,
            x: contact_id * 10,
            y: contact_id * 20,
        }
    }

    #[test]
    fn selects_by_capabilities_and_name_not_event_number() {
        let candidates = vec![
            candidate("/dev/input/event1", "Wacom Pen", false, true),
            candidate("/dev/input/event93", "cyttsp5_mt", true, true),
            candidate("/dev/input/event2", "gpio-keys", false, false),
        ];
        assert_eq!(select_touch_candidate(&candidates).unwrap(), 1);
    }

    #[test]
    fn touch_name_wins_when_multiple_type_b_devices_exist() {
        let candidates = vec![
            candidate("/dev/input/event5", "generic pointer", true, true),
            candidate("/dev/input/event8", "reMarkable touchscreen", true, true),
        ];
        assert_eq!(select_touch_candidate(&candidates).unwrap(), 1);
    }

    #[test]
    fn power_key_discovery_prefers_named_gpio_device_without_event_number_table() {
        let candidates = vec![
            PowerKeyCandidate {
                path: PathBuf::from("/dev/input/event99"),
                name: "generic input".into(),
                has_power_key: true,
            },
            PowerKeyCandidate {
                path: PathBuf::from("/dev/input/event3"),
                name: "gpio-keys".into(),
                has_power_key: true,
            },
            PowerKeyCandidate {
                path: PathBuf::from("/dev/input/event1"),
                name: "touchscreen".into(),
                has_power_key: false,
            },
        ];
        assert_eq!(select_power_key_candidate(&candidates).unwrap(), 1);
    }

    #[test]
    fn power_key_discovery_rejects_ambiguous_devices() {
        let candidates = vec![
            PowerKeyCandidate {
                path: PathBuf::from("/dev/input/event1"),
                name: "power-button-a".into(),
                has_power_key: true,
            },
            PowerKeyCandidate {
                path: PathBuf::from("/dev/input/event2"),
                name: "power-button-b".into(),
                has_power_key: true,
            },
        ];
        assert!(select_power_key_candidate(&candidates).is_err());
    }

    #[test]
    fn ambiguous_equally_ranked_devices_are_not_guessed() {
        let candidates = vec![
            candidate("/dev/input/event4", "Goodix Capacitive Touch", true, true),
            candidate("/dev/input/event7", "Elan Touchscreen", true, true),
        ];
        let error = select_touch_candidate(&candidates).unwrap_err();
        assert!(error.contains("multiple equally suitable"));
    }

    #[test]
    fn pen_named_device_is_rejected_even_if_it_claims_multitouch() {
        let candidates = vec![candidate(
            "/dev/input/event0",
            "Wacom Pen Digitizer",
            true,
            true,
        )];
        assert!(select_touch_candidate(&candidates).is_err());
    }

    #[test]
    fn required_abs_capability_set_is_exact() {
        let mut bits = [0_u8; 8];
        for code in [
            ABS_MT_SLOT,
            ABS_MT_POSITION_X,
            ABS_MT_POSITION_Y,
            ABS_MT_TRACKING_ID,
        ] {
            bits[code as usize / 8] |= 1 << (code % 8);
        }
        assert!(has_required_abs_bits(&bits));
        bits[ABS_MT_TRACKING_ID as usize / 8] &= !(1 << (ABS_MT_TRACKING_ID % 8));
        assert!(!has_required_abs_bits(&bits));
    }

    #[test]
    fn auto_discovery_is_scoped_to_remarkable_machine_names() {
        assert!(is_remarkable_machine("reMarkable 2.0"));
        assert!(is_remarkable_machine("Ferrari"));
        assert!(is_remarkable_machine("imx93-chiappa"));
        assert!(!is_remarkable_machine("Generic ARM64 tablet"));
    }

    #[test]
    fn parses_type_b_down_move_up_with_private_contact_id() {
        let mut parser = TypeBParser::new(1000, 2000, 101, 201);
        parser.push(event(EV_ABS, ABS_MT_SLOT, 3));
        parser.push(event(EV_ABS, ABS_MT_TRACKING_ID, 77));
        parser.push(event(EV_ABS, ABS_MT_POSITION_X, 500));
        parser.push(event(EV_ABS, ABS_MT_POSITION_Y, 1000));
        let down = parser.push(event(EV_SYN, SYN_REPORT, 0));
        assert_eq!(down[0].phase, PointerPhase::Down);
        assert_eq!((down[0].x, down[0].y), (50, 100));
        assert_ne!(down[0].contact_id, 3);
        assert_ne!(down[0].contact_id, 77);

        parser.push(event(EV_ABS, ABS_MT_POSITION_X, 750));
        let moved = parser.push(event(EV_SYN, SYN_REPORT, 0));
        assert_eq!(moved[0].phase, PointerPhase::Move);
        assert_eq!(moved[0].contact_id, down[0].contact_id);

        parser.push(event(EV_ABS, ABS_MT_TRACKING_ID, -1));
        let up = parser.push(event(EV_SYN, SYN_REPORT, 0));
        assert_eq!(up[0].phase, PointerPhase::Up);
        assert_eq!(up[0].contact_id, down[0].contact_id);
    }

    #[test]
    fn syn_dropped_cancels_all_contacts() {
        let mut parser = TypeBParser::new(100, 100, 100, 100);
        parser.push(event(EV_ABS, ABS_MT_TRACKING_ID, 1));
        parser.push(event(EV_ABS, ABS_MT_POSITION_X, 10));
        parser.push(event(EV_ABS, ABS_MT_POSITION_Y, 20));
        parser.push(event(EV_SYN, SYN_REPORT, 0));
        parser.push(event(EV_SYN, SYN_DROPPED, 0));
        let cancelled = parser.push(event(EV_SYN, SYN_REPORT, 0));
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].phase, PointerPhase::Cancel);
    }

    #[test]
    fn maps_nonzero_raw_axis_minimum_to_panel_edges() {
        let mut parser = TypeBParser::with_ranges(100, 1100, 200, 2200, 101, 201);
        parser.push(event(EV_ABS, ABS_MT_TRACKING_ID, 1));
        parser.push(event(EV_ABS, ABS_MT_POSITION_X, 100));
        parser.push(event(EV_ABS, ABS_MT_POSITION_Y, 200));
        let minimum = parser.push(event(EV_SYN, SYN_REPORT, 0));
        assert_eq!((minimum[0].x, minimum[0].y), (0, 0));

        parser.push(event(EV_ABS, ABS_MT_POSITION_X, 1100));
        parser.push(event(EV_ABS, ABS_MT_POSITION_Y, 2200));
        let maximum = parser.push(event(EV_SYN, SYN_REPORT, 0));
        assert_eq!((maximum[0].x, maximum[0].y), (100, 200));
    }

    #[test]
    fn five_finger_chord_cancels_forwarded_contacts_and_triggers_once() {
        let mut gesture = FiveFingerCleanupGesture::default();
        let first_four = (1..=4).map(|id| pointer(PointerPhase::Down, id)).collect();
        let forwarded = gesture.process(first_four, true, true);
        assert_eq!(forwarded.forward.len(), 4);
        assert!(!forwarded.cleanup_requested);

        let triggered = gesture.process(vec![pointer(PointerPhase::Down, 5)], true, true);
        assert!(triggered.cleanup_requested);
        assert_eq!(triggered.cancel_forwarded.len(), 4);
        assert!(triggered
            .cancel_forwarded
            .iter()
            .all(|event| event.phase == PointerPhase::Cancel));

        assert_eq!(
            gesture.process(vec![pointer(PointerPhase::Move, 1)], true, true),
            TouchGestureResult::default()
        );
        let all_up = (1..=5).map(|id| pointer(PointerPhase::Up, id)).collect();
        assert_eq!(
            gesture.process(all_up, true, true),
            TouchGestureResult::default()
        );

        let next_five = (6..=10).map(|id| pointer(PointerPhase::Down, id)).collect();
        assert!(gesture.process(next_five, true, true).cleanup_requested);
    }

    #[test]
    fn local_five_finger_cleanup_does_not_require_pointer_forwarding() {
        let mut gesture = FiveFingerCleanupGesture::default();
        let five = (1..=5).map(|id| pointer(PointerPhase::Down, id)).collect();
        let result = gesture.process(five, false, true);
        assert!(result.cleanup_requested);
        assert!(result.forward.is_empty());
        assert!(result.cancel_forwarded.is_empty());
    }

    #[test]
    fn surface_transition_suppresses_active_contacts_until_all_are_up() {
        let mut gesture = FiveFingerCleanupGesture::default();
        let first_four = (1..=4).map(|id| pointer(PointerPhase::Down, id)).collect();
        assert_eq!(gesture.process(first_four, true, true).forward.len(), 4);
        gesture.surface_transition();

        assert_eq!(
            gesture.process(vec![pointer(PointerPhase::Move, 1)], true, true),
            TouchGestureResult::default()
        );
        let all_up = (1..=4).map(|id| pointer(PointerPhase::Up, id)).collect();
        assert_eq!(
            gesture.process(all_up, true, true),
            TouchGestureResult::default()
        );
        let fresh = gesture.process(vec![pointer(PointerPhase::Down, 9)], true, true);
        assert_eq!(fresh.forward.len(), 1);
        assert_eq!(fresh.forward[0].contact_id, 9);
    }

    #[test]
    fn triggered_five_finger_suppression_survives_surface_transition() {
        let mut gesture = FiveFingerCleanupGesture::default();
        let five = (1..=5).map(|id| pointer(PointerPhase::Down, id)).collect();
        assert!(gesture.process(five, false, true).cleanup_requested);
        gesture.surface_transition();
        assert_eq!(
            gesture.process(vec![pointer(PointerPhase::Move, 5)], true, true),
            TouchGestureResult::default()
        );
        let all_up = (1..=5).map(|id| pointer(PointerPhase::Up, id)).collect();
        gesture.process(all_up, true, true);
        assert_eq!(
            gesture
                .process(vec![pointer(PointerPhase::Down, 8)], true, true)
                .forward
                .len(),
            1
        );
    }
}
