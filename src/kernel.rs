//! Linux virtual-device preparation, metadata import and bounded frame writes.
//!
//! Device creation does not grab any original; the supervised route worker
//! grabs one only after authenticated consumer readiness and a neutral handoff.
//! Test devices require explicit isolation metadata and root-owned pre-add udev
//! import, and must be grabbed by their isolated receiver before any injection.

use crate::{
    input::{ABS_MT_SLOT, ABS_MT_TRACKING_ID, EV_SYN, Frame, RawEvent, SYN_REPORT},
    supervisor::{Heartbeat, Report, WorkerIdentity},
    transport::{Descriptor, permitted_property},
};
use evdev::raw_stream::RawDevice;
use nix::libc;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    mem::{offset_of, size_of},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd},
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

const REGISTRY_PARENT: &str = "/run/prek";
const REGISTRY: &str = "/run/prek/devices";
const MAX_METADATA: u64 = 16 * 1024;
const EVENT_SIZE: usize = size_of::<libc::input_event>();
const TYPE_OFFSET: usize = offset_of!(libc::input_event, type_);
const CODE_OFFSET: usize = offset_of!(libc::input_event, code);
const VALUE_OFFSET: usize = offset_of!(libc::input_event, value);

nix::ioctl_none!(create_device, b'U', 1);
nix::ioctl_none!(destroy_device, b'U', 2);
nix::ioctl_write_ptr!(device_setup, b'U', 3, libc::uinput_setup);
nix::ioctl_write_ptr!(absolute_setup, b'U', 4, libc::uinput_abs_setup);
nix::ioctl_write_int!(event_bit, b'U', 100);
nix::ioctl_write_int!(key_bit, b'U', 101);
nix::ioctl_write_int!(relative_bit, b'U', 102);
nix::ioctl_write_int!(absolute_bit, b'U', 103);
nix::ioctl_write_int!(misc_bit, b'U', 104);
nix::ioctl_write_int!(led_bit, b'U', 105);
// Linux encodes sizeof(char*) but receives the string itself, not char**.
const SET_PHYS: libc::c_ulong =
    nix::request_code_write!(b'U', 108, size_of::<*const libc::c_char>()) as libc::c_ulong;
nix::ioctl_write_ptr_bad!(physical_path, SET_PHYS, libc::c_char);
nix::ioctl_write_int!(switch_bit, b'U', 109);
nix::ioctl_write_int!(property_bit, b'U', 110);
nix::ioctl_read_buf!(sysname, b'U', 44, u8);

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn denied(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.into())
}

fn require_purpose(descriptor: &Descriptor, generated: bool) -> io::Result<()> {
    if descriptor.is_generated_keyboard() != generated {
        return Err(denied(
            "device publication purpose differs from the requested path",
        ));
    }
    Ok(())
}

/// The marker is a fixed grammar, never a caller-supplied filesystem path.
fn filename(marker: &str) -> io::Result<CString> {
    let marker = crate::transport::Marker::parse(marker).map_err(invalid)?;
    CString::new(format!("{}.json", marker.base.replace('/', "-")))
        .map_err(|_| invalid("invalid marker"))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    version: u32,
    marker: String,
    isolated: bool,
    properties: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<WorkerIdentity>,
}

impl Metadata {
    fn validate(&self, marker: &str) -> io::Result<()> {
        filename(marker)?;
        let generated = crate::transport::Marker::parse(marker)
            .map_err(invalid)?
            .generated;
        if self.version != 2
            || crate::transport::Marker::parse(marker)
                .map_err(invalid)?
                .publication
                .is_none()
            || self.owner.is_some_and(|owner| !owner.valid())
            || self.marker != marker
            || self.properties.len() > 32
            || self
                .properties
                .get("PREK_PUBLICATION")
                .is_none_or(|value| value != marker)
            || self.isolated != marker.starts_with("prek/test/")
            || generated
                != self
                    .properties
                    .get("PREK_GENERATED_KEYBOARD")
                    .is_some_and(|value| value == "1")
            || !generated && self.properties.contains_key("PREK_GENERATED_KEYBOARD")
            || self
                .properties
                .get("PREK_VIRTUAL")
                .is_none_or(|value| value != "1")
        {
            return Err(invalid("metadata identity/version does not match device"));
        }
        for (key, value) in &self.properties {
            if !(permitted_property(key)
                || key == "PREK_VIRTUAL"
                || key == "PREK_PUBLICATION"
                || generated && key == "PREK_GENERATED_KEYBOARD"
                || self.isolated && matches!(key.as_str(), "PREK_TEST" | "LIBINPUT_IGNORE_DEVICE"))
                || value.is_empty()
                || value.len() > 1024
                || value.chars().any(char::is_control)
            {
                return Err(invalid("invalid or forbidden device metadata property"));
            }
        }
        if self.isolated
            && (self
                .properties
                .get("PREK_TEST")
                .is_none_or(|value| value != "1")
                || self
                    .properties
                    .get("LIBINPUT_IGNORE_DEVICE")
                    .is_none_or(|value| value != "1"))
        {
            return Err(invalid("isolated devices must be excluded from libinput"));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Directory {
    file: File,
    owner: u32,
    mutation: Mutex<()>,
}

struct Mutation<'a> {
    directory: &'a Directory,
    _memory: MutexGuard<'a, ()>,
}
impl Drop for Mutation<'_> {
    fn drop(&mut self) {
        // SAFETY: owned directory fd; this guard holds the only mutation lock.
        unsafe { libc::flock(self.directory.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

struct FileCleanup {
    directory: Arc<Directory>,
    name: CString,
    inode: u64,
}
impl Drop for FileCleanup {
    fn drop(&mut self) {
        self.directory.unlink_same(&self.name, self.inode);
    }
}

impl Directory {
    fn open(path: &Path, owner: u32) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        if metadata.uid() != owner || metadata.mode() & 0o022 != 0 {
            return Err(denied(
                "device registry must have trusted ownership and no group/other writes",
            ));
        }
        Ok(Self {
            file,
            owner,
            mutation: Mutex::new(()),
        })
    }

    fn lock(&self) -> io::Result<Mutation<'_>> {
        let memory = self
            .mutation
            .try_lock()
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "registry mutation is busy"))?;
        // Serialize separate processes and separately opened Registry handles.
        // SAFETY: owned directory fd, nonblocking advisory lock only.
        if unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Mutation {
            directory: self,
            _memory: memory,
        })
    }

