//! Linux discovery and the sole worker's device-reading loop.
use crate::{
    Gesture,
    input::{
        self, Assembly, Axis, Contact, ContactId, Device, Frame, FrameAssembler, RawEvent, Source,
    },
    replay::{self, Observation, Record, Replay},
};
use evdev::{InputEvent, KeyCode, raw_stream::RawDevice};
use nix::{
    libc,
    poll::{PollFd, PollFlags, PollTimeout, poll},
    time::{ClockId, clock_gettime},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::{AsFd, AsRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

nix::ioctl_write_ptr!(set_clock_id, b'E', 0xa0, libc::c_int);
nix::ioctl_read_buf!(get_mt_slots, b'E', 0x0a, libc::c_int);

fn open(path: &Path) -> io::Result<RawDevice> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    RawDevice::from_fd(file.into())
}

use crate::diagnostics::Entry;

pub fn inventory() -> io::Result<Vec<Entry>> {
    let mut paths = Vec::new();
    match fs::read_dir("/dev/input") {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if entry.file_name().to_str().is_some_and(|name| {
                    name.strip_prefix("event").is_some_and(|number| {
                        !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
                    })
                }) {
                    paths.push(entry.path());
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    paths.sort();
    let entries: Vec<_> = paths
        .into_iter()
        .map(
            |path| match open(&path).and_then(|device| describe(&path, &device, 1)) {
                Ok(device) => Entry {
                    path,
                    available: true,
                    device: Some(device),
                    error: None,
                },
                Err(error) => Entry {
                    path,
                    available: false,
                    device: None,
                    error: Some(error.to_string()),
                },
            },
        )
        .collect();
    Ok(entries)
}

pub fn list(output: &mut impl Write, json: bool) -> io::Result<()> {
    let entries = inventory()?;
    if json {
        serde_json::to_writer(&mut *output, &entries).map_err(io::Error::other)?;
        writeln!(output)?;
    } else {
        if entries.is_empty() {
            writeln!(output, "No event devices found in /dev/input.")?;
        }
        for entry in entries {
            if let Some(device) = entry.device {
                writeln!(
                    output,
                    "{}  {:?}  identity={} seat={}{}",
                    entry.path.display(),
                    device.name,
                    device.identity,
                    device.source.seat,
                    if device.virtual_device {
                        " virtual"
                    } else {
                        ""
                    }
                )?;
            } else {
                writeln!(
                    output,
                    "{}  unavailable: {}",
                    entry.path.display(),
                    entry.error.unwrap_or_default()
                )?;
            }
        }
    }
    Ok(())
}

/// Read processed udev properties, without a subprocess or libudev dependency.
fn properties(path: &Path) -> BTreeMap<String, String> {
    let Ok(metadata) = fs::metadata(path) else {
        return BTreeMap::new();
    };
    let number = metadata.rdev();
    let database = format!(
        "/run/udev/data/c{}:{}",
        libc::major(number),
        libc::minor(number)
    );
    fs::read_to_string(database)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (key, value) = line.strip_prefix("E:")?.split_once('=')?;
            Some((key.to_owned(), value.to_owned()))
        })
        .collect()
}

pub fn describe(path: &Path, raw: &RawDevice, generation: u64) -> io::Result<Device> {
    let props = properties(path);
    let name = raw.name().unwrap_or("unnamed").to_owned();
    let physical = raw.physical_path().unwrap_or_default().to_owned();
    let unique = raw.unique_name().unwrap_or_default().to_owned();
    let id = raw.input_id();
    let location = if !unique.is_empty() {
        format!("unique:{unique}:{physical}")
    } else if let Some(serial) = props.get("ID_SERIAL").filter(|value| !value.is_empty()) {
        format!("serial:{serial}:{physical}")
    } else if let Some(path) = props.get("ID_PATH").filter(|value| !value.is_empty()) {
        format!("path:{path}:{physical}")
    } else if !physical.is_empty() {
        format!("physical:{physical}")
    } else {
        format!("ambiguous:{name}")
    };
    let identity = format!(
        "{:04x}:{:04x}:{:04x}:{location}",
        id.bus_type().0,
        id.vendor(),
        id.product()
    );
    let seat = props
        .get("ID_SEAT")
        .cloned()
        .unwrap_or_else(|| "seat0".into());
    let mut capabilities = BTreeMap::new();
    macro_rules! codes {
        ($kind:expr, $accessor:ident) => {
            if let Some(set) = raw.$accessor() {
                capabilities.insert($kind, set.iter().map(|code| code.0).collect());
            }
        };
    }
    capabilities.insert(input::EV_SYN, vec![input::SYN_REPORT]);
    codes!(input::EV_KEY, supported_keys);
    codes!(input::EV_REL, supported_relative_axes);
    codes!(input::EV_ABS, supported_absolute_axes);
    codes!(input::EV_SW, supported_switches);
    codes!(4, misc_properties);
    codes!(17, supported_leds);
    codes!(18, supported_sounds);
    codes!(21, supported_ff);
    if raw.supported_events().contains(evdev::EventType::REPEAT) {
        capabilities.insert(20, vec![0, 1]);
    }
    let axes = raw
        .get_absinfo()?
        .map(|(code, info)| {
            (
                code.0,
                Axis {
                    minimum: info.minimum(),
                    maximum: info.maximum(),
                    resolution: info.resolution(),
                    fuzz: info.fuzz(),
                    flat: info.flat(),
                },
            )
        })
        .collect();
    let virtual_device = props.get("PREK_VIRTUAL").is_some_and(|value| value == "1")
        || physical.starts_with("prek/")
        || name.starts_with("Prek virtual ");
    Ok(Device {
        source: Source {
            device: input::identity_number(&identity),
            generation,
            seat,
        },
        identity,
        name,
        path: path.display().to_string(),
        physical,
        unique,
        vendor: id.vendor(),
        product: id.product(),
        properties: raw.properties().iter().map(|property| property.0).collect(),
        capabilities,
        axes,
        virtual_device,
    })
}

/// Query persistent values, including every inactive Type B slot. The caller
/// owns this descriptor and must arrange its input handoff/reconciliation.
pub fn absolute_state(raw: &RawDevice, device: &Device) -> io::Result<input::AbsoluteState> {
    let mut state = input::AbsoluteState::default();
    if device.axes.is_empty() {
        return Ok(state);
    }
    let values = raw.get_abs_state()?;
    for &code in device.axes.keys() {
        if code > 0x3f {
            return Err(io::Error::other("unsupported absolute axis"));
        }
        if code == input::ABS_MT_TRACKING_ID {
            continue;
        }
        if input::per_slot(code) {
            let capacity = device
                .touch_capacity()
                .ok_or_else(|| io::Error::other("invalid multi-touch capacity"))?;
            if device.axes[&input::ABS_MT_SLOT].minimum != 0 {
                return Err(io::Error::other(
                    "kernel multi-touch snapshots require zero-based slots",
                ));
            }
            let mut slots = vec![0; capacity + 1];
            slots[0] = i32::from(code);
            // SAFETY: owned evdev fd; the bounded i32 buffer encodes its axis
            // followed by exactly the supported number of kernel slot values.
            unsafe { get_mt_slots(raw.as_raw_fd(), &mut slots) }.map_err(io::Error::from)?;
            for (slot, &value) in slots[1..].iter().enumerate() {
                state
                    .slots
                    .entry(slot as u16)
                    .or_default()
                    .values
                    .insert(code, value);
            }
        } else {
            state.values.insert(code, values[usize::from(code)].value);
        }
    }
    state.validate(&device.axes).map_err(io::Error::other)?;
    Ok(state)
}

pub(crate) fn initial_contacts(raw: &RawDevice, device: &Device) -> io::Result<Vec<Contact>> {
    let Some(capacity) = device.touch_capacity() else {
        return Ok(Vec::new());
    };
    if device.axes[&input::ABS_MT_SLOT].minimum != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "multi-touch slots must start at zero for kernel reconciliation",
        ));
    }
    let query = |code: u16| -> io::Result<Vec<i32>> {
        let mut values = vec![0; capacity + 1];
        values[0] = i32::from(code);
        // SAFETY: the encoded bounded i32 buffer size includes the requested
        // axis in the first element and space for every supported slot.
        unsafe { get_mt_slots(raw.as_raw_fd(), &mut values) }.map_err(io::Error::from)?;
        Ok(values[1..].to_vec())
    };
    let ids = query(input::ABS_MT_TRACKING_ID)?;
    if ids.iter().all(|&id| id < 0) {
        return Ok(Vec::new());
    }
    let xs = query(input::ABS_MT_POSITION_X)?;
    let ys = query(input::ABS_MT_POSITION_Y)?;
    Ok(ids
        .into_iter()
        .enumerate()
        .filter(|(_, id)| *id >= 0)
        .map(|(slot, tracking)| Contact {
            id: ContactId {
                slot: slot as u16,
                tracking,
                generation: slot as u64 + 1,
            },
            x: xs[slot],
            y: ys[slot],
            pressure: None,
            fresh: false,
        })
        .collect())
}

