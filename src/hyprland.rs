//! Bounded IPC against a verified Hyprland process. Socket/process ownership is
//! checked here; active logind seat/session authority belongs to registration.
//! Queries never grab devices, inject events, or change compositor settings.

use crate::transport::{Class, Consumer, ConsumerEvidence, Descriptor};
use nix::libc;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    mem::{offset_of, size_of},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const MAX_REPLY: usize = 2 * 1024 * 1024;
const REQUEST_TIME: Duration = Duration::from_millis(200);

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}
impl Version {
    fn parse(value: &Value) -> io::Result<Self> {
        let text = value
            .get("version")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("Hyprland version is missing"))?;
        let parts: Vec<_> = text.split('.').collect();
        let [major, minor, patch] = parts.as_slice() else {
            return Err(invalid("unrecognized Hyprland version"));
        };
        let version = Self {
            major: major
                .parse()
                .map_err(|_| invalid("invalid Hyprland major version"))?,
            minor: minor
                .parse()
                .map_err(|_| invalid("invalid Hyprland minor version"))?,
            patch: patch
                .parse()
                .map_err(|_| invalid("invalid Hyprland patch version"))?,
        };
        if version.major != 0 || !matches!(version.minor, 54 | 55) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Hyprland device schema is not verified for this version",
            ));
        }
        Ok(version)
    }
}

fn birth(pid: u32) -> io::Result<u64> {
    fs::read_to_string(format!("/proc/{pid}/stat"))?
        .rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| invalid("compositor process identity is missing"))
}