    fn unlink_same(&self, name: &CString, inode: u64) -> bool {
        if let Ok(file) = self.open_file(name, libc::O_RDONLY | libc::O_NONBLOCK)
            && let Ok(stat) = file.metadata()
            && stat.is_file()
            && stat.ino() == inode
            && stat.uid() == self.owner
        {
            // SAFETY: canonical single-component name under the held directory.
            return unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) } == 0;
        }
        false
    }

    fn open_file(&self, name: &CString, flags: i32) -> io::Result<File> {
        // SAFETY: the directory fd is owned and name is a bounded, NUL-terminated
        // single component. O_NOFOLLOW prevents substitution with a symlink.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::mode_t,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openat returns a new descriptor owned by this call.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn read(&self, marker: &str) -> io::Result<Metadata> {
        let (metadata, _) = self.read_record(marker)?;
        metadata.validate(marker)?;
        Ok(metadata)
    }
    fn read_base(&self, marker: &str) -> io::Result<Metadata> {
        self.read_record(marker).map(|(metadata, _)| metadata)
    }
    fn read_record(&self, marker: &str) -> io::Result<(Metadata, u64)> {
        let file = self.open_file(&filename(marker)?, libc::O_RDONLY | libc::O_NONBLOCK)?;
        let stat = file.metadata()?;
        if !stat.is_file()
            || stat.uid() != self.owner
            || stat.mode() & 0o077 != 0
            || stat.len() > MAX_METADATA
        {
            return Err(denied(
                "device metadata must be a bounded private trusted regular file",
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_METADATA + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_METADATA {
            return Err(invalid("metadata exceeds size limit"));
        }
        let metadata: Metadata = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        metadata.validate(&metadata.marker)?;
        if filename(&metadata.marker)? != filename(marker)? {
            return Err(invalid("metadata belongs to a different source slot"));
        }
        Ok((metadata, stat.ino()))
    }
}

/// A retained directory fd fences path replacement. Production access always
/// requires root ownership; the helper has no caller-supplied registry path.
#[derive(Clone, Debug)]
pub struct Registry {
    directory: Arc<Directory>,
}

impl Registry {
    #[cfg(test)]
    pub(crate) fn fixture(path: &Path) -> io::Result<Self> {
        Ok(Self {
            directory: Arc::new(Directory::open(path, fs::metadata(path)?.uid())?),
        })
    }
    pub fn system() -> io::Result<Self> {
        // SAFETY: geteuid has no pointer arguments or mutable side effects.
        if unsafe { libc::geteuid() } != 0 {
            return Err(denied(
                "device metadata publication requires the root broker",
            ));
        }
        Ok(Self {
            directory: Arc::new(Directory::open(Path::new(REGISTRY), 0)?),
        })
    }
    /// Create the root-owned registry and its parent when no service manager
    /// has, before a route worker opens it.
    pub fn prepare_system() -> io::Result<()> {
        // SAFETY: geteuid has no pointer arguments or mutable side effects.
        if unsafe { libc::geteuid() } != 0 {
            return Err(denied(
                "device metadata publication requires the root broker",
            ));
        }
        prepare(Path::new(REGISTRY_PARENT), 0)
    }

    fn publish(
        &self,
        descriptor: Descriptor,
        isolated: bool,
        owner: Option<WorkerIdentity>,
    ) -> io::Result<Publication> {
        descriptor.validate().map_err(invalid)?;
        if descriptor.is_isolated() != isolated {
            return Err(denied("descriptor isolation does not match publication"));
        }
        if owner.is_some() {
            supported_backend(&descriptor)?;
        }
        let descriptor = descriptor
            .bind_publication(crate::protocol::Epoch::random()?)
            .map_err(invalid)?;
        let metadata = Metadata {
            version: 2,
            marker: descriptor.marker.clone(),
            isolated,
            properties: descriptor.udev.clone(),
            owner,
        };
        metadata.validate(&metadata.marker)?;
        let bytes = serde_json::to_vec(&metadata).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_METADATA {
            return Err(invalid("metadata exceeds size limit"));
        }
        let name = filename(&metadata.marker)?;
        let _mutation = self.directory.lock()?;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // Keep the Rust 1.88 API; newer toolchains renamed this operation.
        #[allow(deprecated)]
        let nonce = NEXT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| invalid("publication sequence exhausted"))?;
        let temporary = CString::new(match owner {
            Some(owner) => format!(".prek-tmp-{}-{nonce:016x}", owner.tag()),
            None => format!(".prek-unmanaged-{:08x}-{nonce:016x}", std::process::id()),
        })
        .map_err(|_| invalid("invalid publication temporary name"))?;
        let mut file = self
            .directory
            .open_file(&temporary, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
        let stat = file.metadata()?;
        let _cleanup = FileCleanup {
            directory: Arc::clone(&self.directory),
            name: temporary.clone(),
            inode: stat.ino(),
        };
        file.write_all(&bytes)?;
        file.sync_all()?;
        // Publish complete metadata atomically, without replacing another
        // lifetime's record. A killed writer leaves only an owned temporary.
        // SAFETY: two bounded single-component names and a retained directory.
        if unsafe {
            libc::linkat(
                self.directory.file.as_raw_fd(),
                temporary.as_ptr(),
                self.directory.file.as_raw_fd(),
                name.as_ptr(),
                0,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Publication {
            directory: Arc::clone(&self.directory),
            name,
            inode: stat.ino(),
            descriptor,
            metadata,
        })
    }

    /// Prepare a physical counterpart's metadata before UI_DEV_CREATE.
    pub fn prepare(&self, descriptor: Descriptor) -> io::Result<Publication> {
        require_purpose(&descriptor, false)?;
        self.publish(descriptor, false, None)
    }

    /// Only an explicitly authorized isolated test harness may use this path.
    /// Its downstream event receiver must exclusively grab its own virtual
    /// node before injection, so the console also receives no test keystrokes.
    pub fn prepare_isolated(&self, descriptor: Descriptor) -> io::Result<Publication> {
        require_purpose(&descriptor, false)?;
        self.publish(descriptor.for_isolated().map_err(denied)?, true, None)
    }

    /// Bind pre-add metadata to the authenticated worker that owns all uinput
    /// descriptors. The supervisor can remove it after confirmed worker exit.
    pub fn prepare_for_worker(
        &self,
        descriptor: Descriptor,
        health: &Heartbeat,
    ) -> io::Result<Publication> {
        require_purpose(&descriptor, false)?;
        self.prepare_bound(descriptor, health, false)
    }

    pub fn prepare_isolated_for_worker(
        &self,
        descriptor: Descriptor,
        health: &Heartbeat,
    ) -> io::Result<Publication> {
        require_purpose(&descriptor, false)?;
        self.prepare_bound(descriptor.for_isolated().map_err(denied)?, health, true)
    }

    pub fn prepare_generated_for_worker(
        &self,
        descriptor: Descriptor,
        health: &Heartbeat,
    ) -> io::Result<Publication> {
        require_purpose(&descriptor, true)?;
        self.prepare_bound(descriptor, health, false)
    }

    /// Publish only an isolated generated keyboard for a user-launched kernel fixture.
    pub fn prepare_generated_isolated(&self, descriptor: Descriptor) -> io::Result<Publication> {
        require_purpose(&descriptor, true)?;
        self.publish(descriptor.for_isolated().map_err(denied)?, true, None)
    }

    pub fn prepare_generated_isolated_for_worker(
        &self,
        descriptor: Descriptor,
        health: &Heartbeat,
    ) -> io::Result<Publication> {
        require_purpose(&descriptor, true)?;
        self.prepare_bound(descriptor.for_isolated().map_err(denied)?, health, true)
    }

    fn prepare_bound(
        &self,
        descriptor: Descriptor,
        health: &Heartbeat,
        isolated: bool,
    ) -> io::Result<Publication> {
        let owner = health.worker_identity();
        if owner.pid() != std::process::id() {
            return Err(denied("publication belongs to a different worker"));
        }
        self.publish(descriptor, isolated, Some(owner))
    }

    /// A Report exists only after the supervising process has reaped its child.
    /// Unmanaged records and other worker lifetimes are preserved.
    pub fn recover_worker(&self, report: &Report) -> io::Result<usize> {
        self.recover(Some(report.worker_identity()), true)
    }

    /// Restart recovery also covers supervisor death. Each persisted owner is
    /// checked against a kernel PID handle and process birth before deletion.
    pub fn recover_orphans(&self) -> io::Result<usize> {
        self.recover(None, false)
    }

    /// Recover only this persisted lifetime after confirming its kernel process
    /// has exited. Other lifetimes and unmanaged publications are preserved.
    pub fn recover_orphan(&self, worker: WorkerIdentity) -> io::Result<usize> {
        if !worker.valid() {
            return Err(invalid("invalid orphan recovery identity"));
        }
        self.recover(Some(worker), false)
    }

    fn recover(&self, scope: Option<WorkerIdentity>, reaped: bool) -> io::Result<usize> {
        let _mutation = self.directory.lock()?;
        let mut selected = Vec::new();
        let mut lifetimes = BTreeMap::new();
        // The proc link resolves our retained fd, never a replaced registry path.
        for (count, entry) in
            fs::read_dir(format!("/proc/self/fd/{}", self.directory.file.as_raw_fd()))?.enumerate()
        {
            if count >= 4096 {
                return Err(invalid("registry recovery scan exceeds bounds"));
            }
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let owner = if let Some(rest) = name.strip_prefix(".prek-tmp-") {
                let Some((tag, nonce)) = rest.rsplit_once('-') else {
                    continue;
                };
                if nonce.len() != 16 || u64::from_str_radix(nonce, 16).is_err() {
                    continue;
                }
                WorkerIdentity::from_tag(tag)
            } else if let Some(marker) = registry_marker(&name) {
                self.directory.read_base(&marker)?.owner
            } else {
                None
            };
            let Some(owner) = owner else {
                continue;
            };
            if scope.is_some_and(|scope| scope != owner) {
                continue;
            }
            let dead = if reaped {
                true
            } else if let Some(dead) = lifetimes.get(&owner) {
                *dead
            } else {
                let dead = owner.confirmed_dead()?;
                lifetimes.insert(owner, dead);
                dead
            };
            if !dead {
                continue;
            }
            let name = CString::new(name).map_err(|_| invalid("invalid registry filename"))?;
            let file = self
                .directory
                .open_file(&name, libc::O_RDONLY | libc::O_NONBLOCK)?;
            let stat = file.metadata()?;
            if !stat.is_file()
                || stat.uid() != self.directory.owner
                || stat.mode() & 0o077 != 0
                || stat.len() > MAX_METADATA
            {
                return Err(denied("untrusted recovery record"));
            }
            selected.push((name, stat.ino()));
        }
        // Validate the full scan before mutation, so an unreadable/malformed
        // owned entry cannot produce a partial success acknowledgement.
        let mut removed = 0;
        for (name, inode) in selected {
            if !self.directory.unlink_same(&name, inode) {
                return Err(invalid("recovery record changed before removal"));
            }
            removed += 1;
        }
        Ok(removed)
    }
}

/// Create `parent` and its private `devices` registry when missing; each must
/// be owned by `owner` without group/other writes.
fn prepare(parent: &Path, owner: u32) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o755).create(parent) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    let directory = Directory::open(parent, owner)?;
    // SAFETY: a verified directory fd and a static NUL-terminated name.
    if unsafe { libc::mkdirat(directory.file.as_raw_fd(), c"devices".as_ptr(), 0o700) } < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    Directory::open(&parent.join("devices"), owner).map(drop)
}

fn registry_marker(name: &str) -> Option<String> {
    let parts: Vec<_> = name.strip_suffix(".json")?.split('-').collect();
    let marker = match parts.as_slice() {
        ["prek", "keys", device, generation] => format!("prek/keys/{device}/{generation}"),
        ["prek", "test", "keys", device, generation] => {
            format!("prek/test/keys/{device}/{generation}")
        }
        ["prek", device, generation] => format!("prek/{device}/{generation}"),
        ["prek", "test", device, generation] => format!("prek/test/{device}/{generation}"),
        _ => return None,
    };
    filename(&marker).ok().map(|_| marker)
}

#[derive(Debug)]
pub struct Publication {
    directory: Arc<Directory>,
    name: CString,
    inode: u64,
    descriptor: Descriptor,
    metadata: Metadata,
}
impl Publication {
    pub(crate) fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }
    fn verify(&self) -> io::Result<()> {
        let (metadata, inode) = self.directory.read_record(&self.metadata.marker)?;
        if inode != self.inode || metadata != self.metadata {
            return Err(invalid(
                "virtual publication was replaced after preparation",
            ));
        }
        Ok(())
    }
}