struct Reader {
    raw: RawDevice,
    device: Device,
    assembler: FrameAssembler,
}
impl Reader {
    fn new(path: &Path, generation: u64) -> io::Result<(Self, Record)> {
        let raw = open(path)?;
        let clock = libc::CLOCK_MONOTONIC;
        // SAFETY: the owned descriptor and initialized c_int pointer match
        // EVIOCSCLOCKID; this changes only our own reader's clock.
        unsafe { set_clock_id(raw.as_raw_fd(), &clock) }.map_err(io::Error::from)?;
        let device = describe(path, &raw, generation)?;
        let keys = if raw.supported_keys().is_some() {
            raw.get_key_state()?.iter().map(|key| key.0).collect()
        } else {
            BTreeSet::new()
        };
        let contacts = initial_contacts(&raw, &device)?;
        let record = Record::Device {
            device: device.clone(),
            keys,
            contacts,
        };
        let assembler = FrameAssembler::new(device.source.clone());
        Ok((
            Self {
                raw,
                device,
                assembler,
            },
            record,
        ))
    }

    fn drain(&mut self) -> io::Result<Vec<Frame>> {
        let mut frames = Vec::new();
        let mut events_read = 0;
        loop {
            let events: Vec<_> = match self.raw.fetch_events() {
                Ok(events) => events.collect(),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            if events.is_empty() {
                break;
            }
            events_read += events.len();
            if events_read > 65_536 {
                return Err(io::Error::other(
                    "device backlog exceeds monitor's bounded drain limit",
                ));
            }
            for event in events {
                let at = event
                    .timestamp()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| io::Error::other("invalid monotonic input timestamp"))?;
                let event = RawEvent {
                    kind: event.event_type().0,
                    code: event.code(),
                    value: event.value(),
                };
                match self.assembler.push(event, at) {
                    Assembly::Frame(frame) => frames.push(frame),
                    Assembly::Lost => {
                        return Err(io::Error::other(
                            "input synchronization lost; this source requires reconciliation before recognition can resume",
                        ));
                    }
                    Assembly::Pending => {}
                }
            }
        }
        Ok(frames)
    }
}

