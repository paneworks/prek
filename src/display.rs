//! Display outputs read from a Wayland compositor as an ordinary client, and
//! screen power read from DRM connectors in sysfs.
//!
//! [`outputs`] speaks the Wayland wire protocol directly: core `wl_display`,
//! `wl_registry`, `wl_callback` and `wl_output`, plus the standard
//! `zxdg_output_manager_v1`. [`screen_on`] reads a connector's `enabled` and
//! `dpms` attributes.

use nix::{
    errno::Errno,
    libc,
    poll::{PollFd, PollFlags, PollTimeout, poll},
};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read, Write},
    mem::offset_of,
    os::{
        fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    path::Path,
    time::{Duration, Instant},
};

const MAX_OUTPUTS: usize = 64;
const MAX_MESSAGE: usize = 64 * 1024;
const MAX_STRING: usize = 4096;
const MAX_ROUNDTRIPS: usize = 16;
const HEADER: usize = 8;
const READ_CHUNK: usize = 4096;
const CONNECT_RETRY: Duration = Duration::from_millis(5);

const DISPLAY: u32 = 1;
const OUTPUT_INTERFACE: &str = "wl_output";
const MANAGER_INTERFACE: &str = "zxdg_output_manager_v1";
const OUTPUT_VERSION: u32 = 4;
const XDG_VERSION: u32 = 3;
const MODE_CURRENT: u32 = 0x1;

/// One compositor output in logical and pixel coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    pub name: String,
    pub description: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub pixel_width: i32,
    pub pixel_height: i32,
    pub transform: u32,
    pub scale: i32,
}

/// Connects to the Wayland display socket at the absolute path `socket` and
/// returns every output sorted by name.
///
/// Binds each `wl_output` at version `min(advertised, 4)` and
/// `zxdg_output_manager_v1` at `min(advertised, 3)`, then repeats up to 16
/// `wl_display.sync` roundtrips until every output has reported `done`. Fails
/// with `TimedOut` when `until` passes first, and with `InvalidData` for a
/// compositor without xdg-output, more than 64 outputs, messages over 64 KiB,
/// strings over 4096 bytes, duplicate names, non-positive sizes or scales, a
/// transform above 7, or outputs still incomplete after the last roundtrip. A
/// `wl_display.error` event fails the query.
pub fn outputs(socket: &Path, until: Instant) -> io::Result<Vec<Output>> {
    Watch::open(socket, until)?.outputs(until)
}

/// A retained display connection whose outputs follow the compositor's
/// events: outputs announced later are bound, removed ones are released, and
/// object ids are reused only after the compositor deletes them.
pub struct Watch {
    client: Client,
}
impl Watch {
    /// Connects and binds every advertised output, with the limits of [`outputs`].
    pub fn open(socket: &Path, until: Instant) -> io::Result<Self> {
        let mut client = Client::new(connect(socket, until)?, until);
        client.registry = client.create(Object::Registry);
        client.queue(Message::new(DISPLAY, 1).uint(client.registry));
        client.roundtrip()?;
        client.bind()?;
        client.settle()?;
        Ok(Self { client })
    }
    /// Every output after delivering all events sent before this call, and
    /// none left unread behind the final roundtrip.
    pub fn outputs(&mut self, until: Instant) -> io::Result<Vec<Output>> {
        self.client.until = until;
        for _ in 0..MAX_ROUNDTRIPS {
            self.client.settle()?;
            self.client.changed = false;
            self.client.receive()?;
            if !self.client.changed {
                return self.client.collect();
            }
        }
        Err(invalid("compositor outputs kept changing"))
    }
    /// Delivers the events the compositor already sent, without waiting;
    /// whether an output or the output manager changed since [`Watch::outputs`].
    pub fn receive(&mut self) -> io::Result<bool> {
        self.client.receive()?;
        Ok(self.client.changed)
    }
}
impl AsFd for Watch {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.client.stream.as_fd()
    }
}

/// Kernel screen power for the DRM connector `connector` under `sysfs_drm`.
///
/// Matches exactly one `card<N>-<connector>` entry and returns `Some(true)`
/// when its `enabled` reads `enabled` and its `dpms` reads `On`,
/// `Some(false)` when it is disabled or not `On`, and `None` when no unique
/// connector exists.
pub fn screen_on_at(sysfs_drm: &Path, connector: &str) -> io::Result<Option<bool>> {
    let entries = match fs::read_dir(sysfs_drm) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut found = None;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some((card, rest)) = name.to_str().and_then(|name| name.split_once('-')) else {
            continue;
        };
        let numbered = card
            .strip_prefix("card")
            .is_some_and(|index| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()));
        if !numbered || rest != connector {
            continue;
        }
        if found.is_some() {
            return Ok(None);
        }
        found = Some(entry.path());
    }
    let Some(path) = found else {
        return Ok(None);
    };
    match attribute(&path.join("enabled"))?.as_str() {
        "enabled" => Ok(Some(attribute(&path.join("dpms"))? == "On")),
        "disabled" => Ok(Some(false)),
        other => Err(invalid(format!(
            "connector {connector} reports enabled={other:?}"
        ))),
    }
}

