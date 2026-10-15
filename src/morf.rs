//! Broker-only native Morf lock endpoint. Lua state and process/socket loss
//! never establish a completed unlock transition.
pub mod controls;
mod shell;
pub use shell::{Phase as ShellPhase, State as ShellState};
pub type ShellClient = Client<ShellState>;

use crate::{
    hyprland,
    session::{Logind, Peer, Registration},
};
use nix::libc;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd},
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const RPC_TIME: Duration = Duration::from_millis(200);
const MAX_REPLY: usize = 2048;
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Pending,
    Locked,
    UnlockDispatched,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub version: u32,
    pub role: String,
    pub epoch: String,
    pub revision: u64,
    pub phase: Phase,
    pub compositor_pid: u32,
    pub compositor_start: u64,
    pub compositor_uid: u32,
}
impl State {
    fn validate(&self) -> io::Result<()> {
        let revision = match self.phase {
            Phase::Pending => 1,
            Phase::Locked => 2,
            Phase::UnlockDispatched => 3,
        };
        if self.version != 1
            || self.role != "lock"
            || self.revision != revision
            || self.epoch.len() != 32
            || !self.epoch.bytes().all(|byte| byte.is_ascii_hexdigit())
            || self.compositor_pid == 0
            || self.compositor_start == 0
        {
            return Err(invalid("invalid native Morf lock state"));
        }
        Ok(())
    }
}
mod sealed {
    pub trait Sealed {}
}
/// Implemented only by the native lock and shell wire states.
pub trait NativeState: sealed::Sealed + serde::de::DeserializeOwned + Clone + Eq {
    const SUFFIX: &'static str;
    fn validate(&self) -> io::Result<()>;
    fn compositor(&self) -> (u32, u64, u32);
    fn epoch(&self) -> &str;
    fn revision(&self) -> u64;
    fn preserves(&self, old: &Self) -> bool {
        self.epoch() == old.epoch()
            && self.revision() >= old.revision()
            && (self.revision() != old.revision() || self == old)
    }
}
impl sealed::Sealed for State {}
impl NativeState for State {
    const SUFFIX: &'static str = "-lock-provider.sock";
    fn validate(&self) -> io::Result<()> {
        State::validate(self)
    }
    fn compositor(&self) -> (u32, u64, u32) {
        (
            self.compositor_pid,
            self.compositor_start,
            self.compositor_uid,
        )
    }
    fn epoch(&self) -> &str {
        &self.epoch
    }
    fn revision(&self) -> u64 {
        self.revision
    }
}
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Proof {
    pid: u32,
    start: u64,
    epoch: String,
}
pub struct Client<S: NativeState = State> {
    directory: File,
    name: String,
    stream: UnixStream,
    inode: u64,
    uid: u32,
    pid: u32,
    start: u64,
    executable: (u64, u64),
    registration: Registration,
    last: Option<S>,
}
impl<S: NativeState> Client<S> {
    pub(crate) fn last_state(&self) -> Option<&S> {
        self.last.as_ref()
    }
    pub fn discover(
        runtime: &Path,
        uid: u32,
        executable: &Path,
        logind: &Logind,
        compositor: &hyprland::Client,
    ) -> io::Result<Option<Self>> {
        let mut found = Self::discover_all(runtime, uid, executable, logind, compositor)?;
        if found.len() > 1 {
            return Err(denied("ambiguous native providers"));
        }
        Ok(found.pop())
    }
    pub fn discover_all(
        runtime: &Path,
        uid: u32,
        executable: &Path,
        logind: &Logind,
        compositor: &hyprland::Client,
    ) -> io::Result<Vec<Self>> {
        let runtime = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(runtime)?;
        let metadata = runtime.metadata()?;
        if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(denied("native provider runtime is not private to its user"));
        }
        let directory = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(PathBuf::from(format!("/proc/self/fd/{}", runtime.as_raw_fd())).join("morf"))
        {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let metadata = directory.metadata()?;
        // Morf's shared registry can be readable inside its private runtime;
        // only its owner may mutate it. Each native socket remains mode 0600.
        if metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
            return Err(denied(
                "native Morf provider directory is not private to its user",
            ));
        }
        let root = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let mut names = Vec::new();
        for entry in fs::read_dir(&root)?.take(65) {
            let name = entry?.file_name();
            if names.len() >= 64 {
                return Err(invalid("too many native provider directory entries"));
            }
            names.push(name);
        }
        let mut result = Vec::new();
        let until = Instant::now() + RPC_TIME;
        for name in names {
            if Instant::now() >= until {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let Some(name) = name.to_str().filter(|name| name.ends_with(S::SUFFIX)) else {
                continue;
            };
            if name.len() > 128
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
            {
                return Err(invalid("invalid native Morf provider name"));
            }
            let stat = match fs::symlink_metadata(root.join(name)) {
                Ok(stat) => stat,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if !stat.file_type().is_socket() || stat.uid() != uid || stat.mode() & 0o077 != 0 {
                return Err(denied("native Morf provider endpoint is not private"));
            }
            let Some(stream) = connect_provider(&root.join(name), until)? else {
                continue;
            };
            let peer = Peer::from_socket(&stream)?;
            if peer.uid() != uid {
                return Err(denied("native provider peer UID differs"));
            }
            let actual = fs::metadata(format!("/proc/{}/exe", peer.pid()))?;
            let expected = fs::metadata(executable)?;
            if (actual.dev(), actual.ino()) != (expected.dev(), expected.ino()) {
                return Err(denied("native provider executable differs from policy"));
            }
            let pid = peer.pid();
            let start = birth(pid)?;
            let registration = match logind.register(peer, compositor) {
                Ok(registration) => registration,
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => continue,
                Err(error) => return Err(error),
            };
            let mut client = Self {
                directory: directory.try_clone()?,
                name: name.into(),
                stream,
                inode: stat.ino(),
                uid,
                pid,
                start,
                executable: (actual.dev(), actual.ino()),
                registration,
                last: None,
            };
            match client.sample_until(logind, compositor, until) {
                Ok(_) => {}
                Err(error)
                    if error.kind() == io::ErrorKind::PermissionDenied
                        && error.to_string() == "native provider serves another compositor" =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
            result.push(client);
        }
        Ok(result)
    }
    fn verify(&self) -> io::Result<()> {
        let path =
            PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd())).join(&self.name);
        let stat = fs::symlink_metadata(path)?;
        let actual = fs::metadata(format!("/proc/{}/exe", self.pid))?;
        if !stat.file_type().is_socket()
            || stat.ino() != self.inode
            || stat.uid() != self.uid
            || stat.mode() & 0o077 != 0
            || birth(self.pid)? != self.start
            || (actual.dev(), actual.ino()) != self.executable
        {
            return Err(denied("native Morf provider binding changed"));
        }
        Ok(())
    }
    pub fn sample(&mut self, logind: &Logind, compositor: &hyprland::Client) -> io::Result<S> {
        self.sample_until(logind, compositor, Instant::now() + RPC_TIME)
    }
    pub(crate) fn sample_until(
        &mut self,
        logind: &Logind,
        compositor: &hyprland::Client,
        until: Instant,
    ) -> io::Result<S> {
        self.verify()?;
        self.registration.check(logind, compositor)?;
        self.write(b"{\"version\":1,\"op\":\"state\"}\n", until)?;
        let mut bytes = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if Instant::now() >= until {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match self.stream.read(&mut byte) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => {
                    if bytes.len() >= MAX_REPLY {
                        return Err(invalid("native provider reply exceeds limit"));
                    }
                    bytes.push(byte[0]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    wait(self.stream.as_raw_fd(), libc::POLLIN, until)?
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        let state: S = serde_json::from_slice(&bytes)?;
        state.validate()?;
        self.registration.check(logind, compositor)?;
        self.verify()?;
        let binding = self.registration.binding()?;
        if state.compositor()
            != (
                binding.compositor_pid,
                binding.compositor_start,
                binding.uid,
            )
        {
            return Err(denied("native provider serves another compositor"));
        }
        if self.last.as_ref().is_some_and(|old| !state.preserves(old)) {
            return Err(denied(
                "native provider transition regressed or changed identity",
            ));
        }
        self.last = Some(state.clone());
        Ok(state)
    }
    fn write(&mut self, bytes: &[u8], until: Instant) -> io::Result<()> {
        let mut pending = bytes;
        while !pending.is_empty() {
            if Instant::now() >= until {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match self.stream.write(pending) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(count) => pending = &pending[count..],
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    wait(self.stream.as_raw_fd(), libc::POLLOUT, until)?
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}
impl Client<State> {
    pub(crate) fn proof(&self, state: &State) -> Proof {
        Proof {
            pid: self.pid,
            start: self.start,
            epoch: state.epoch.clone(),
        }
    }
    /// Called after the broker committed the verified unlock transition. The
    /// provider may terminate immediately; its exit is never an unlock proof.
    pub(crate) fn release(&mut self, state: &State) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(&serde_json::json!({"version": 1, "op": "release",
            "epoch": state.epoch, "revision": state.revision}))?;
        bytes.push(b'\n');
        self.write(&bytes, Instant::now() + RPC_TIME)
    }
}
impl<S: NativeState> AsFd for Client<S> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }
}
fn connect_provider(path: &Path, until: Instant) -> io::Result<Option<UnixStream>> {
    match hyprland::connect(path, until) {
        Ok(stream) => Ok(Some(stream)),
        // SIGKILL can leave an inert native socket inode. It supplies no
        // evidence and must not hide another live provider in this registry.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
fn birth(pid: u32) -> io::Result<u64> {
    fs::read_to_string(format!("/proc/{pid}/stat"))?
        .rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| invalid("native provider birth is missing"))
}
fn wait(fd: i32, events: i16, until: Instant) -> io::Result<()> {
    let remaining = until.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::ErrorKind::TimedOut.into());
    }
    let mut descriptor = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    // SAFETY: initialized live descriptor and bounded timeout.
    let result = unsafe {
        libc::poll(
            &mut descriptor,
            1,
            remaining.as_millis().max(1).min(i32::MAX as u128) as i32,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if result == 0 {
        return Err(io::ErrorKind::TimedOut.into());
    }
    Ok(())
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dead_native_socket_inodes_are_skipped_without_removing_user_paths() {
        use std::os::unix::{fs::DirBuilderExt, net::UnixListener};
        let directory = PathBuf::from(format!("/tmp/prek-morf-dead-{}", std::process::id()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let dead = directory.join("dead.sock");
        let listener = UnixListener::bind(&dead).unwrap();
        drop(listener);
        assert!(
            connect_provider(&dead, Instant::now() + RPC_TIME)
                .unwrap()
                .is_none()
        );
        assert!(dead.exists());
        let live = directory.join("live.sock");
        let _listener = UnixListener::bind(&live).unwrap();
        assert!(
            connect_provider(&live, Instant::now() + RPC_TIME)
                .unwrap()
                .is_some()
        );
        assert!(
            connect_provider(&directory.join("missing.sock"), Instant::now() + RPC_TIME)
                .unwrap()
                .is_none()
        );
        fs::remove_dir_all(directory).unwrap();
    }
    fn value() -> serde_json::Value {
        serde_json::json!({"version": 1, "role": "lock", "epoch": "a".repeat(32),
            "revision": 2, "phase": "locked", "compositor_pid": 42,
            "compositor_start": 123, "compositor_uid": 1000})
    }
    #[test]
    fn native_schema_requires_acknowledged_transition_order_and_compositor_identity() {
        let state: State = serde_json::from_value(value()).unwrap();
        state.validate().unwrap();
        for (field, replacement) in [
            ("version", serde_json::json!(2)),
            ("role", serde_json::json!("shell")),
            ("revision", serde_json::json!(1)),
            ("compositor_pid", serde_json::json!(0)),
            ("compositor_start", serde_json::json!(0)),
            ("epoch", serde_json::json!("forged")),
        ] {
            let mut invalid = value();
            invalid[field] = replacement;
            assert!(
                serde_json::from_value::<State>(invalid)
                    .unwrap()
                    .validate()
                    .is_err(),
                "accepted {field}"
            );
        }
        let mut invalid = value();
        invalid["phase"] = serde_json::json!("unlock_dispatched");
        assert!(
            serde_json::from_value::<State>(invalid)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut unknown = value();
        unknown["claimed_uid"] = serde_json::json!(0);
        assert!(serde_json::from_value::<State>(unknown).is_err());
        let mut fake_hint = value();
        fake_hint["phase"] = serde_json::json!("unlocked");
        assert!(serde_json::from_value::<State>(fake_hint).is_err());
    }
    #[test]
    fn provider_proof_is_bound_to_process_birth_and_epoch() {
        let proof = Proof {
            pid: 42,
            start: 123,
            epoch: "a".repeat(32),
        };
        assert_ne!(
            proof,
            Proof {
                start: 124,
                ..proof.clone()
            }
        );
        assert_ne!(
            proof,
            Proof {
                pid: 43,
                ..proof.clone()
            }
        );
        assert_ne!(
            proof,
            Proof {
                epoch: "b".repeat(32),
                ..proof
            }
        );
    }
}
