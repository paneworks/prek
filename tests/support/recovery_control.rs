//! Retained control directories and pinned processes for isolated recovery tests.
#![allow(dead_code)]
use nix::libc;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    ffi::CString,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
};

const MAX_BYTES: u64 = 64 * 1024;
fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}
fn component(name: &str) -> io::Result<CString> {
    if name.is_empty()
        || name.len() > 128
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(invalid("invalid recovery control component"));
    }
    CString::new(name).map_err(io::Error::other)
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Identity(u64, u64);
impl Identity {
    fn of(metadata: &Metadata) -> Self {
        Self(metadata.dev(), metadata.ino())
    }
    pub fn encode(self) -> String {
        format!("{}:{}", self.0, self.1)
    }
    pub fn parse(text: &str) -> io::Result<Self> {
        let (device, inode) = text
            .split_once(':')
            .ok_or_else(|| invalid("missing recovery directory identity"))?;
        let identity = Self(
            device.parse().map_err(io::Error::other)?,
            inode.parse().map_err(io::Error::other)?,
        );
        if identity.1 == 0 || identity.encode() != text {
            return Err(invalid("invalid recovery directory identity"));
        }
        Ok(identity)
    }
}
pub struct Directory {
    file: File,
    path: PathBuf,
    owner: u32,
    observed: RefCell<BTreeMap<String, Identity>>,
}
impl Directory {
    pub fn from_env() -> io::Result<Self> {
        let path = std::env::var_os("PREK_RECOVERY_DIRECTORY")
            .ok_or_else(|| invalid("missing owned recovery directory"))?;
        let identity =
            Identity::parse(&std::env::var("PREK_RECOVERY_IDENTITY").map_err(io::Error::other)?)?;
        Self::open(Path::new(&path), 0, identity)
    }
    pub fn open(path: &Path, owner: u32, identity: Identity) -> io::Result<Self> {
        if !path.is_absolute()
            || path
                .components()
                .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
        {
            return Err(invalid(
                "recovery directory must be an absolute trusted path",
            ));
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/")?;
        for part in path.components().skip(1) {
            let Component::Normal(part) = part else {
                return Err(invalid("invalid recovery path"));
            };
            let name = CString::new(part.as_encoded_bytes()).map_err(io::Error::other)?;
            // SAFETY: retained parent directory and bounded terminated component.
            let fd = unsafe {
                libc::openat(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful openat returned a new owned descriptor.
            file = unsafe { File::from_raw_fd(fd) };
            let metadata = file.metadata()?;
            if metadata.uid() != owner || metadata.mode() & 0o022 != 0 {
                return Err(invalid("untrusted recovery path owner or permissions"));
            }
        }
        let directory = Self::from_file(file, path.to_owned(), owner)?;
        if directory.identity()? != identity {
            return Err(invalid("recovery directory was replaced"));
        }
        Ok(directory)
    }
    pub fn from_file(file: File, path: PathBuf, owner: u32) -> io::Result<Self> {
        let metadata = file.metadata()?;
        if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o777 != 0o700 {
            return Err(invalid(
                "recovery control directory must be private and owned",
            ));
        }
        Ok(Self {
            file,
            path,
            owner,
            observed: RefCell::new(BTreeMap::new()),
        })
    }
    pub fn identity(&self) -> io::Result<Identity> {
        Ok(Identity::of(&self.file.metadata()?))
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    fn open_file(&self, name: &str, flags: i32) -> io::Result<File> {
        let name = component(name)?;
        // SAFETY: retained directory and terminated single component.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openat returned a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    fn inspect(&self, file: &File) -> io::Result<Identity> {
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != self.owner
            || metadata.mode() & 0o777 != 0o600
            || metadata.nlink() != 1
            || metadata.len() > MAX_BYTES
        {
            return Err(invalid("untrusted recovery control file"));
        }
        Ok(Identity::of(&metadata))
    }
    fn observe(&self, name: &str, file: &File) -> io::Result<()> {
        let identity = self.inspect(file)?;
        let mut observed = self.observed.borrow_mut();
        if let Some(previous) = observed.get(name) {
            if *previous != identity {
                return Err(invalid("recovery control file was replaced"));
            }
        } else {
            if observed.len() >= 8 {
                return Err(invalid("recovery control file budget exceeded"));
            }
            observed.insert(name.to_owned(), identity);
        }
        Ok(())
    }
    pub fn write<T: Serialize>(&self, name: &str, value: &T) -> io::Result<()> {
        let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(invalid("recovery control value exceeds bound"));
        }
        if self.observed.borrow().len() >= 8 {
            return Err(invalid("recovery control file budget exceeded"));
        }
        let mut file = self.open_file(name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
        self.observe(name, &file)?;
        file.write_all(&bytes)?;
        file.sync_all()
    }
    pub fn read<T: DeserializeOwned>(&self, name: &str) -> io::Result<Option<T>> {
        let file = match self.open_file(name, libc::O_RDONLY) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        self.observe(name, &file)?;
        let mut bytes = vec![];
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(invalid("recovery control file grew beyond bound"));
        }
        match serde_json::from_slice(&bytes) {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.is_eof() => Ok(None),
            Err(error) => Err(io::Error::other(error)),
        }
    }
    pub fn child(&self, name: &str) -> io::Result<Self> {
        let name_c = component(name)?;
        // SAFETY: retained parent directory and terminated component.
        if unsafe { libc::mkdirat(self.file.as_raw_fd(), name_c.as_ptr(), 0o700) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = self.open_file(name, libc::O_RDONLY | libc::O_DIRECTORY)?;
        Self::from_file(file, self.path.join(name), self.owner)
    }
    pub fn remove_child(&self, name: &str, identity: Identity) -> io::Result<()> {
        let child = self.open_file(name, libc::O_RDONLY | libc::O_DIRECTORY)?;
        if Identity::of(&child.metadata()?) != identity {
            return Err(invalid("recovery case directory was replaced"));
        }
        let name = component(name)?;
        // SAFETY: verified empty child directory under the retained parent.
        if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    pub fn cleanup(&self) -> io::Result<()> {
        let entries: Vec<_> = fs::read_dir(format!("/proc/self/fd/{}", self.file.as_raw_fd()))?
            .take(9)
            .collect::<Result<_, _>>()?;
        let observed = self.observed.borrow();
        if entries.len() != observed.len() {
            return Err(invalid("unknown recovery control files remain"));
        }
        for entry in &entries {
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid("invalid control filename"))?;
            let file = self.open_file(&name, libc::O_RDONLY)?;
            if observed.get(&name) != Some(&self.inspect(&file)?) {
                return Err(invalid("replaced control file retained"));
            }
        }
        for (name, identity) in observed.iter() {
            let file = self.open_file(name, libc::O_RDONLY)?;
            if Identity::of(&file.metadata()?) != *identity {
                return Err(invalid("control file changed during cleanup"));
            }
            let name = component(name)?;
            // SAFETY: verified owned file under the retained private directory.
            if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

pub struct Process(OwnedFd);
impl Process {
    pub fn pin(pid: u32, birth: Option<u64>) -> io::Result<Self> {
        if pid <= 1 || pid > i32::MAX as u32 {
            return Err(invalid("invalid owned process identity"));
        }
        // SAFETY: bounded PID and zero pidfd_open flags.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful pidfd_open returned a new owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        if let Some(birth) = birth {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
            let actual = stat
                .rsplit_once(')')
                .and_then(|(_, fields)| fields.split_whitespace().nth(19))
                .and_then(|field| field.parse::<u64>().ok());
            if actual != Some(birth) {
                return Err(invalid("owned worker process birth differs"));
            }
        }
        Ok(Self(fd))
    }
    pub fn signal(&self, signal: i32) -> io::Result<()> {
        // SAFETY: retained process handle, null siginfo and zero flags.
        if unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