/// Kernel screen power for `connector` under `/sys/class/drm`.
pub fn screen_on(connector: &str) -> io::Result<Option<bool>> {
    screen_on_at(Path::new("/sys/class/drm"), connector)
}

fn attribute(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    File::open(path)?.take(256).read_to_string(&mut text)?;
    Ok(text.trim().to_owned())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "Wayland output query timed out")
}

fn poll_timeout(remaining: Duration) -> PollTimeout {
    PollTimeout::try_from(remaining.as_micros().div_ceil(1000)).unwrap_or(PollTimeout::MAX)
}

fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn connect(socket: &Path, until: Instant) -> io::Result<UnixStream> {
    let bytes = socket.as_os_str().as_bytes();
    // SAFETY: sockaddr_un is plain data and all-zero is a valid value.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if !socket.is_absolute() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Wayland socket requires a bounded absolute path",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let length = (offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    // SAFETY: no pointers; success returns a new owned socket descriptor.
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh descriptor owned by nothing else.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    loop {
        // SAFETY: initialized Unix address with exact terminated pathname length.
        let result = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                length,
            )
        };
        if result == 0 {
            return Ok(UnixStream::from(fd));
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => {}
            Some(libc::EAGAIN) => {
                let remaining = until.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(timed_out());
                }
                std::thread::sleep(remaining.min(CONNECT_RETRY));
            }
            _ => return Err(error),
        }
    }
}

/// A request or event in Wayland wire format.
struct Message {
    bytes: Vec<u8>,
}
impl Message {
    fn new(object: u32, opcode: u16) -> Self {
        let mut bytes = Vec::with_capacity(32);
        bytes.extend_from_slice(&object.to_ne_bytes());
        bytes.extend_from_slice(&u32::from(opcode).to_ne_bytes());
        Self { bytes }
    }
    fn uint(mut self, value: u32) -> Self {
        self.bytes.extend_from_slice(&value.to_ne_bytes());
        self
    }
    #[cfg(test)]
    fn int(self, value: i32) -> Self {
        self.uint(value as u32)
    }
    fn string(mut self, value: &str) -> Self {
        self = self.uint(value.len() as u32 + 1);
        self.bytes.extend_from_slice(value.as_bytes());
        self.bytes.push(0);
        self.bytes.resize(self.bytes.len().next_multiple_of(4), 0);
        self
    }
    fn finish(mut self) -> Vec<u8> {
        let header = ((self.bytes.len() as u32) << 16) | word(&self.bytes, 4);
        self.bytes[4..HEADER].copy_from_slice(&header.to_ne_bytes());
        self.bytes
    }
}

/// Sequential argument decoder over one message body.
struct Args<'a> {
    body: &'a [u8],
    offset: usize,
}
impl<'a> Args<'a> {
    fn new(body: &'a [u8]) -> Self {
        Self { body, offset: 0 }
    }
    fn uint(&mut self) -> io::Result<u32> {
        let bytes = self
            .body
            .get(self.offset..self.offset + 4)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .ok_or_else(|| invalid("truncated Wayland message"))?;
        self.offset += 4;
        Ok(u32::from_ne_bytes(bytes))
    }
    fn int(&mut self) -> io::Result<i32> {
        self.uint().map(|value| value as i32)
    }
    fn string(&mut self) -> io::Result<Option<String>> {
        let length = self.uint()? as usize;
        if length == 0 {
            return Ok(None);
        }
        if length > MAX_STRING {
            return Err(invalid("Wayland string exceeds 4096 bytes"));
        }
        let padded = length.next_multiple_of(4);
        let bytes = self
            .body
            .get(self.offset..self.offset + padded)
            .ok_or_else(|| invalid("truncated Wayland string"))?;
        self.offset += padded;
        let (text, terminator) = bytes[..length].split_at(length - 1);
        if terminator != [0] || text.contains(&0) {
            return Err(invalid("Wayland string is not NUL-terminated"));
        }
        String::from_utf8(text.to_vec())
            .map(Some)
            .map_err(|_| invalid("Wayland string is not UTF-8"))
    }
}

#[derive(Clone, Copy)]
enum Object {
    Registry,
    Callback,
    Manager,
    Output(u32),
    XdgOutput(u32),
    Released,
}