pub struct ObserveOptions {
    pub gestures_only: bool,
    pub json: bool,
    pub duration: Option<Duration>,
}

pub fn watch(
    paths: &[PathBuf],
    options: ObserveOptions,
    output: &mut impl Write,
    recording: Option<&mut dyn Write>,
) -> io::Result<()> {
    let duration = options.duration;
    let mut sink = WatchSink {
        replay: Replay::default(),
        options,
        output,
        recording,
    };
    observe(paths, duration, &mut sink)
}

/// Transport shared by monitoring and configured rules. Original descriptors
/// stay in this loop; supervised ownership is available through `reader`.
pub trait InputSink {
    /// Runs only in the sole device-owning worker. A ready transport can take
    /// ownership here; the original descriptor never crosses IPC.
    fn reader(
        &mut self,
        _raw: &mut RawDevice,
        _device: &Device,
        _health: Option<&crate::supervisor::Heartbeat>,
    ) -> io::Result<ReaderDisposition> {
        Ok(ReaderDisposition::Keep)
    }
    /// Retire virtual output while the loop still owns every original fd.
    /// Cleanup must be bounded and must also tolerate a failed transport.
    fn shutdown(&mut self) {}
    fn push(&mut self, record: Record) -> io::Result<()>;
    fn deadline(&self) -> Option<Duration>;
    fn flush(&mut self) -> io::Result<()>;
    fn finished(&self) -> bool {
        false
    }
    fn service(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn poll_sources(&self) -> Vec<PollFd<'_>> {
        Vec::new()
    }
    fn device_policy(&self) -> Option<(&crate::config::Config, u64)> {
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReaderDisposition {
    Keep,
    /// Drop this reader after reporting removal, then rediscover its next
    /// source generation. Partial frames never cross the replacement boundary.
    Reopen,
}

struct ReaderOwner<'a, S: InputSink> {
    readers: Vec<Reader>,
    sink: &'a mut S,
}
impl<S: InputSink> Drop for ReaderOwner<'_, S> {
    fn drop(&mut self) {
        // The body runs before `readers` is dropped, including on errors and
        // unwinding. Virtual output retires before any original grab releases.
        self.sink.shutdown();
    }
}
struct WatchSink<'a, 'b, W: Write> {
    replay: Replay,
    options: ObserveOptions,
    output: &'a mut W,
    recording: Option<&'b mut dyn Write>,
}
impl<W: Write> InputSink for WatchSink<'_, '_, W> {
    fn push(&mut self, record: Record) -> io::Result<()> {
        self.replay.push(record.clone())?;
        if let Some(file) = self.recording.as_mut() {
            replay::write_record(file, &record)?;
        }
        for observation in self.replay.take_observations() {
            print_observation(self.output, &observation, &self.options)?;
        }
        Ok(())
    }
    fn deadline(&self) -> Option<Duration> {
        self.replay.deadline()
    }
    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = self.recording.as_mut() {
            file.flush()?;
        }
        self.output.flush()
    }
}