impl Drop for Publication {
    fn drop(&mut self) {
        // Do not remove a replacement file belonging to a newer publication.
        if let Ok(_mutation) = self.directory.lock() {
            self.directory.unlink_same(&self.name, self.inode);
        }
    }
}

/// Udev's IMPORT helper. Caller identity is checked and only static metadata is
/// returned. No raw events, paths, commands or credentials are imported.
pub fn import_metadata(marker: &str, output: &mut impl Write) -> io::Result<()> {
    let registry = Registry::system()?;
    let metadata = registry.directory.read(marker)?;
    for (key, value) in metadata.properties {
        writeln!(output, "{key}={value}")?;
    }
    Ok(())
}

/// Input events use native ABI fields. Timestamp/padding bytes are explicitly
/// zero; caller scheduling determines injection time. No struct padding is read.
pub fn encode_events(events: &[RawEvent]) -> io::Result<Vec<u8>> {
    if events.is_empty() || events.len() > 4096 {
        return Err(invalid("event batch exceeds bounds"));
    }
    let mut bytes = vec![0; events.len() * EVENT_SIZE];
    for (event, item) in events.iter().zip(bytes.as_chunks_mut::<EVENT_SIZE>().0) {
        item[TYPE_OFFSET..TYPE_OFFSET + 2].copy_from_slice(&event.kind.to_ne_bytes());
        item[CODE_OFFSET..CODE_OFFSET + 2].copy_from_slice(&event.code.to_ne_bytes());
        item[VALUE_OFFSET..VALUE_OFFSET + 4].copy_from_slice(&event.value.to_ne_bytes());
    }
    Ok(bytes)
}