/// Properties of one bound output collected from its events.
#[derive(Clone, Default)]
struct Pending {
    output: u32,
    xdg: u32,
    version: u32,
    xdg_version: u32,
    name: Option<String>,
    xdg_name: Option<String>,
    description: Option<String>,
    xdg_description: Option<String>,
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
    mode: Option<(i32, i32)>,
    transform: Option<u32>,
    scale: Option<i32>,
    output_done: bool,
    xdg_done: bool,
}
impl Pending {
    fn complete(&self) -> bool {
        (self.version < 2 || self.output_done) && (self.xdg_version >= 3 || self.xdg_done)
    }
    fn output(self) -> io::Result<Output> {
        let name = self
            .name
            .or(self.xdg_name)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| invalid("output has no name"))?;
        let missing = |what: &str| invalid(format!("output {name} has no {what}"));
        let (x, y) = self.position.ok_or_else(|| missing("logical position"))?;
        let (width, height) = self.size.ok_or_else(|| missing("logical size"))?;
        let (pixel_width, pixel_height) = self.mode.ok_or_else(|| missing("current mode"))?;
        let transform = self.transform.ok_or_else(|| missing("geometry"))?;
        let scale = self.scale.unwrap_or(1);
        if width <= 0 || height <= 0 || pixel_width <= 0 || pixel_height <= 0 || scale <= 0 {
            return Err(invalid(format!(
                "output {name} has a non-positive size or scale"
            )));
        }
        if transform > 7 {
            return Err(invalid(format!(
                "output {name} has unknown transform {transform}"
            )));
        }
        Ok(Output {
            description: self
                .description
                .or(self.xdg_description)
                .unwrap_or_default(),
            name,
            x,
            y,
            width,
            height,
            pixel_width,
            pixel_height,
            transform,
            scale,
        })
    }
}