pub fn observe(
    paths: &[PathBuf],
    duration: Option<Duration>,
    sink: &mut impl InputSink,
) -> io::Result<()> {
    observe_loop(paths, duration, sink, None)
}

/// Supervised observation reports health from this same reader/timer loop.
/// The parent owns no event descriptors, and an idle poll still reports health.
pub fn observe_supervised(
    paths: &[PathBuf],
    duration: Option<Duration>,
    sink: &mut impl InputSink,
    health: &mut crate::supervisor::Heartbeat,
) -> io::Result<()> {
    observe_loop(paths, duration, sink, Some(health))
}

fn observe_loop(
    paths: &[PathBuf],
    duration: Option<Duration>,
    sink: &mut impl InputSink,
    mut health: Option<&mut crate::supervisor::Heartbeat>,
) -> io::Result<()> {
    if let Some(health) = health.as_mut() {
        health.progress()?;
    }
    let mut owner = ReaderOwner {
        readers: Vec::new(),
        sink,
    };
    let ReaderOwner { readers, sink } = &mut owner;
    let sink = &mut **sink;
    let mut identities = BTreeSet::new();
    let header = Record::Header {
        version: replay::FORMAT_VERSION,
    };
    sink.push(header)?;
    for path in paths {
        if let Some(health) = health.as_mut() {
            health.progress()?;
        }
        let (reader, record) = Reader::new(path, 1).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot observe {}: {error}", path.display()),
            )
        })?;
        if !identities.insert(reader.device.source.device) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "selected devices have ambiguous/duplicate identities; choose distinct stable input sources",
            ));
        }
        eprintln!(
            "Observing {} ({:?}); input still reaches applications.",
            path.display(),
            reader.device.name
        );
        sink.push(record)?;
        sink.push(Record::Absolute {
            source: reader.device.source.clone(),
            state: absolute_state(&reader.raw, &reader.device)?,
        })?;
        readers.push(reader);
    }
    if readers.is_empty() && sink.device_policy().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "select at least one device",
        ));
    }
    let stop_at = duration
        .map(|duration| monotonic().map(|start| start.saturating_add(duration)))
        .transpose()?;
    let mut pending = Vec::<Frame>::new();
    let mut generations: BTreeMap<u64, u64> = readers
        .iter()
        .map(|reader| (reader.device.source.device, 1))
        .collect();
    let mut scan_at = monotonic()?;
    let mut policy_generation = 0;
    let mut input_at = Duration::ZERO;
    loop {
        sink.service()?;
        if sink.finished() {
            return Ok(());
        }
        if let Some(health) = health.as_mut() {
            match health.progress() {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => break,
                Err(error) => return Err(error),
            }
        }
        let now = monotonic()?;
        if let Some((config, generation)) = sink.device_policy()
            && (now >= scan_at || generation != policy_generation)
        {
            let config = config.clone();
            refresh_readers(
                &config,
                readers,
                &mut generations,
                &mut pending,
                sink,
                input_at,
            )?;
            policy_generation = generation;
            scan_at = monotonic()? + Duration::from_millis(250);
        }
        sink.flush()?;
        let mut reopen = Vec::new();
        for (index, reader) in readers.iter_mut().enumerate() {
            if sink.reader(&mut reader.raw, &reader.device, health.as_deref())?
                == ReaderDisposition::Reopen
            {
                reopen.push(index);
            }
        }
        for index in reopen.into_iter().rev() {
            let source = readers[index].device.source.clone();
            pending.retain(|frame| frame.source != source);
            sink.push(Record::Remove { source, at: now })?;
            readers.remove(index);
            scan_at = now;
        }
        if stop_at.is_some_and(|stop| now >= stop) {
            break;
        }
        let health_deadline = health
            .as_ref()
            .map(|health| health.deadline())
            .transpose()?;
        let deadline = sink
            .deadline()
            .into_iter()
            .chain(stop_at)
            .chain(health_deadline)
            .chain(sink.device_policy().map(|_| scan_at))
            .min();
        let timeout = if !pending.is_empty() {
            PollTimeout::ZERO
        } else {
            deadline.map_or(PollTimeout::NONE, |deadline| {
                poll_timeout(deadline.saturating_sub(now))
            })
        };
        let flags = {
            let mut fds: Vec<_> = readers
                .iter()
                .map(|reader| PollFd::new(reader.raw.as_fd(), PollFlags::POLLIN))
                .collect();
            if let Some(health) = health.as_ref() {
                fds.push(PollFd::new(health.as_fd(), PollFlags::POLLIN));
            }
            fds.extend(sink.poll_sources());
            match poll(&mut fds, timeout) {
                Ok(_) => fds
                    .iter()
                    .map(|fd| fd.revents().unwrap_or_else(PollFlags::empty))
                    .collect::<Vec<_>>(),
                Err(nix::errno::Errno::EINTR) => continue,
                Err(error) => return Err(error.into()),
            }
        };
        // Sample before draining all sources. Later frames stay pending so
        // no source can advance timers beyond another source's unread input.
        let watermark = monotonic()?;
        input_at = watermark;
        let mut failed = Vec::new();
        for (index, reader) in readers.iter_mut().enumerate() {
            let error = if flags[index]
                .intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL)
            {
                Some(io::Error::other("device disconnected"))
            } else {
                match reader.drain() {
                    Ok(frames) => {
                        pending.extend(frames);
                        None
                    }
                    Err(error) => Some(error),
                }
            };
            if let Some(error) = error {
                eprintln!("prek: {}: {error}", reader.device.path);
                failed.push(index);
            }
        }
        if pending.len() > 65_536 {
            return Err(io::Error::other("monitor pending-frame limit exceeded"));
        }
        pending.sort_by(|a, b| (a.at, &a.source, a.sequence).cmp(&(b.at, &b.source, b.sequence)));
        let future = pending.partition_point(|frame| frame.at <= watermark);
        for frame in pending.drain(..future) {
            let record = Record::Frame { frame };
            sink.push(record)?;
            if sink.finished() {
                sink.flush()?;
                return Ok(());
            }
        }
        for index in failed.into_iter().rev() {
            let source = readers[index].device.source.clone();
            pending.retain(|frame| frame.source != source);
            let record = Record::Remove {
                source,
                at: watermark,
            };
            sink.push(record)?;
            readers.remove(index);
        }
        let tick = Record::Tick { at: watermark };
        sink.push(tick)?;
        sink.flush()?;
        if readers.is_empty() && sink.device_policy().is_none() {
            return Err(io::Error::other("no observed device remains available"));
        }
    }
    Ok(())
}