fn decode_events(bytes: &[u8]) -> io::Result<Vec<RawEvent>> {
    if bytes.is_empty()
        || !bytes.len().is_multiple_of(EVENT_SIZE)
        || bytes.len() > 4096 * EVENT_SIZE
    {
        return Err(invalid("invalid kernel input-event batch"));
    }
    Ok(bytes
        .as_chunks::<EVENT_SIZE>()
        .0
        .iter()
        .map(|item| RawEvent {
            kind: u16::from_ne_bytes(item[TYPE_OFFSET..TYPE_OFFSET + 2].try_into().unwrap()),
            code: u16::from_ne_bytes(item[CODE_OFFSET..CODE_OFFSET + 2].try_into().unwrap()),
            value: i32::from_ne_bytes(item[VALUE_OFFSET..VALUE_OFFSET + 4].try_into().unwrap()),
        })
        .collect())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Flush {
    Complete,
    Pending,
}

#[derive(Debug)]
pub(crate) struct Batch {
    bytes: Vec<u8>,
    written: usize,
}

impl Batch {
    fn written_prefix(&self) -> io::Result<Vec<RawEvent>> {
        if self.written == 0 {
            Ok(Vec::new())
        } else {
            decode_events(&self.bytes[..self.written])
        }
    }

    pub(crate) fn events(events: &[RawEvent]) -> io::Result<Self> {
        Ok(Self {
            bytes: encode_events(events)?,
            written: 0,
        })
    }
    pub(crate) fn flush(&mut self, writer: &mut impl Write) -> io::Result<Flush> {
        // Bound syscall work per loop iteration, including repeated EINTR.
        for _ in 0..16 {
            if self.written == self.bytes.len() {
                return Ok(Flush::Complete);
            }
            match writer.write(&self.bytes[self.written..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "event write made no progress",
                    ));
                }
                Ok(length)
                    if length <= self.bytes.len() - self.written && length % EVENT_SIZE == 0 =>
                {
                    self.written += length;
                }
                Ok(_) => {
                    return Err(invalid(
                        "partial write did not end on an input-event boundary",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    return Ok(Flush::Pending);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(if self.written == self.bytes.len() {
            Flush::Complete
        } else {
            Flush::Pending
        })
    }
}

fn supported_backend(descriptor: &Descriptor) -> io::Result<()> {
    descriptor.validate().map_err(invalid)?;
    if descriptor.repeat.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "uinput software repeat cannot transparently mirror source repeats and settings",
        ));
    }
    if descriptor.axes.values().any(|axis| axis.fuzz != 0) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "nonzero fuzz would filter forwarded coordinates twice; compatibility is pending",
        ));
    }
    if let Some(slot) = descriptor.axes.get(&ABS_MT_SLOT) {
        if slot.maximum >= 100 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "uinput supports at most 100 Type B slots; leave source ungrabbed",
            ));
        }
        let tracking = &descriptor.axes[&ABS_MT_TRACKING_ID];
        if slot.flat != 0
            || tracking.minimum != 0
            || tracking.maximum != i32::from(u16::MAX)
            || tracking.flat != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "uinput Type B initialization would change slot/tracking-ID metadata",
            ));
        }
    }
    for (&code, axis) in &descriptor.axes {
        let span = i64::from(axis.maximum) - i64::from(axis.minimum);
        if i64::from(axis.flat) > span {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "axis {code} flat {} exceeds its native range {span}",
                    axis.flat
                ),
            ));
        }
    }
    Ok(())
}

fn processed_publication(
    publication: &crate::linux::DevicePublication,
    descriptor: &Descriptor,
) -> io::Result<bool> {
    descriptor.validate().map_err(invalid)?;
    if crate::transport::Marker::parse(&descriptor.marker)
        .map_err(invalid)?
        .publication
        .is_none()
    {
        return Err(invalid(
            "virtual metadata requires a bound publication identity",
        ));
    }
    if !publication.initialized
        || publication
            .properties
            .get("PREK_VIRTUAL")
            .is_none_or(|value| value != "1")
        || publication
            .properties
            .get("PREK_PUBLICATION")
            .is_none_or(|value| value != &descriptor.marker)
    {
        return Ok(false);
    }
    for (key, value) in &descriptor.udev {
        if publication.properties.get(key) != Some(value) {
            return Err(invalid(
                "processed virtual property differs from preparation",
            ));
        }
    }
    Ok(true)
}

/// Sole owner of its uinput descriptor and pre-add metadata publication.
/// Dropping closes/destroys the virtual node; no cloned FD is exposed.
#[derive(Debug)]
pub struct Virtual {
    file: File,
    publication: Publication,
    pending: Option<Batch>,
    stopped: bool,
    created: bool,
    sequence: Option<u64>,
}

impl Virtual {
    #[cfg(test)]
    pub(crate) fn ordinary_file_fixture(publication: Publication, file: File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        // SAFETY: geteuid takes no arguments and reads process credentials.
        let uid = unsafe { libc::geteuid() };
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.mode() & 0o077 != 0
            || metadata.uid() != uid
            || !publication.descriptor.is_isolated()
        {
            return Err(invalid(
                "fixture requires a private owned regular file and isolated publication",
            ));
        }
        publication.verify()?;
        Ok(Self {
            file,
            publication,
            pending: None,
            stopped: false,
            created: false,
            sequence: None,
        })
    }
    #[cfg(test)]
    pub(crate) fn is_ordinary_file_fixture(&self) -> bool {
        !self.created
            && self.publication.descriptor.is_isolated()
            && self
                .file
                .metadata()
                .is_ok_and(|metadata| metadata.is_file())
    }
    pub fn create(publication: Publication) -> io::Result<Self> {
        supported_backend(&publication.descriptor)?;
        // Registry metadata must still exist before the device add event.
        publication.verify()?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open("/dev/uinput")?;
        let mut output = Self {
            file,
            publication,
            pending: None,
            stopped: false,
            created: false,
            sequence: None,
        };
        let spec = &output.publication.descriptor;
        let fd = output.file.as_raw_fd();
        // SAFETY: every ioctl operates on our owned uinput fd and validated
        // capability codes; none touches an original device or takes a grab.
        unsafe {
            for (&kind, codes) in &spec.capabilities {
                event_bit(fd, kind.into())?;
                for &code in codes {
                    match kind {
                        0 => {}
                        1 => {
                            key_bit(fd, code.into())?;
                        }
                        2 => {
                            relative_bit(fd, code.into())?;
                        }
                        3 => {
                            absolute_bit(fd, code.into())?;
                        }
                        4 => {
                            misc_bit(fd, code.into())?;
                        }
                        5 => {
                            switch_bit(fd, code.into())?;
                        }
                        17 => {
                            led_bit(fd, code.into())?;
                        }
                        _ => return Err(invalid("unsupported uinput event type")),
                    }
                }
            }
            for &property in &spec.properties {
                property_bit(fd, property.into())?;
            }
            for (&code, axis) in &spec.axes {
                let mut value: libc::uinput_abs_setup = std::mem::zeroed();
                value.code = code;
                value.absinfo.minimum = axis.minimum;
                value.absinfo.maximum = axis.maximum;
                value.absinfo.resolution = axis.resolution;
                value.absinfo.fuzz = axis.fuzz;
                value.absinfo.flat = axis.flat;
                absolute_setup(fd, &value)?;
            }
            let marker = CString::new(output.publication.metadata.marker.clone())
                .map_err(|_| invalid("invalid physical marker"))?;
            physical_path(fd, marker.as_ptr())?;
            let mut setup: libc::uinput_setup = std::mem::zeroed();
            setup.id = libc::input_id {
                bustype: spec.identity.bus,
                vendor: spec.identity.vendor,
                product: spec.identity.product,
                version: spec.identity.version,
            };
            let name = spec.name.clone();
            if name.len() >= setup.name.len() {
                return Err(invalid("uinput name too long"));
            }
            for (slot, byte) in setup.name.iter_mut().zip(name.bytes()) {
                *slot = byte as libc::c_char;
            }
            device_setup(fd, &setup)?;
            create_device(fd)?;
        }
        output.created = true;
        Ok(output)
    }