/// Connection state of one display client.
struct Client {
    stream: UnixStream,
    until: Instant,
    input: Vec<u8>,
    output: Vec<u8>,
    next_id: u32,
    free: Vec<u32>,
    objects: BTreeMap<u32, Object>,
    registry: u32,
    globals: Vec<(u32, u32)>,
    manager: Option<(u32, u32)>,
    bound: Option<(u32, u32)>,
    manager_lost: bool,
    pending: BTreeMap<u32, Pending>,
    synced: Option<u32>,
    changed: bool,
}
impl Client {
    fn new(stream: UnixStream, until: Instant) -> Self {
        Self {
            stream,
            until,
            input: Vec::new(),
            output: Vec::new(),
            next_id: 2,
            free: Vec::new(),
            objects: BTreeMap::new(),
            registry: 0,
            globals: Vec::new(),
            manager: None,
            bound: None,
            manager_lost: false,
            pending: BTreeMap::new(),
            synced: None,
            changed: false,
        }
    }
    fn create(&mut self, object: Object) -> u32 {
        let id = self.free.pop().unwrap_or_else(|| {
            self.next_id += 1;
            self.next_id - 1
        });
        self.objects.insert(id, object);
        id
    }
    fn queue(&mut self, message: Message) {
        self.output.extend_from_slice(&message.finish());
    }
    fn flush(&mut self) -> io::Result<()> {
        let mut written = 0;
        while written < self.output.len() {
            match self.stream.write(&self.output[written..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(count) => written += count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.wait(PollFlags::POLLOUT)?
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        self.output.clear();
        Ok(())
    }
    fn wait(&self, flags: PollFlags) -> io::Result<()> {
        let remaining = self.until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timed_out());
        }
        let mut fds = [PollFd::new(self.stream.as_fd(), flags)];
        match poll(&mut fds, poll_timeout(remaining)) {
            Ok(_) | Err(Errno::EINTR) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
    /// The next complete message already buffered.
    fn buffered(&mut self) -> io::Result<Option<(u32, u16, Vec<u8>)>> {
        if self.input.len() < HEADER {
            return Ok(None);
        }
        let header = word(&self.input, 4);
        let size = (header >> 16) as usize;
        if size < HEADER || !size.is_multiple_of(4) || size > MAX_MESSAGE {
            return Err(invalid("malformed Wayland message header"));
        }
        if self.input.len() < size {
            return Ok(None);
        }
        let object = word(&self.input, 0);
        let body = self.input[HEADER..size].to_vec();
        self.input.drain(..size);
        Ok(Some((object, header as u16, body)))
    }
    /// Reads one chunk; `false` when nothing is readable yet.
    fn read_chunk(&mut self) -> io::Result<bool> {
        let mut chunk = [0; READ_CHUNK];
        match self.stream.read(&mut chunk) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "compositor closed the connection",
            )),
            Ok(count) => {
                self.input.extend_from_slice(&chunk[..count]);
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(true),
            Err(error) => Err(error),
        }
    }
    fn message(&mut self) -> io::Result<(u32, u16, Vec<u8>)> {
        loop {
            if Instant::now() >= self.until {
                return Err(timed_out());
            }
            if let Some(message) = self.buffered()? {
                return Ok(message);
            }
            if !self.read_chunk()? {
                self.wait(PollFlags::POLLIN)?
            }
        }
    }
    /// Dispatches every message readable now without waiting.
    fn receive(&mut self) -> io::Result<()> {
        loop {
            while let Some((object, opcode, body)) = self.buffered()? {
                self.dispatch(object, opcode, &body)?;
            }
            if !self.read_chunk()? {
                return Ok(());
            }
        }
    }
    fn roundtrip(&mut self) -> io::Result<()> {
        let callback = self.create(Object::Callback);
        self.queue(Message::new(DISPLAY, 0).uint(callback));
        self.flush()?;
        while self.synced != Some(callback) {
            let (object, opcode, body) = self.message()?;
            self.dispatch(object, opcode, &body)?;
        }
        self.synced = None;
        if self.manager_lost {
            return Err(invalid("compositor removed xdg-output"));
        }
        self.flush()
    }
    fn settle(&mut self) -> io::Result<()> {
        for _ in 0..MAX_ROUNDTRIPS {
            self.roundtrip()?;
            if self.pending.values().all(Pending::complete) {
                return Ok(());
            }
        }
        Err(invalid("compositor never completed its outputs"))
    }
    fn bind(&mut self) -> io::Result<()> {
        let (global, version) = self
            .manager
            .ok_or_else(|| invalid("compositor does not offer xdg-output"))?;
        let xdg_version = version.min(XDG_VERSION);
        let manager = self.create(Object::Manager);
        self.queue(
            Message::new(self.registry, 0)
                .uint(global)
                .string(MANAGER_INTERFACE)
                .uint(xdg_version)
                .uint(manager),
        );
        self.bound = Some((manager, xdg_version));
        for (global, version) in std::mem::take(&mut self.globals) {
            self.bind_output(global, version)?;
        }
        Ok(())
    }
    fn bind_output(&mut self, global: u32, version: u32) -> io::Result<()> {
        let Some((manager, xdg_version)) = self.bound else {
            return Ok(());
        };
        if self.pending.len() >= MAX_OUTPUTS {
            return Err(invalid("compositor advertises more than 64 outputs"));
        }
        let version = version.min(OUTPUT_VERSION);
        let output = self.create(Object::Output(global));
        self.queue(
            Message::new(self.registry, 0)
                .uint(global)
                .string(OUTPUT_INTERFACE)
                .uint(version)
                .uint(output),
        );
        let xdg = self.create(Object::XdgOutput(global));
        self.queue(Message::new(manager, 1).uint(xdg).uint(output));
        self.pending.insert(
            global,
            Pending {
                output,
                xdg,
                version,
                xdg_version,
                ..Pending::default()
            },
        );
        Ok(())
    }
    fn collect(&self) -> io::Result<Vec<Output>> {
        let mut outputs = self
            .pending
            .values()
            .cloned()
            .map(Pending::output)
            .collect::<io::Result<Vec<_>>>()?;
        outputs.sort_by(|left, right| left.name.cmp(&right.name));
        if let Some(pair) = outputs.windows(2).find(|pair| pair[0].name == pair[1].name) {
            return Err(invalid(format!("duplicate output name {}", pair[0].name)));
        }
        Ok(outputs)
    }
    fn dispatch(&mut self, object: u32, opcode: u16, body: &[u8]) -> io::Result<()> {
        let mut args = Args::new(body);
        if object == DISPLAY {
            return match opcode {
                0 => {
                    let target = args.uint()?;
                    let code = args.uint()?;
                    let message = args.string()?.unwrap_or_default();
                    Err(io::Error::other(format!(
                        "compositor error {code} on object {target}: {message}"
                    )))
                }
                1 => {
                    let id = args.uint()?;
                    if self.objects.remove(&id).is_some() {
                        self.free.push(id);
                    }
                    Ok(())
                }
                _ => Ok(()),
            };
        }
        let Some(kind) = self.objects.get(&object).copied() else {
            return Ok(());
        };
        match (kind, opcode) {
            (Object::Registry, 0) => {
                let name = args.uint()?;
                let interface = args.string()?.unwrap_or_default();
                let version = args.uint()?;
                self.global(name, &interface, version)
            }
            (Object::Registry, 1) => {
                self.global_remove(args.uint()?);
                Ok(())
            }
            (Object::Callback, 0) => {
                self.synced = Some(object);
                Ok(())
            }
            (Object::Output(global), opcode) => match self.pending.get_mut(&global) {
                Some(pending) => {
                    self.changed = true;
                    output_event(pending, opcode, args)
                }
                None => Ok(()),
            },
            (Object::XdgOutput(global), opcode) => match self.pending.get_mut(&global) {
                Some(pending) => {
                    self.changed = true;
                    xdg_event(pending, opcode, args)
                }
                None => Ok(()),
            },
            _ => Ok(()),
        }
    }
    fn global(&mut self, name: u32, interface: &str, version: u32) -> io::Result<()> {
        if interface != OUTPUT_INTERFACE && interface != MANAGER_INTERFACE {
            return Ok(());
        }
        if version == 0 {
            return Err(invalid(format!("{interface} advertised at version 0")));
        }
        self.changed = true;
        if interface == MANAGER_INTERFACE {
            if self.bound.is_none() {
                self.manager = Some((name, version));
            }
        } else if self.bound.is_some() {
            self.bind_output(name, version)?;
        } else if self.globals.len() >= MAX_OUTPUTS {
            return Err(invalid("compositor advertises more than 64 outputs"));
        } else {
            self.globals.push((name, version));
        }
        Ok(())
    }
    fn global_remove(&mut self, name: u32) {
        if self.bound.is_none() {
            self.globals.retain(|(global, _)| *global != name);
            if self.manager.is_some_and(|(global, _)| global == name) {
                self.manager = None;
            }
            return;
        }
        if self.manager.is_some_and(|(global, _)| global == name) {
            self.manager_lost = true;
            self.changed = true;
        }
        let Some(pending) = self.pending.remove(&name) else {
            return;
        };
        self.changed = true;
        self.queue(Message::new(pending.xdg, 0));
        self.objects.insert(pending.xdg, Object::Released);
        if pending.version >= 3 {
            self.queue(Message::new(pending.output, 0));
        }
        self.objects.insert(pending.output, Object::Released);
    }
}

fn output_event(pending: &mut Pending, opcode: u16, mut args: Args<'_>) -> io::Result<()> {
    match opcode {
        0 => {
            for _ in 0..5 {
                args.int()?;
            }
            args.string()?;
            args.string()?;
            pending.transform = Some(args.uint()?);
        }
        1 => {
            let flags = args.uint()?;
            let size = (args.int()?, args.int()?);
            if flags & MODE_CURRENT != 0 {
                pending.mode = Some(size);
            }
        }
        2 => pending.output_done = true,
        3 => pending.scale = Some(args.int()?),
        4 => pending.name = args.string()?,
        5 => pending.description = args.string()?,
        _ => {}
    }
    Ok(())
}

fn xdg_event(pending: &mut Pending, opcode: u16, mut args: Args<'_>) -> io::Result<()> {
    match opcode {
        0 => pending.position = Some((args.int()?, args.int()?)),
        1 => pending.size = Some((args.int()?, args.int()?)),
        2 => pending.xdg_done = true,
        3 => pending.xdg_name = args.string()?,
        4 => pending.xdg_description = args.string()?,
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::net::UnixListener,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        thread::{self, JoinHandle},
    };

    const MANAGER_GLOBAL: u32 = 50;
    const SEAT_GLOBAL: u32 = 60;

    fn scratch(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "prek-display-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[derive(Clone)]
    struct Head {
        name: String,
        description: String,
        position: (i32, i32),
        size: (i32, i32),
        mode: (i32, i32),
        transform: u32,
        scale: i32,
    }
    fn head(name: &str, position: (i32, i32), size: (i32, i32), scale: i32) -> Head {
        Head {
            name: name.to_owned(),
            description: format!("{name} panel"),
            position,
            size,
            mode: (size.0 * scale, size.1 * scale),
            transform: 0,
            scale,
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Reply {
        Normal,
        Silent,
        Error,
    }

    #[derive(Clone)]
    enum Change {
        Add(Head),
        Remove(u32),
        RemoveAfter(u32),
    }

    /// In-process compositor fake answering one client connection.
    struct Fake {
        heads: Vec<Head>,
        output_version: u32,
        xdg_version: Option<u32>,
        reply: Reply,
        changes: BTreeMap<u32, Change>,
        stream: Option<std::sync::mpsc::Sender<UnixStream>>,
    }
    impl Fake {
        fn new(heads: Vec<Head>) -> Self {
            Self {
                heads,
                output_version: 4,
                xdg_version: Some(3),
                reply: Reply::Normal,
                changes: BTreeMap::new(),
                stream: None,
            }
        }
        fn serve(self, listener: UnixListener) -> JoinHandle<Server> {
            thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                if let Some(sender) = &self.stream {
                    sender.send(stream.try_clone().unwrap()).unwrap();
                }
                let mut server = Server {
                    fake: self,
                    stream,
                    input: Vec::new(),
                    registry: 0,
                    manager: (0, 0),
                    outputs: BTreeMap::new(),
                    binds: Vec::new(),
                    syncs: 0,
                    largest_id: 0,
                    released: Vec::new(),
                };
                while let Some((object, opcode, body)) = server.request() {
                    if server.fake.reply == Reply::Silent {
                        continue;
                    }
                    let events = server.handle(object, opcode, &body);
                    let bytes: Vec<u8> = events.into_iter().flat_map(Message::finish).collect();
                    if server.stream.write_all(&bytes).is_err() {
                        break;
                    }
                }
                server
            })
        }
    }

    struct Server {
        fake: Fake,
        stream: UnixStream,
        input: Vec<u8>,
        registry: u32,
        manager: (u32, u32),
        outputs: BTreeMap<u32, (usize, u32)>,
        binds: Vec<(String, u32)>,
        syncs: u32,
        largest_id: u32,
        released: Vec<u32>,
    }
    impl Server {
        fn request(&mut self) -> Option<(u32, u16, Vec<u8>)> {
            loop {
                if self.input.len() >= HEADER {
                    let header = word(&self.input, 4);
                    let size = (header >> 16) as usize;
                    if self.input.len() >= size {
                        let object = word(&self.input, 0);
                        let body = self.input[HEADER..size].to_vec();
                        self.input.drain(..size);
                        return Some((object, header as u16, body));
                    }
                }
                let mut chunk = [0; READ_CHUNK];
                match self.stream.read(&mut chunk) {
                    Ok(0) | Err(_) => return None,
                    Ok(count) => self.input.extend_from_slice(&chunk[..count]),
                }
            }
        }
        fn handle(&mut self, object: u32, opcode: u16, body: &[u8]) -> Vec<Message> {
            let mut args = Args::new(body);
            match (object, opcode) {
                (DISPLAY, 1) if self.fake.reply == Reply::Error => vec![
                    Message::new(DISPLAY, 0)
                        .uint(DISPLAY)
                        .uint(1)
                        .string("invalid method"),
                ],
                (DISPLAY, 1) => {
                    self.registry = args.uint().unwrap();
                    self.globals()
                }
                (DISPLAY, 0) => {
                    let callback = args.uint().unwrap();
                    self.largest_id = self.largest_id.max(callback);
                    self.syncs += 1;
                    let change = self.fake.changes.remove(&self.syncs);
                    let mut events = match change.clone() {
                        Some(Change::Add(head)) => {
                            self.fake.heads.push(head);
                            vec![
                                Message::new(self.registry, 0)
                                    .uint(self.fake.heads.len() as u32)
                                    .string(OUTPUT_INTERFACE)
                                    .uint(self.fake.output_version),
                            ]
                        }
                        Some(Change::Remove(global)) => {
                            vec![Message::new(self.registry, 1).uint(global)]
                        }
                        Some(Change::RemoveAfter(_)) | None => Vec::new(),
                    };
                    events.push(Message::new(callback, 0).uint(7));
                    events.push(Message::new(DISPLAY, 1).uint(callback));
                    if let Some(Change::RemoveAfter(global)) = change {
                        events.push(Message::new(self.registry, 1).uint(global));
                    }
                    events
                }
                (_, 0) if self.outputs.contains_key(&object) => {
                    self.released.push(object);
                    vec![Message::new(DISPLAY, 1).uint(object)]
                }
                (_, 0) if object == self.registry => {
                    let global = args.uint().unwrap();
                    let interface = args.string().unwrap().unwrap();
                    let version = args.uint().unwrap();
                    let id = args.uint().unwrap();
                    self.binds.push((interface, version));
                    if global == MANAGER_GLOBAL {
                        self.manager = (id, version);
                        return Vec::new();
                    }
                    let index = (global - 1) as usize;
                    self.outputs.insert(id, (index, version));
                    self.output_events(id, index, version)
                }
                (_, 1) if object == self.manager.0 => {
                    let xdg = args.uint().unwrap();
                    let output = args.uint().unwrap();
                    self.xdg_events(xdg, output)
                }
                _ => Vec::new(),
            }
        }
        fn globals(&self) -> Vec<Message> {
            let registry = self.registry;
            let mut events: Vec<_> = (1..=self.fake.heads.len() as u32)
                .map(|name| {
                    Message::new(registry, 0)
                        .uint(name)
                        .string(OUTPUT_INTERFACE)
                        .uint(self.fake.output_version)
                })
                .collect();
            events.push(
                Message::new(registry, 0)
                    .uint(SEAT_GLOBAL)
                    .string("wl_seat")
                    .uint(9),
            );
            if let Some(version) = self.fake.xdg_version {
                events.push(
                    Message::new(registry, 0)
                        .uint(MANAGER_GLOBAL)
                        .string(MANAGER_INTERFACE)
                        .uint(version),
                );
            }
            events
        }
        fn output_events(&self, id: u32, index: usize, version: u32) -> Vec<Message> {
            let head = &self.fake.heads[index];
            let mut events = vec![
                Message::new(id, 0)
                    .int(head.position.0)
                    .int(head.position.1)
                    .int(300)
                    .int(200)
                    .int(0)
                    .string("Fake")
                    .string("Panel")
                    .uint(head.transform),
                Message::new(id, 9).uint(1).uint(2),
                Message::new(id, 1)
                    .uint(0)
                    .int(head.mode.0 / 2)
                    .int(head.mode.1 / 2)
                    .int(60000),
                Message::new(id, 1)
                    .uint(0x3)
                    .int(head.mode.0)
                    .int(head.mode.1)
                    .int(60000),
            ];
            if version >= 2 {
                events.push(Message::new(id, 3).int(head.scale));
            }
            if version >= 4 {
                events.push(Message::new(id, 4).string(&head.name));
                events.push(Message::new(id, 5).string(&head.description));
            }
            if version >= 2 {
                events.push(Message::new(id, 2));
            }
            events
        }
        fn xdg_events(&self, xdg: u32, output: u32) -> Vec<Message> {
            let (index, _) = self.outputs[&output];
            let head = &self.fake.heads[index];
            let version = self.manager.1;
            let mut events = vec![
                Message::new(xdg, 0)
                    .int(head.position.0)
                    .int(head.position.1),
                Message::new(xdg, 1).int(head.size.0).int(head.size.1),
            ];
            if version >= 2 {
                events.push(Message::new(xdg, 3).string(&head.name));
                events.push(Message::new(xdg, 4).string(&head.description));
            }
            events.push(if version >= 3 {
                Message::new(output, 2)
            } else {
                Message::new(xdg, 2)
            });
            events
        }
    }

    fn query(fake: Fake, within: Duration) -> (io::Result<Vec<Output>>, Vec<(String, u32)>) {
        let root = scratch("wayland");
        let socket = root.join("wayland-0");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = fake.serve(listener);
        let result = outputs(&socket, Instant::now() + within);
        let binds = server.join().unwrap().binds;
        fs::remove_dir_all(root).unwrap();
        (result, binds)
    }

    fn names(outputs: &[Output]) -> Vec<&str> {
        outputs.iter().map(|output| output.name.as_str()).collect()
    }

    #[test]
    #[ignore = "needs a live Wayland session: XDG_RUNTIME_DIR and WAYLAND_DISPLAY"]
    fn a_live_watch_reads_the_same_outputs_on_every_roundtrip() {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").unwrap();
        let display = std::env::var_os("WAYLAND_DISPLAY").unwrap();
        let socket = Path::new(&runtime).join(display);
        let until = || Instant::now() + Duration::from_secs(1);
        let mut watch = Watch::open(&socket, until()).unwrap();
        let first = watch.outputs(until()).unwrap();
        assert!(!first.is_empty());
        for _ in 0..200 {
            assert_eq!(watch.outputs(until()).unwrap(), first);
        }
        println!("{first:?}");
    }

    #[test]
    fn a_retained_watch_follows_hotplug_and_releases_removed_outputs() {
        let mut fake = Fake::new(panels());
        fake.changes
            .insert(3, Change::Add(head("HDMI-A-1", (3200, 0), (1920, 1080), 1)));
        fake.changes.insert(5, Change::Remove(1));
        let root = scratch("watch");
        let socket = root.join("wayland-0");
        let server = fake.serve(UnixListener::bind(&socket).unwrap());
        let until = || Instant::now() + Duration::from_secs(5);
        let mut watch = Watch::open(&socket, until()).unwrap();
        assert_eq!(
            names(&watch.outputs(until()).unwrap()),
            ["DP-2", "HDMI-A-1", "eDP-1"]
        );
        assert_eq!(
            names(&watch.outputs(until()).unwrap()),
            ["DP-2", "HDMI-A-1"]
        );
        for _ in 0..50 {
            assert_eq!(watch.outputs(until()).unwrap().len(), 2);
        }
        drop(watch);
        let server = server.join().unwrap();
        fs::remove_dir_all(root).unwrap();
        assert_eq!(server.released.len(), 1);
        assert!(server.largest_id < 16, "{}", server.largest_id);
    }

    #[test]
    fn a_watch_reports_only_output_events_the_compositor_sent_on_its_own() {
        let (sender, streams) = std::sync::mpsc::channel();
        let mut fake = Fake::new(panels());
        fake.stream = Some(sender);
        let root = scratch("receive");
        let socket = root.join("wayland-0");
        let server = fake.serve(UnixListener::bind(&socket).unwrap());
        let until = || Instant::now() + Duration::from_secs(5);
        let mut watch = Watch::open(&socket, until()).unwrap();
        let mut compositor = streams.recv().unwrap();
        assert_eq!(names(&watch.outputs(until()).unwrap()), ["DP-2", "eDP-1"]);
        for _ in 0..3 {
            assert!(!watch.receive().unwrap(), "roundtrip traffic is no change");
        }
        let registry = 2;
        compositor
            .write_all(
                &Message::new(registry, 0)
                    .uint(70)
                    .string("wl_seat")
                    .uint(9)
                    .finish(),
            )
            .unwrap();
        assert!(!watch.receive().unwrap(), "other globals are no change");
        compositor
            .write_all(&Message::new(registry, 1).uint(1).finish())
            .unwrap();
        assert!(watch.receive().unwrap());
        assert!(
            watch.receive().unwrap(),
            "a change stays reported until listed"
        );
        assert_eq!(names(&watch.outputs(until()).unwrap()), ["DP-2"]);
        assert!(!watch.receive().unwrap());
        drop(compositor);
        drop(watch);
        assert_eq!(server.join().unwrap().released.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_output_event_behind_the_final_callback_is_listed_before_returning() {
        let mut fake = Fake::new(panels());
        fake.changes.insert(3, Change::RemoveAfter(1));
        let root = scratch("trailing");
        let socket = root.join("wayland-0");
        let server = fake.serve(UnixListener::bind(&socket).unwrap());
        let until = || Instant::now() + Duration::from_secs(5);
        let mut watch = Watch::open(&socket, until()).unwrap();
        assert_eq!(names(&watch.outputs(until()).unwrap()), ["DP-2"]);
        assert!(!watch.receive().unwrap());
        drop(watch);
        assert_eq!(server.join().unwrap().released.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_closed_compositor_fails_a_receive() {
        let (sender, streams) = std::sync::mpsc::channel();
        let mut fake = Fake::new(panels());
        fake.stream = Some(sender);
        let root = scratch("closed");
        let socket = root.join("wayland-0");
        let server = fake.serve(UnixListener::bind(&socket).unwrap());
        let until = || Instant::now() + Duration::from_secs(5);
        let mut watch = Watch::open(&socket, until()).unwrap();
        let compositor = streams.recv().unwrap();
        watch.outputs(until()).unwrap();
        compositor.shutdown(std::net::Shutdown::Both).unwrap();
        server.join().unwrap();
        assert_eq!(
            watch.receive().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn panels() -> Vec<Head> {
        let mut external = head("DP-2", (1280, 0), (1920, 1080), 1);
        external.transform = 1;
        vec![head("eDP-1", (0, 0), (1280, 800), 2), external]
    }

    #[test]
    fn reads_two_outputs_sorted_by_name() {
        let mut fake = Fake::new(panels());
        fake.output_version = 5;
        fake.xdg_version = Some(7);
        let (result, binds) = query(fake, Duration::from_secs(5));
        assert_eq!(
            result.unwrap(),
            vec![
                Output {
                    name: "DP-2".into(),
                    description: "DP-2 panel".into(),
                    x: 1280,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    pixel_width: 1920,
                    pixel_height: 1080,
                    transform: 1,
                    scale: 1,
                },
                Output {
                    name: "eDP-1".into(),
                    description: "eDP-1 panel".into(),
                    x: 0,
                    y: 0,
                    width: 1280,
                    height: 800,
                    pixel_width: 2560,
                    pixel_height: 1600,
                    transform: 0,
                    scale: 2,
                },
            ]
        );
        assert_eq!(
            binds,
            vec![
                (MANAGER_INTERFACE.to_owned(), 3),
                (OUTPUT_INTERFACE.to_owned(), 4),
                (OUTPUT_INTERFACE.to_owned(), 4),
            ]
        );
    }

    #[test]
    fn older_versions_take_names_from_xdg_output() {
        let mut fake = Fake::new(panels());
        fake.output_version = 3;
        fake.xdg_version = Some(2);
        let (result, binds) = query(fake, Duration::from_secs(5));
        let outputs = result.unwrap();
        let names: Vec<_> = outputs.iter().map(|output| output.name.as_str()).collect();
        assert_eq!(names, ["DP-2", "eDP-1"]);
        assert_eq!(outputs[1].description, "eDP-1 panel");
        assert_eq!(outputs[1].scale, 2);
        assert_eq!(binds[0], (MANAGER_INTERFACE.to_owned(), 2));
        assert_eq!(binds[1], (OUTPUT_INTERFACE.to_owned(), 3));
    }

    #[test]
    fn compositor_without_xdg_output_is_refused() {
        let mut fake = Fake::new(panels());
        fake.xdg_version = None;
        let (result, _) = query(fake, Duration::from_secs(5));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn silent_compositor_times_out() {
        let mut fake = Fake::new(panels());
        fake.reply = Reply::Silent;
        let started = Instant::now();
        let (result, _) = query(fake, Duration::from_millis(200));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    }

    #[test]
    fn display_error_fails_the_query() {
        let mut fake = Fake::new(panels());
        fake.reply = Reply::Error;
        let (result, _) = query(fake, Duration::from_secs(5));
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("invalid method"), "{error}");
    }

    #[test]
    fn oversized_string_is_refused() {
        let mut heads = panels();
        heads[0].name = "x".repeat(MAX_STRING + 1);
        let (result, _) = query(Fake::new(heads), Duration::from_secs(5));
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("4096"), "{error}");
    }

    #[test]
    fn invalid_outputs_are_refused() {
        let mut duplicate = panels();
        duplicate[1].name = "eDP-1".into();
        let mut transform = panels();
        transform[0].transform = 8;
        let mut empty = panels();
        empty[1].size = (0, 1080);
        for heads in [duplicate, transform, empty] {
            let (result, _) = query(Fake::new(heads), Duration::from_secs(5));
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn relative_socket_is_refused() {
        let error = outputs(
            Path::new("wayland-0"),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    fn connector(root: &Path, name: &str, enabled: &str, dpms: &str) {
        let directory = root.join(name);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("enabled"), format!("{enabled}\n")).unwrap();
        fs::write(directory.join("dpms"), format!("{dpms}\n")).unwrap();
    }

    #[test]
    fn screen_power_follows_enabled_and_dpms() {
        for (enabled, dpms, expected) in [
            ("enabled", "On", true),
            ("enabled", "Off", false),
            ("enabled", "Standby", false),
            ("disabled", "On", false),
        ] {
            let root = scratch("drm");
            connector(&root, "card0-DSI-1", enabled, dpms);
            connector(&root, "card0-DSI-10", "enabled", "On");
            assert_eq!(screen_on_at(&root, "DSI-1").unwrap(), Some(expected));
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn screen_power_requires_one_connector() {
        let root = scratch("drm");
        fs::create_dir(root.join("card0")).unwrap();
        fs::write(root.join("version"), "drm 1.1.0\n").unwrap();
        connector(&root, "card0-eDP-1", "enabled", "On");
        connector(&root, "cardx-DSI-1", "enabled", "On");
        assert_eq!(screen_on_at(&root, "DSI-1").unwrap(), None);
        connector(&root, "card0-DSI-1", "enabled", "On");
        assert_eq!(screen_on_at(&root, "DSI-1").unwrap(), Some(true));
        connector(&root, "card1-DSI-1", "enabled", "On");
        assert_eq!(screen_on_at(&root, "DSI-1").unwrap(), None);
        assert_eq!(screen_on_at(&root.join("absent"), "DSI-1").unwrap(), None);
        fs::remove_dir_all(root).unwrap();
    }
}