fn selected(config: &crate::config::Config, device: &Device) -> bool {
    config.policy.allowed_seats.contains(&device.source.seat)
        && config.devices.iter().any(|(id, selector)| {
            config.policy.allowed_devices.contains(id) && selector.matches(device)
        })
}

fn initialized_udev(path: &Path) -> bool {
    let Ok(stat) = fs::metadata(path) else {
        return false;
    };
    let number = stat.rdev();
    let database = format!(
        "/run/udev/data/c{}:{}",
        libc::major(number),
        libc::minor(number)
    );
    let Ok(file) = File::open(database) else {
        return false;
    };
    let Ok(stat) = file.metadata() else {
        return false;
    };
    if !stat.is_file() || stat.uid() != 0 || stat.mode() & 0o022 != 0 {
        return false;
    }
    let mut text = String::new();
    if io::Read::read_to_string(&mut io::Read::take(file, 65_537), &mut text).is_err()
        || text.len() > 65_536
    {
        return false;
    }
    // systemd's completed database publication records usec_initialized as I:.
    // https://github.com/systemd/systemd/blob/v259.3/src/libsystemd/sd-device/device-private.c
    text.lines().any(|line| {
        line.strip_prefix("I:")
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|value| value > 0)
    })
}

fn same_node(reader: &Reader) -> bool {
    let Ok(path) = fs::metadata(&reader.device.path) else {
        return false;
    };
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: retained event descriptor and correctly sized initialized output.
    (unsafe { libc::fstat(reader.raw.as_raw_fd(), &mut stat) }) == 0
        && stat.st_ino == path.ino()
        && stat.st_dev == path.dev()
        && stat.st_rdev == path.rdev()
}