    pub fn marker(&self) -> &str {
        &self.publication.metadata.marker
    }
    pub fn isolated(&self) -> bool {
        self.publication.metadata.isolated
    }
    pub fn descriptor(&self) -> &Descriptor {
        &self.publication.descriptor
    }

    /// Verify processed udev properties and the actual evdev ABI of this owned
    /// virtual node. Node existence alone is never consumer readiness.
    pub fn processed_metadata(&self) -> io::Result<bool> {
        Ok(self.processed_device()?.is_some())
    }

    pub(crate) fn generated_neutral(&self) -> io::Result<bool> {
        require_purpose(self.descriptor(), true)?;
        let Some(raw) = self.processed_device()? else {
            return Ok(false);
        };
        Ok(raw.get_key_state()?.iter().next().is_none())
    }

    fn processed_device(&self) -> io::Result<Option<RawDevice>> {
        if !self.created || self.stopped {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "virtual device is not live",
            ));
        }
        self.publication.verify()?;
        let nodes = self.event_nodes()?;
        if nodes.is_empty() {
            return Ok(None);
        }
        if nodes.len() != 1 {
            return Err(invalid("virtual device has ambiguous event nodes"));
        }
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&nodes[0])
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        use std::os::unix::fs::FileTypeExt;
        let node = file.metadata()?;
        if !node.file_type().is_char_device() || node.rdev() == 0 {
            return Err(invalid("virtual event node is not a device"));
        }
        let publication = crate::linux::DevicePublication::read(file.as_fd())?;
        let spec = self.descriptor();
        if !processed_publication(&publication, spec)? {
            return Ok(None);
        }
        let raw = RawDevice::from_fd(file.into())?;
        let id = raw.input_id();
        if raw.name() != Some(spec.name.as_str())
            || raw.physical_path() != Some(self.marker())
            || (id.bus_type().0, id.vendor(), id.product(), id.version())
                != (
                    spec.identity.bus,
                    spec.identity.vendor,
                    spec.identity.product,
                    spec.identity.version,
                )
            || raw
                .properties()
                .iter()
                .map(|property| property.0)
                .collect::<Vec<_>>()
                != spec.properties
            || raw
                .supported_events()
                .iter()
                .map(|event| event.0)
                .collect::<Vec<_>>()
                != spec.capabilities.keys().copied().collect::<Vec<_>>()
        {
            return Err(invalid(
                "created virtual identity/properties/capabilities differ",
            ));
        }
        for (&kind, expected) in &spec.capabilities {
            if kind == EV_SYN {
                continue;
            }
            let mut bytes = [0u8; 128];
            let request = nix::request_code_read!(b'E', 0x20 + u32::from(kind), bytes.len());
            // SAFETY: read-only ioctl on this owned node with a bounded buffer.
            if unsafe { libc::ioctl(raw.as_raw_fd(), request, bytes.as_mut_ptr()) } < 0 {
                return Err(io::Error::last_os_error());
            }
            let codes = (0..bytes.len() * 8)
                .filter(|bit| bytes[bit / 8] & (1 << (bit % 8)) != 0)
                .map(|bit| bit as u16)
                .collect::<Vec<_>>();
            if &codes != expected {
                return Err(invalid("created virtual event codes differ"));
            }
        }
        let axes: BTreeMap<_, _> = raw
            .get_absinfo()?
            .map(|(code, info)| {
                (
                    code.0,
                    crate::input::Axis {
                        minimum: info.minimum(),
                        maximum: info.maximum(),
                        resolution: info.resolution(),
                        fuzz: info.fuzz(),
                        flat: info.flat(),
                    },
                )
            })
            .collect();
        if axes != spec.axes {
            return Err(invalid("created virtual axes differ"));
        }
        if !publication.current(raw.as_fd(), &nodes[0]) || self.event_nodes()? != nodes {
            return Ok(None);
        }
        self.publication.verify()?;
        Ok(Some(raw))
    }

    /// Discover only this owned virtual node. Absence is not readiness; the
    /// broker must still validate processed udev and the actual consumer.
    pub fn event_nodes(&self) -> io::Result<Vec<PathBuf>> {
        let mut name = [0u8; 128];
        // SAFETY: owned uinput fd and bounded output buffer, UI_GET_SYSNAME ABI.
        let length =
            unsafe { sysname(self.file.as_raw_fd(), &mut name) }.map_err(io::Error::from)?;
        if length <= 0 || length as usize > name.len() {
            return Err(invalid("invalid sysfs device name length"));
        }
        let name = name[..length as usize]
            .split(|byte| *byte == 0)
            .next()
            .unwrap_or_default();
        let name = std::str::from_utf8(name).map_err(io::Error::other)?;
        if !name
            .strip_prefix("input")
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        {
            return Err(invalid("invalid virtual device sysfs name"));
        }
        let mut nodes = Vec::new();
        for entry in fs::read_dir(Path::new("/sys/devices/virtual/input").join(name))? {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_str().is_some_and(|name| {
                name.strip_prefix("event").is_some_and(|id| {
                    !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())
                })
            }) {
                nodes.push(Path::new("/dev/input").join(name));
            }
        }
        nodes.sort();
        Ok(nodes)
    }

    /// Initialize persistent coordinates on a pristine virtual device without
    /// introducing a contact or key. Flush this before consumer readiness.
    pub fn initialize_absolute(
        &mut self,
        state: &crate::input::AbsoluteState,
        at: std::time::Duration,
    ) -> io::Result<()> {
        if self.sequence.is_some() || self.pending.is_some() {
            return Err(invalid(
                "absolute initialization requires a pristine virtual device",
            ));
        }
        state.validate(&self.descriptor().axes).map_err(invalid)?;
        let frame = state.initialization(self.descriptor().source.clone(), at);
        self.enqueue(&frame)
    }

    /// One in-flight frame maximum. Acknowledgement to Router is allowed only
    /// after flush completes; no appended/duplicated SYN_REPORT is generated.
    pub fn enqueue(&mut self, frame: &Frame) -> io::Result<()> {
        if self.stopped {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "virtual output stopped",
            ));
        }
        if self.pending.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "previous frame has not completed",
            ));
        }
        let spec = &self.publication.descriptor;
        // Repeat-setting echoes never reach a mirror whose consumer repeats keys itself.
        let filtered: Vec<_>;
        let events = if spec.compositor_repeat && frame.events.iter().any(|event| event.kind == 20)
        {
            filtered = frame
                .events
                .iter()
                .copied()
                .filter(|event| event.kind != 20)
                .collect();
            &filtered[..]
        } else {
            &frame.events[..]
        };
        if frame.source != spec.source
            || self
                .sequence
                .is_some_and(|sequence| frame.sequence <= sequence)
            || events
                .last()
                .is_none_or(|event| event.kind != EV_SYN || event.code != SYN_REPORT)
            || events[..events.len().saturating_sub(1)]
                .iter()
                .any(|event| event.kind == EV_SYN)
            || events.iter().any(|event| {
                event.kind != EV_SYN
                    && !spec
                        .capabilities
                        .get(&event.kind)
                        .is_some_and(|codes| codes.contains(&event.code))
            })
        {
            return Err(invalid(
                "frame source/sequence/boundaries/capabilities do not match output",
            ));
        }
        self.pending = Some(Batch::events(events)?);
        self.sequence = Some(frame.sequence);
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<Flush> {
        if self.stopped {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "virtual output stopped",
            ));
        }
        let Some(pending) = &mut self.pending else {
            return Ok(Flush::Complete);
        };
        match pending.flush(&mut self.file) {
            Ok(Flush::Complete) => {
                self.pending = None;
                Ok(Flush::Complete)
            }
            Ok(Flush::Pending) => Ok(Flush::Pending),
            Err(error) => {
                self.stopped = true;
                Err(error)
            }
        }
    }

    /// Drain only supported reverse feedback. The forwarding loop must not
    /// echo it back into this descriptor. Physical feedback writes are a
    /// separate verified writable-source path, not raw input injection.
    pub(crate) fn cancel_pending(&mut self) -> io::Result<Vec<RawEvent>> {
        if self.stopped {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "virtual output stopped",
            ));
        }
        let Some(pending) = self.pending.take() else {
            return Ok(Vec::new());
        };
        pending.written_prefix()
    }

    pub fn feedback(&mut self) -> io::Result<Vec<RawEvent>> {
        let mut bytes = vec![0u8; EVENT_SIZE * 32];
        match self.file.read(&mut bytes) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "uinput feedback stream ended",
            )),
            Ok(length) => {
                let events = decode_events(&bytes[..length])?;
                if events.iter().any(|event| {
                    event.kind != 17
                        || !self
                            .publication
                            .descriptor
                            .capabilities
                            .get(&17)
                            .is_some_and(|codes| codes.contains(&event.code))
                        || !matches!(event.value, 0 | 1)
                }) {
                    return Err(invalid("unsupported or malformed uinput feedback"));
                }
                Ok(events)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(Vec::new())
            }
            Err(error) => Err(error),
        }
    }
}