fn wait(fd: i32, events: i16, until: Instant) -> io::Result<()> {
    loop {
        let remaining = until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let mut poll = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: one initialized poll descriptor with a bounded timeout.
        let result = unsafe {
            libc::poll(
                &mut poll,
                1,
                remaining.as_millis().max(1).min(i32::MAX as u128) as i32,
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result == 0 {
            return Err(io::ErrorKind::TimedOut.into());
        }
        if poll.revents & libc::POLLNVAL != 0 {
            return Err(invalid("IPC descriptor is invalid"));
        }
        return Ok(());
    }
}

fn connect(path: &Path, until: Instant) -> io::Result<UnixStream> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(invalid("IPC socket path exceeds bounds"));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    // SAFETY: no pointers; successful socket creates a new owned descriptor.
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
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let length = (offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    // SAFETY: correctly initialized address and precise bounded length.
    if unsafe { libc::connect(raw, (&address as *const libc::sockaddr_un).cast(), length) } < 0 {
        let error = io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::EINPROGRESS) | Some(libc::EAGAIN)
        ) {
            return Err(error);
        }
        wait(raw, libc::POLLOUT, until)?;
        let mut status: libc::c_int = 0;
        let mut length = size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: live socket and initialized integer status output.
        if unsafe {
            libc::getsockopt(
                raw,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut status as *mut libc::c_int).cast(),
                &mut length,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
    }
    Ok(UnixStream::from(fd))
}

/// A policy-selected compositor executable and private instance directory.
/// The caller supplies the UID/executable from trusted registration policy,
/// rather than accepting either from an unverified client claim.
#[derive(Debug)]
pub struct Client {
    directory: File,
    socket_inode: u64,
    uid: u32,
    pid: u32,
    start: u64,
    executable: (u64, u64),
    instance: String,
    version: Version,
}

impl Client {
    pub fn open(runtime: &Path, instance: &str, uid: u32, executable: &Path) -> io::Result<Self> {
        if !runtime.is_absolute()
            || instance.is_empty()
            || instance.len() > 160
            || !instance
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
            || matches!(instance, "." | "..")
        {
            return Err(invalid("invalid compositor instance path"));
        }
        let private = |path: &Path| -> io::Result<File> {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)?;
            let stat = file.metadata()?;
            if stat.uid() != uid || stat.mode() & 0o077 != 0 {
                return Err(denied(
                    "compositor directory must be private to its registered UID",
                ));
            }
            Ok(file)
        };
        let runtime = private(runtime)?;
        let hypr =
            private(&PathBuf::from(format!("/proc/self/fd/{}", runtime.as_raw_fd())).join("hypr"))?;
        let directory =
            private(&PathBuf::from(format!("/proc/self/fd/{}", hypr.as_raw_fd())).join(instance))?;
        let socket_path =
            PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(".socket.sock");
        let stat = fs::symlink_metadata(&socket_path)?;
        if !stat.file_type().is_socket() || stat.uid() != uid {
            return Err(denied("compositor endpoint must be its owned Unix socket"));
        }
        let stream = connect(&socket_path, Instant::now() + REQUEST_TIME)?;
        let credentials = credentials(&stream)?;
        if credentials.uid != uid || credentials.pid <= 0 {
            return Err(denied(
                "compositor socket peer does not match registered UID",
            ));
        }
        let expected = fs::metadata(executable)?;
        let actual = fs::metadata(format!("/proc/{}/exe", credentials.pid))?;
        if (actual.dev(), actual.ino()) != (expected.dev(), expected.ino()) {
            return Err(denied(
                "compositor executable differs from registration policy",
            ));
        }
        let pid = credentials.pid as u32;
        let mut client = Self {
            directory,
            socket_inode: stat.ino(),
            uid,
            pid,
            start: birth(pid)?,
            executable: (expected.dev(), expected.ino()),
            instance: instance.into(),
            version: Version {
                major: 0,
                minor: 0,
                patch: 0,
            },
        };
        // The initial connection carries no request. Every real request verifies
        // the same peer, executable, birth and endpoint before and after I/O.
        drop(stream);
        client.version = Version::parse(&client.query("version")?)?;
        Ok(client)
    }

    fn socket_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd())).join(".socket.sock")
    }
    fn verify(&self, stream: &UnixStream) -> io::Result<()> {
        let stat = fs::symlink_metadata(self.socket_path())?;
        let peer = credentials(stream)?;
        let executable = fs::metadata(format!("/proc/{}/exe", self.pid))?;
        if !stat.file_type().is_socket()
            || stat.ino() != self.socket_inode
            || stat.uid() != self.uid
            || peer.pid != self.pid as i32
            || peer.uid != self.uid
            || birth(self.pid)? != self.start
            || (executable.dev(), executable.ino()) != self.executable
        {
            return Err(denied("compositor binding changed"));
        }
        Ok(())
    }

    fn query(&self, kind: &str) -> io::Result<Value> {
        if !matches!(
            kind,
            "version" | "devices" | "monitors" | "clients" | "activewindow"
        ) {
            return Err(invalid("unsupported compositor query"));
        }
        let until = Instant::now() + REQUEST_TIME;
        let mut stream = connect(&self.socket_path(), until)?;
        self.verify(&stream)?;
        let request = format!("j/{kind}");
        let mut pending = request.as_bytes();
        while !pending.is_empty() {
            match stream.write(pending) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(written) => pending = &pending[written..],
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    wait(stream.as_raw_fd(), libc::POLLOUT, until)?
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            if Instant::now() >= until {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if bytes.len() + read > MAX_REPLY {
                        return Err(invalid("compositor reply exceeds bounds"));
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    wait(stream.as_raw_fd(), libc::POLLIN, until)?
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        self.verify(&stream)?;
        serde_json::from_slice(&bytes).map_err(io::Error::other)
    }

    pub fn version(&self) -> &Version {
        &self.version
    }
    /// Recheck the retained socket, peer, process birth and executable without
    /// reading desktop state. Registration uses this around logind inspection.
    pub fn check_binding(&self) -> io::Result<()> {
        let stream = connect(&self.socket_path(), Instant::now() + REQUEST_TIME)?;
        self.verify(&stream)
    }
    pub fn uid(&self) -> u32 {
        self.uid
    }
    pub fn consumer(&self) -> Consumer {
        Consumer::Hyprland {
            pid: self.pid,
            start: self.start,
            instance: self.instance.clone(),
        }
    }
    pub fn inventory(&self) -> io::Result<Value> {
        let inventory = self.query("devices")?;
        validate_inventory(&inventory)?;
        Ok(inventory)
    }

    /// Only returned for a unique accepted device and settings actually exposed
    /// by this stock IPC schema. This does not supply logind authority or prove
    /// kernel metadata/neutrality; the broker still performs those gates.
    pub fn device_evidence(
        &self,
        descriptor: &Descriptor,
        at: Duration,
    ) -> io::Result<Option<ConsumerEvidence>> {
        descriptor.validate().map_err(invalid)?;
        let inventory = self.inventory()?;
        let (group, name) = match descriptor.class {
            Class::Keyboard | Class::Buttons => ("keyboards", internal_name(&descriptor.name)),
            Class::Mouse | Class::Touchpad => ("mice", internal_name(&descriptor.name)),
            Class::Touchscreen => ("touch", internal_name(&descriptor.name)),
            Class::Switches => ("switches", descriptor.name.clone()),
        };
        let matches: Vec<_> = inventory[group]
            .as_array()
            .unwrap()
            .iter()
            .filter(|device| {
                let candidate = device["name"].as_str().unwrap();
                candidate == name
                    || candidate
                        .strip_prefix(&format!("{name}-"))
                        .is_some_and(|suffix| {
                            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                        })
            })
            .collect();
        if matches.is_empty() {
            return Ok(None);
        }
        if matches.len() != 1 || matches[0]["name"].as_str() != Some(&name) {
            return Err(invalid(
                "virtual compositor identity is ambiguous or renamed",
            ));
        }
        let settings = verified_settings(matches[0], descriptor)?;
        Ok(Some(ConsumerEvidence {
            consumer: self.consumer(),
            marker: descriptor.marker.clone(),
            name: descriptor.name.clone(),
            class: descriptor.class,
            seat: descriptor.source.seat.clone(),
            settings,
            at,
        }))
    }
}

fn credentials(stream: &UnixStream) -> io::Result<libc::ucred> {
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: initialized credential output for the live connected socket.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut peer as *mut libc::ucred).cast(),
            &mut length,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    if length as usize != size_of::<libc::ucred>() {
        return Err(invalid("invalid compositor peer credentials"));
    }
    Ok(peer)
}

fn internal_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if matches!(character, ' ' | '\n' | ',') {
                '-'
            } else {
                character.to_ascii_lowercase()
            }
        })
        .collect()
}
fn validate_inventory(value: &Value) -> io::Result<()> {
    for group in ["keyboards", "mice", "touch", "switches", "tablets"] {
        let list = value
            .get(group)
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("unrecognized compositor device inventory"))?;
        if list.len() > 1024 {
            return Err(invalid("compositor inventory exceeds device bounds"));
        }
        for device in list {
            if group == "tablets" {
                continue;
            }
            let name = device.get("name").and_then(Value::as_str).filter(|name| {
                !name.is_empty() && name.len() <= 1024 && !name.chars().any(char::is_control)
            });
            let address = device
                .get("address")
                .and_then(Value::as_str)
                .and_then(|value| value.strip_prefix("0x"))
                .and_then(|value| u64::from_str_radix(value, 16).ok())
                .filter(|address| *address > 0);
            if name.is_none() || address.is_none() {
                return Err(invalid("invalid compositor device identity"));
            }
        }
    }
    Ok(())
}
fn verified_settings(
    device: &Value,
    descriptor: &Descriptor,
) -> io::Result<BTreeMap<String, String>> {
    let mut settings = BTreeMap::new();
    for (key, expected) in &descriptor.consumer_settings {
        let field = match (descriptor.class, key.as_str()) {
            (Class::Keyboard | Class::Buttons, "kb_rules") => "rules",
            (Class::Keyboard | Class::Buttons, "kb_model") => "model",
            (Class::Keyboard | Class::Buttons, "kb_layout") => "layout",
            (Class::Keyboard | Class::Buttons, "kb_variant") => "variant",
            (Class::Keyboard | Class::Buttons, "kb_options") => "options",
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "required device setting is not verifiable through stock compositor IPC",
                ));
            }
        };
        let actual = device
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("required compositor setting is missing"))?;
        if actual != expected {
            return Err(invalid(
                "effective compositor setting differs from device policy",
            ));
        }
        settings.insert(key.clone(), actual.into());
    }
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions_are_explicit_and_unknown_schemas_refuse() {
        for version in ["0.54.3", "0.55.4"] {
            assert!(Version::parse(&serde_json::json!({"version":version})).is_ok());
        }
        for version in ["0.56.0", "0.55", "1.0.0", "0.55.4-unknown"] {
            assert!(Version::parse(&serde_json::json!({"version":version})).is_err());
        }
    }
    #[test]
    fn inventory_requires_complete_typed_device_ids() {
        let mut value =
            serde_json::json!({"keyboards":[],"mice":[],"touch":[],"switches":[],"tablets":[]});
        assert!(validate_inventory(&value).is_ok());
        value["touch"] = serde_json::json!([{"name":"test","address":"0x0"}]);
        assert!(validate_inventory(&value).is_err());
        value["touch"] = serde_json::json!([{"name":"test","address":"0x1234"}]);
        assert!(validate_inventory(&value).is_ok());
        assert_eq!(
            internal_name("Prek virtual 0001 Touch, Pad"),
            "prek-virtual-0001-touch--pad"
        );
    }
}