fn refresh_readers(
    config: &crate::config::Config,
    readers: &mut Vec<Reader>,
    generations: &mut BTreeMap<u64, u64>,
    pending: &mut Vec<Frame>,
    sink: &mut impl InputSink,
    now: Duration,
) -> io::Result<()> {
    let entries = inventory()?;
    let mut identities = BTreeMap::new();
    let mut ambiguous = BTreeSet::new();
    for entry in &entries {
        if let Some(device) = &entry.device
            && selected(config, device)
            && initialized_udev(&entry.path)
            && identities.insert(device.source.device, device).is_some()
        {
            ambiguous.insert(device.source.device);
        }
    }
    for index in (0..readers.len()).rev() {
        let device = &readers[index].device;
        if !selected(config, device)
            || ambiguous.contains(&device.source.device)
            || !same_node(&readers[index])
            || identities.get(&device.source.device).is_none_or(|current| {
                current.path != device.path
                    || current.source.seat != device.source.seat
                    || current.identity != device.identity
                    || current.capabilities != device.capabilities
                    || current.axes != device.axes
                    || current.properties != device.properties
            })
        {
            let source = readers[index].device.source.clone();
            pending.retain(|frame| frame.source != source);
            sink.push(Record::Remove { source, at: now })?;
            readers.remove(index);
        }
    }
    for entry in entries {
        let Some(device) = entry.device else {
            continue;
        };
        if !selected(config, &device)
            || !initialized_udev(&entry.path)
            || ambiguous.contains(&device.source.device)
            || readers
                .iter()
                .any(|reader| reader.device.source.device == device.source.device)
        {
            continue;
        }
        if !generations.contains_key(&device.source.device) && generations.len() >= 4096 {
            continue;
        }
        let generation = generations
            .get(&device.source.device)
            .copied()
            .unwrap_or_default()
            .checked_add(1)
            .ok_or_else(|| io::Error::other("input device generation exhausted"))?;
        let Ok((reader, record)) = Reader::new(&entry.path, generation) else {
            continue;
        };
        // Discovery/open races cannot replace a selected identity with an
        // unselected source. Both the opened metadata and seat must agree.
        if reader.device.identity != device.identity
            || reader.device.source.device != device.source.device
            || !selected(config, &reader.device)
        {
            continue;
        }
        sink.push(record)?;
        generations.insert(reader.device.source.device, generation);
        sink.push(Record::Absolute {
            source: reader.device.source.clone(),
            state: absolute_state(&reader.raw, &reader.device)?,
        })?;
        eprintln!(
            "Observing {} ({:?}); input still reaches applications.",
            reader.device.path, reader.device.name
        );
        readers.push(reader);
    }
    Ok(())
}