impl AsFd for Virtual {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl Drop for Virtual {
    fn drop(&mut self) {
        if self.created {
            // SAFETY: owned created uinput fd. Failure still falls back to file
            // close, which invokes uinput_release and destroys the device.
            unsafe {
                let _ = destroy_device(self.file.as_raw_fd());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    mod fuzz;
    mod generated;
    mod geometry;
    mod publication;
    mod repeat;
    use super::*;
    use crate::{input::Device, transport::KernelIdentity};
    use std::{
        collections::VecDeque,
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
    };

    struct Temporary {
        path: PathBuf,
    }
    impl Temporary {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "prek-registry-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self { path }
        }
        fn registry(&self) -> Registry {
            // Only this unit helper accepts the test user's ownership. Kernel
            // integration/production always uses root-owned Registry::system.
            Registry {
                directory: Arc::new(
                    Directory::open(&self.path, unsafe { libc::geteuid() }).unwrap(),
                ),
            }
        }
    }
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn descriptor() -> Descriptor {
        let mut device = crate::transport::tests::touch();
        device.identity = "test:touch".into();
        device.name = "Prek test touchscreen".into();
        device.physical = "fixture/touch".into();
        device.path.clear();
        for axis in device.axes.values_mut() {
            axis.fuzz = 0;
        }
        Descriptor::mirror(
            &device,
            KernelIdentity {
                bus: 3,
                vendor: 1,
                product: 2,
                version: 1,
            },
            None,
            &BTreeMap::new(),
        )
        .unwrap()
    }

    #[test]
    fn registry_preparation_creates_private_directories_only_under_a_trusted_parent() {
        use std::os::unix::fs::PermissionsExt;
        let temporary = Temporary::new();
        let owner = unsafe { libc::geteuid() };
        let parent = temporary.path.join("prek");
        prepare(&parent, owner).unwrap();
        prepare(&parent, owner).unwrap();
        let devices = fs::metadata(parent.join("devices")).unwrap();
        assert!(devices.is_dir());
        assert_eq!(devices.mode() & 0o077, 0);
        assert!(Directory::open(&parent.join("devices"), owner).is_ok());
        fs::remove_dir(parent.join("devices")).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o775)).unwrap();
        assert_eq!(
            prepare(&parent, owner).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(!parent.join("devices").exists());
        assert_eq!(
            prepare(&temporary.path.join("prek"), owner + 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn atomic_publication_does_not_replace_existing_metadata_or_leave_temporaries() {
        let temporary = Temporary::new();
        let registry = temporary.registry();
        let original = registry.prepare_isolated(descriptor()).unwrap();
        let contents = registry.directory.read(&original.metadata.marker).unwrap();
        assert_eq!(
            registry.prepare_isolated(descriptor()).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            registry.directory.read(&original.metadata.marker).unwrap(),
            contents
        );
        assert_eq!(fs::read_dir(&temporary.path).unwrap().count(), 1);
    }

    #[test]
    fn orphan_recovery_preserves_live_workers_and_unmanaged_publications() {
        let temporary = Temporary::new();
        let registry = temporary.registry();
        let mut command = Command::new("sleep");
        command.arg("10");
        let supervisor = crate::supervisor::Supervisor::spawn(command).unwrap();
        let managed = registry
            .publish(
                descriptor().for_isolated().unwrap(),
                true,
                Some(supervisor.worker_identity()),
            )
            .unwrap();
        // Rebuild rather than altering a validated descriptor.
        let mut source = crate::transport::tests::touch();
        source.identity = "test:other".into();
        source.name = "Prek test other".into();
        source.physical = "fixture/other".into();
        source.path.clear();
        source.source.generation += 1;
        for axis in source.axes.values_mut() {
            axis.fuzz = 0;
        }
        let spec = Descriptor::mirror(
            &source,
            KernelIdentity {
                bus: 3,
                vendor: 1,
                product: 2,
                version: 1,
            },
            None,
            &BTreeMap::new(),
        )
        .unwrap();
        let unmanaged = registry.prepare_isolated(spec).unwrap();
        assert_eq!(registry.recover_orphans().unwrap(), 0);
        drop(supervisor);
        assert_eq!(registry.recover_orphans().unwrap(), 1);
        assert!(registry.directory.read(&managed.metadata.marker).is_err());
        assert_eq!(
            registry.directory.read(&unmanaged.metadata.marker).unwrap(),
            unmanaged.metadata
        );
    }

    #[test]
    fn orphan_recovery_removes_a_dead_workers_generated_keyboard_records() {
        let temporary = Temporary::new();
        let registry = temporary.registry();
        let mut command = Command::new("sleep");
        command.arg("10");
        let supervisor = crate::supervisor::Supervisor::spawn(command).unwrap();
        let keyboard = |isolated: bool| {
            let descriptor = Descriptor::generated_keyboard(crate::input::Source {
                device: 1,
                generation: 1,
                seat: "seat0".into(),
            })
            .unwrap();
            let descriptor = match isolated {
                true => descriptor.for_isolated().unwrap(),
                false => descriptor,
            };
            registry
                .publish(descriptor, isolated, Some(supervisor.worker_identity()))
                .unwrap()
        };
        let markers = [keyboard(false), keyboard(true)].map(|publication| {
            let marker = publication.metadata.marker.clone();
            std::mem::forget(publication);
            marker
        });
        assert_eq!(registry.recover_orphans().unwrap(), 0);
        drop(supervisor);
        assert_eq!(registry.recover_orphans().unwrap(), 2);
        for marker in markers {
            assert!(registry.directory.read(&marker).is_err());
        }
        assert_eq!(fs::read_dir(&temporary.path).unwrap().count(), 0);
    }

    #[test]
    fn scoped_orphan_recovery_preserves_other_dead_epochs_and_live_workers() {
        let temporary = Temporary::new();
        let registry = temporary.registry();
        let mut command = Command::new("sleep");
        command.arg("10");
        let supervisor = crate::supervisor::Supervisor::spawn(command).unwrap();
        let own = supervisor.worker_identity();
        let publication = registry
            .publish(descriptor().for_isolated().unwrap(), true, Some(own))
            .unwrap();
        let mut json = serde_json::to_value(own).unwrap();
        json["epoch"] = serde_json::json!(json["epoch"].as_u64().unwrap() + 1);
        let foreign: WorkerIdentity = serde_json::from_value(json).unwrap();
        let name = CString::new(format!(".prek-tmp-{}-0000000000000000", foreign.tag())).unwrap();
        let file = registry
            .directory
            .open_file(&name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)
            .unwrap();
        assert_eq!(registry.recover_orphan(own).unwrap(), 0);
        drop(supervisor);
        assert_eq!(registry.recover_orphan(own).unwrap(), 1);
        assert!(
            registry
                .directory
                .read(&publication.metadata.marker)
                .is_err()
        );
        assert_eq!(file.metadata().unwrap().nlink(), 1);
        assert_eq!(registry.recover_orphan(own).unwrap(), 0);
        assert_eq!(registry.recover_orphan(foreign).unwrap(), 1);
        assert_eq!(file.metadata().unwrap().nlink(), 0);
    }

    #[test]
    fn report_recovery_fences_epochs_and_removes_incomplete_owned_temporaries() {
        let temporary = Temporary::new();
        let registry = temporary.registry();
        let mut supervisor = crate::supervisor::Supervisor::spawn(Command::new("true")).unwrap();
        let report = supervisor.wait().unwrap();
        let own = report.worker_identity();
        let mut json = serde_json::to_value(own).unwrap();
        json["epoch"] = serde_json::json!(json["epoch"].as_u64().unwrap() + 1);
        let newer = serde_json::from_value(json).unwrap();
        let preserved = registry
            .publish(descriptor().for_isolated().unwrap(), true, Some(newer))
            .unwrap();
        let name = format!(".prek-tmp-{}-0000000000000000", own.tag());
        let file = registry
            .directory
            .open_file(
                &CString::new(name).unwrap(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            )
            .unwrap();
        assert_eq!(file.metadata().unwrap().len(), 0);
        drop(file);
        assert_eq!(registry.recover_worker(&report).unwrap(), 1);
        assert_eq!(
            registry.directory.read(&preserved.metadata.marker).unwrap(),
            preserved.metadata
        );
    }

    #[test]
    fn root_registry_marker_cannot_escape_or_alias_a_path() {
        for marker in [
            "../../etc/shadow",
            "prek/../a",
            "prek/0000000000000001/000000000000000A",
            "prek/test/x/y",
            "prek/a/b/extra",
        ] {
            assert!(filename(marker).is_err());
        }
        assert!(filename("prek/0000000000000001/0000000000000001").is_ok());
    }

    #[test]
    fn metadata_is_private_preadd_exclusive_and_removed_by_its_owner() {
        let temp = Temporary::new();
        let registry = temp.registry();
        let spec = descriptor();
        let publication = registry.prepare(spec.clone()).unwrap();
        let marker = publication.metadata.marker.clone();
        assert_eq!(
            registry.directory.read(&marker).unwrap().properties["PREK_VIRTUAL"],
            "1"
        );
        assert!(registry.prepare(spec).is_err());
        let path = temp.path.join(publication.name.to_str().unwrap());
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        drop(publication);
        assert!(!path.exists());
    }

    #[test]
    fn imports_reject_symlinks_unknown_properties_and_descriptor_mutation() {
        let temp = Temporary::new();
        let registry = temp.registry();
        let spec = descriptor();
        let mut changed = spec.clone();
        changed.properties.clear();
        assert!(registry.prepare(changed).is_err());
        let path = temp
            .path
            .join(filename(&spec.marker).unwrap().to_str().unwrap());
        std::os::unix::fs::symlink("/dev/null", &path).unwrap();
        assert!(registry.directory.read(&spec.marker).is_err());
        fs::remove_file(&path).unwrap();
        let publication = registry.prepare(spec).unwrap();
        let mut metadata = publication.metadata.clone();
        metadata.properties.insert("RUN".into(), "something".into());
        assert!(metadata.validate(&metadata.marker).is_err());
        metadata.properties.remove("RUN");
        metadata
            .properties
            .insert("ID_SEAT".into(), "seat0\nRUN=bad".into());
        assert!(metadata.validate(&metadata.marker).is_err());
    }

    #[test]
    fn isolated_publication_requires_both_feedback_exclusion_markers() {
        let temp = Temporary::new();
        let registry = temp.registry();
        let publication = registry.prepare_isolated(descriptor()).unwrap();
        let marker = &publication.metadata.marker;
        assert!(marker.starts_with("prek/test/"));
        assert_eq!(publication.descriptor.marker, *marker);
        assert_eq!(publication.descriptor.udev, publication.metadata.properties);
        publication.descriptor.validate().unwrap();
        assert!(registry.prepare(publication.descriptor.clone()).is_err());
        let device = crate::transport::tests::touch();
        let production = Descriptor::mirror(
            &device,
            publication.descriptor.identity,
            None,
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(registry.prepare_isolated(production).is_err());
        let mut metadata = registry.directory.read(marker).unwrap();
        assert_eq!(metadata.properties["LIBINPUT_IGNORE_DEVICE"], "1");
        metadata.properties.remove("PREK_TEST");
        assert!(metadata.validate(marker).is_err());
    }

    #[test]
    fn native_event_encoding_keeps_raw_values_and_single_syn_without_padding() {
        let events = [
            RawEvent {
                kind: 2,
                code: 11,
                value: -120,
            },
            RawEvent {
                kind: 1,
                code: 30,
                value: 2,
            },
            RawEvent {
                kind: 0,
                code: 0,
                value: 0,
            },
        ];
        let bytes = encode_events(&events).unwrap();
        assert_eq!(bytes.len(), events.len() * EVENT_SIZE);
        assert_eq!(decode_events(&bytes).unwrap(), events);
        for item in bytes.as_chunks::<EVENT_SIZE>().0 {
            assert!(item[..TYPE_OFFSET].iter().all(|byte| *byte == 0));
        }
        assert_eq!(EVENT_SIZE, 24); // Both required targets have the Linux LP64 ABI.
        assert_eq!(SET_PHYS, 0x4008556c); // _IOW('U', 108, char*) on both targets.
        assert_eq!(size_of::<libc::uinput_setup>(), 92);
        assert_eq!(size_of::<libc::uinput_abs_setup>(), 28);
        assert!(encode_events(&[]).is_err());
        assert!(decode_events(&bytes[..bytes.len() - 1]).is_err());
    }

    struct Writes {
        steps: VecDeque<io::Result<usize>>,
        output: Vec<u8>,
    }
    impl Write for Writes {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let length = self.steps.pop_front().unwrap_or(Ok(bytes.len()))?;
            self.output
                .extend_from_slice(&bytes[..length.min(bytes.len())]);
            Ok(length)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn cancelled_key_batch_reports_only_its_written_event_prefix() {
        let events = [
            RawEvent {
                kind: 1,
                code: 42,
                value: 1,
            },
            RawEvent {
                kind: 1,
                code: 30,
                value: 1,
            },
            RawEvent {
                kind: 0,
                code: 0,
                value: 0,
            },
        ];
        for count in 0..=events.len() {
            let mut batch = Batch::events(&events).unwrap();
            let mut writer = Writes {
                steps: if count == 0 {
                    VecDeque::from([Err(io::ErrorKind::WouldBlock.into())])
                } else {
                    VecDeque::from([
                        Ok(count * EVENT_SIZE),
                        Err(io::ErrorKind::WouldBlock.into()),
                    ])
                },
                output: Vec::new(),
            };
            batch.flush(&mut writer).unwrap();
            assert_eq!(batch.written_prefix().unwrap(), events[..count]);
            assert_eq!(
                writer.output,
                encode_events(&events).unwrap()[..count * EVENT_SIZE]
            );
        }
    }

    #[test]
    fn nonblocking_partial_writes_resume_exactly_once_without_replaying_a_press() {
        let bytes = encode_events(&[
            RawEvent {
                kind: 1,
                code: 30,
                value: 1,
            },
            RawEvent {
                kind: 0,
                code: 0,
                value: 0,
            },
        ])
        .unwrap();
        let mut batch = Batch {
            bytes: bytes.clone(),
            written: 0,
        };
        let mut writer = Writes {
            steps: VecDeque::from([Ok(EVENT_SIZE), Err(io::ErrorKind::WouldBlock.into())]),
            output: Vec::new(),
        };
        assert_eq!(batch.flush(&mut writer).unwrap(), Flush::Pending);
        assert_eq!(writer.output.len(), EVENT_SIZE);
        assert_eq!(batch.flush(&mut writer).unwrap(), Flush::Complete);
        assert_eq!(writer.output, bytes);
    }

    #[test]
    fn uncertain_writes_and_eintr_are_bounded_and_never_blindly_retried() {
        let bytes = encode_events(&[RawEvent {
            kind: 0,
            code: 0,
            value: 0,
        }])
        .unwrap();
        let mut batch = Batch {
            bytes: bytes.clone(),
            written: 0,
        };
        let mut writer = Writes {
            steps: VecDeque::from([Ok(1)]),
            output: Vec::new(),
        };
        assert!(batch.flush(&mut writer).is_err());
        let mut batch = Batch { bytes, written: 0 };
        let mut writer = Writes {
            steps: (0..100)
                .map(|_| Err(io::ErrorKind::Interrupted.into()))
                .collect(),
            output: Vec::new(),
        };
        assert_eq!(batch.flush(&mut writer).unwrap(), Flush::Pending);
        assert_eq!(writer.steps.len(), 84);
        assert!(writer.output.is_empty());
    }

    #[test]
    fn repeating_or_fuzzy_sources_fail_before_kernel_creation() {
        let mut device: Device = crate::transport::tests::touch();
        let id = descriptor().identity;
        let fuzzy = Descriptor::mirror(&device, id, None, &BTreeMap::new()).unwrap();
        assert_eq!(
            supported_backend(&fuzzy).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        for axis in device.axes.values_mut() {
            axis.fuzz = 0;
        }
        device.capabilities.insert(20, vec![0, 1]);
        for (delay_ms, period_ms) in [(250, 33), (0, 0), (0, 33), (250, 0), (60_000, 60_000)] {
            let repeat = Descriptor::mirror(
                &device,
                id,
                Some(crate::transport::Repeat {
                    delay_ms,
                    period_ms,
                }),
                &BTreeMap::new(),
            )
            .unwrap();
            assert_eq!(
                supported_backend(&repeat).unwrap_err().kind(),
                io::ErrorKind::Unsupported
            );
        }
        assert!(supported_backend(&descriptor()).is_ok());
    }

    #[test]
    fn a_compositor_repeat_mirror_drops_repeat_setting_events_from_its_frames() {
        let registry = Temporary::new();
        let files = Temporary::new();
        let mut device = crate::transport::tests::touch();
        device.identity = "test:keyboard".into();
        device.name = "Prek test keyboard".into();
        device.physical = "fixture/keyboard".into();
        device.path.clear();
        device.properties.clear();
        device.axes.clear();
        device.capabilities = BTreeMap::from([(0, vec![0]), (1, vec![30]), (20, vec![0, 1])]);
        let identity = KernelIdentity {
            bus: 3,
            vendor: 1,
            product: 2,
            version: 1,
        };
        let descriptor =
            Descriptor::mirror_with_compositor_repeat(&device, identity, &BTreeMap::new()).unwrap();
        assert!(supported_backend(&descriptor).is_ok());
        let publication = registry.registry().prepare_isolated(descriptor).unwrap();
        let path = files.path.join("output");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        let mut output = Virtual::ordinary_file_fixture(publication, file).unwrap();
        let event = |kind, code, value| RawEvent { kind, code, value };
        let frame = Frame {
            source: output.descriptor().source.clone(),
            at: std::time::Duration::ZERO,
            sequence: 1,
            events: vec![
                event(20, 0, 250),
                event(1, 30, 1),
                event(EV_SYN, SYN_REPORT, 0),
            ],
        };
        output.enqueue(&frame).unwrap();
        assert!(matches!(output.flush().unwrap(), Flush::Complete));
        let kinds: Vec<u16> = fs::read(&path)
            .unwrap()
            .chunks_exact(size_of::<libc::input_event>())
            .map(|chunk| {
                u16::from_ne_bytes(
                    chunk[offset_of!(libc::input_event, type_)..][..2]
                        .try_into()
                        .unwrap(),
                )
            })
            .collect();
        assert_eq!(kinds, [1, EV_SYN]);
    }
}
