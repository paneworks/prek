//! Linux virtual-device preparation, metadata import and bounded frame writes.
//!
//! Device creation does not grab any original. Production routing still needs
//! authenticated consumer readiness, a neutral handoff and the supervisor.
//! Test devices require explicit isolation metadata and root-owned pre-add udev
//! import, and must be grabbed by their isolated receiver before any injection.

use crate::{
    input::{EV_SYN, Frame, RawEvent, SYN_REPORT},
    transport::{Descriptor, permitted_property},
};
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
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
};

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

/// The marker is a fixed grammar, never a caller-supplied filesystem path.
fn filename(marker: &str) -> io::Result<CString> {
    let parts: Vec<_> = marker.split('/').collect();
    let ids = match parts.as_slice() {
        ["prek", device, generation] => [*device, *generation],
        ["prek", "test", device, generation] => [*device, *generation],
        _ => return Err(invalid("invalid Prek device marker")),
    };
    if ids.iter().any(|id| {
        id.len() != 16
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    }) {
        return Err(invalid(
            "device markers require canonical 16-digit hexadecimal IDs",
        ));
    }
    CString::new(format!("{}.json", marker.replace('/', "-")))
        .map_err(|_| invalid("invalid marker"))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    version: u32,
    marker: String,
    isolated: bool,
    properties: BTreeMap<String, String>,
}

impl Metadata {
    fn validate(&self, marker: &str) -> io::Result<()> {
        filename(marker)?;
        if self.version != 1
            || self.marker != marker
            || self.properties.len() > 32
            || self.isolated != marker.starts_with("prek/test/")
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
        Ok(Self { file, owner })
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
        metadata.validate(marker)?;
        Ok(metadata)
    }
}

/// A retained directory fd fences path replacement. Production access always
/// requires root ownership; the helper has no caller-supplied registry path.
#[derive(Clone, Debug)]
pub struct Registry {
    directory: Arc<Directory>,
}

impl Registry {
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

    fn publish(&self, descriptor: Descriptor, isolated: bool) -> io::Result<Publication> {
        descriptor.validate().map_err(invalid)?;
        if descriptor.is_isolated() != isolated {
            return Err(denied("descriptor isolation does not match publication"));
        }
        let metadata = Metadata {
            version: 1,
            marker: descriptor.marker.clone(),
            isolated,
            properties: descriptor.udev.clone(),
        };
        metadata.validate(&metadata.marker)?;
        let bytes = serde_json::to_vec(&metadata).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_METADATA {
            return Err(invalid("metadata exceeds size limit"));
        }
        let name = filename(&metadata.marker)?;
        let mut file = self
            .directory
            .open_file(&name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
        let stat = file.metadata()?;
        let publication = Publication {
            directory: Arc::clone(&self.directory),
            name,
            inode: stat.ino(),
            descriptor,
            metadata,
        };
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(publication)
    }

    /// Prepare a physical counterpart's metadata before UI_DEV_CREATE.
    pub fn prepare(&self, descriptor: Descriptor) -> io::Result<Publication> {
        self.publish(descriptor, false)
    }

    /// Only an explicitly authorized isolated test harness may use this path.
    /// Its downstream event receiver must exclusively grab its own virtual
    /// node before injection, so the console also receives no test keystrokes.
    pub fn prepare_isolated(&self, descriptor: Descriptor) -> io::Result<Publication> {
        self.publish(descriptor.for_isolated().map_err(denied)?, true)
    }
}

#[derive(Debug)]
pub struct Publication {
    directory: Arc<Directory>,
    name: CString,
    inode: u64,
    descriptor: Descriptor,
    metadata: Metadata,
}

impl Drop for Publication {
    fn drop(&mut self) {
        // Do not remove a replacement file belonging to a newer publication.
        if let Ok(file) = self
            .directory
            .open_file(&self.name, libc::O_RDONLY | libc::O_NONBLOCK)
            && let Ok(stat) = file.metadata()
            && stat.ino() == self.inode
            && stat.uid() == self.directory.owner
        {
            // SAFETY: owned directory fd and validated single-component name.
            unsafe {
                libc::unlinkat(self.directory.file.as_raw_fd(), self.name.as_ptr(), 0);
            }
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
struct Batch {
    bytes: Vec<u8>,
    written: usize,
}

impl Batch {
    fn flush(&mut self, writer: &mut impl Write) -> io::Result<Flush> {
        // Bound syscall work per loop iteration, including repeated EINTR.
        for _ in 0..16 {
            if self.written == self.bytes.len() {
                return Ok(Flush::Complete);
            }
            match writer.write(&self.bytes[self.written..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "uinput write made no progress",
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
            "repeat passthrough parity is unverified; do not create/grab this device",
        ));
    }
    if descriptor.axes.values().any(|axis| axis.fuzz != 0) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "nonzero fuzz would filter forwarded coordinates twice; compatibility is pending",
        ));
    }
    Ok(())
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
    pub fn create(publication: Publication) -> io::Result<Self> {
        supported_backend(&publication.descriptor)?;
        // Registry metadata must still exist before the device add event.
        if publication.directory.read(&publication.metadata.marker)? != publication.metadata {
            return Err(invalid("published device metadata changed before creation"));
        }
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
        if frame.source != spec.source
            || self
                .sequence
                .is_some_and(|sequence| frame.sequence <= sequence)
            || frame
                .events
                .last()
                .is_none_or(|event| event.kind != EV_SYN || event.code != SYN_REPORT)
            || frame.events[..frame.events.len().saturating_sub(1)]
                .iter()
                .any(|event| event.kind == EV_SYN)
            || frame.events.iter().any(|event| {
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
        self.pending = Some(Batch {
            bytes: encode_events(&frame.events)?,
            written: 0,
        });
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
    use super::*;
    use crate::{input::Device, transport::KernelIdentity};
    use std::{
        collections::VecDeque,
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
            BTreeMap::new(),
        )
        .unwrap()
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
            BTreeMap::new(),
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
        let fuzzy =
            Descriptor::mirror(&device, id, None, &BTreeMap::new(), BTreeMap::new()).unwrap();
        assert_eq!(
            supported_backend(&fuzzy).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        for axis in device.axes.values_mut() {
            axis.fuzz = 0;
        }
        device.capabilities.insert(20, vec![0, 1]);
        let repeat = Descriptor::mirror(
            &device,
            id,
            Some(crate::transport::Repeat {
                delay_ms: 250,
                period_ms: 33,
            }),
            &BTreeMap::new(),
            BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            supported_backend(&repeat).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert!(supported_backend(&descriptor()).is_ok());
    }
}