pub fn monotonic() -> io::Result<Duration> {
    let now = clock_gettime(ClockId::CLOCK_MONOTONIC).map_err(io::Error::from)?;
    Ok(Duration::new(now.tv_sec() as u64, now.tv_nsec() as u32))
}

fn poll_timeout(duration: Duration) -> PollTimeout {
    let millis = duration.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
    PollTimeout::try_from(millis).expect("bounded poll timeout")
}

fn print_observation(
    output: &mut impl Write,
    observation: &Observation,
    options: &ObserveOptions,
) -> io::Result<()> {
    if options.gestures_only && matches!(observation, Observation::Input { .. }) {
        return Ok(());
    }
    if options.json {
        serde_json::to_writer(&mut *output, observation).map_err(io::Error::other)?;
        return writeln!(output);
    }
    match observation {
        Observation::Touch { event } => writeln!(
            output,
            "touch  source={}  group={}  {:?}",
            event.source.device, event.id, event.gesture
        ),
        Observation::Button {
            gesture: Gesture::Taps { button, count },
            ..
        } => writeln!(output, "gesture  {:?}  taps={count}", KeyCode(button.code)),
        Observation::Button {
            gesture: Gesture::Hold { button },
            ..
        } => writeln!(output, "gesture  {:?}  hold", KeyCode(button.code)),
        Observation::Cancelled { source, reason, .. } => {
            writeln!(output, "cancel  source={}  {reason:?}", source.device)
        }
        Observation::Input { frame } => {
            for event in &frame.raw.events {
                if event.kind == input::EV_SYN {
                    continue;
                }
                let event = InputEvent::new(event.kind, event.code, event.value);
                writeln!(
                    output,
                    "{:.6}  source={}  {:?}",
                    frame.raw.at.as_secs_f64(),
                    frame.raw.source.device,
                    event.destructure()
                )?;
            }
            Ok(())
        }
    }
}
