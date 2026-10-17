use super::*;
use std::io::Read;

const DATABASE: &str = "/run/udev/data";
const MAX_DATABASE: u64 = 65_536;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Node {
    device: u64,
    inode: u64,
    number: u64,
    kind: u32,
}
impl Node {
    fn read(fd: BorrowedFd<'_>) -> io::Result<Self> {
        let stat = nix::sys::stat::fstat(fd)?;
        Ok(Self {
            device: stat.st_dev,
            inode: stat.st_ino,
            number: stat.st_rdev,
            kind: stat.st_mode & libc::S_IFMT,
        })
    }
    fn at_path(&self, path: &Path) -> bool {
        fs::metadata(path).is_ok_and(|stat| {
            self.device == stat.dev()
                && self.inode == stat.ino()
                && self.number == stat.rdev()
                && self.kind == stat.mode() & libc::S_IFMT
        })
    }
    fn database(&self, root: &Path) -> PathBuf {
        root.join(format!(
            "c{}:{}",
            libc::major(self.number),
            libc::minor(self.number)
        ))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Stamp {
    device: u64,
    inode: u64,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
impl Stamp {
    fn protected(stat: &fs::Metadata, owner: u32) -> io::Result<Self> {
        if !stat.is_file() || stat.uid() != owner || stat.mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "udev database must be a protected root-owned regular file",
            ));
        }
        Ok(Self {
            device: stat.dev(),
            inode: stat.ino(),
            length: stat.len(),
            modified: (stat.mtime(), stat.mtime_nsec()),
            changed: (stat.ctime(), stat.ctime_nsec()),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Publication {
    node: Node,
    stamp: Option<Stamp>,
    pub(super) initialized: bool,
    pub(super) properties: BTreeMap<String, String>,
}
impl Publication {
    pub(super) fn read(fd: BorrowedFd<'_>) -> io::Result<Self> {
        Self::read_at(fd, Path::new(DATABASE), 0)
    }
    fn read_at(fd: BorrowedFd<'_>, root: &Path, owner: u32) -> io::Result<Self> {
        let node = Node::read(fd)?;
        let mut value = Self {
            node,
            stamp: None,
            initialized: false,
            properties: BTreeMap::new(),
        };
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(value.node.database(root))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(value),
            Err(error) => return Err(error),
        };
        let before = Stamp::protected(&file.metadata()?, owner)?;
        let mut bytes = Vec::new();
        (&file).take(MAX_DATABASE + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_DATABASE
            || before != Stamp::protected(&file.metadata()?, owner)?
        {
            return Err(invalid("udev database changed or exceeds its read bound"));
        }
        let text =
            std::str::from_utf8(&bytes).map_err(|_| invalid("invalid udev database text"))?;
        let (initialized, properties) = parse(text)?;
        value.stamp = Some(before);
        value.initialized = initialized;
        value.properties = properties;
        if !value.current_at(fd, None, root, owner)? {
            return Err(invalid("udev database changed during discovery"));
        }
        Ok(value)
    }
    pub(super) fn current(&self, fd: BorrowedFd<'_>, path: &Path) -> bool {
        self.current_at(fd, Some(path), Path::new(DATABASE), 0)
            .unwrap_or(false)
    }
    pub(super) fn admits(&self, expected: &Self, device: &Device, scanned: &Device) -> bool {
        self.initialized
            && expected.initialized
            && self == expected
            && same_description(device, scanned)
    }
    fn current_at(
        &self,
        fd: BorrowedFd<'_>,
        path: Option<&Path>,
        root: &Path,
        owner: u32,
    ) -> io::Result<bool> {
        if Node::read(fd)? != self.node || path.is_some_and(|path| !self.node.at_path(path)) {
            return Ok(false);
        }
        let stamp = match fs::symlink_metadata(self.node.database(root)) {
            Ok(stat) => Some(Stamp::protected(&stat, owner)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        Ok(stamp == self.stamp)
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn parse(text: &str) -> io::Result<(bool, BTreeMap<String, String>)> {
    if text.len() as u64 > MAX_DATABASE
        || text.contains('\0')
        || !text.is_empty() && !text.ends_with('\n')
    {
        return Err(invalid("invalid or incomplete udev database"));
    }
    let mut initialized = None;
    let mut properties = BTreeMap::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("I:") {
            if initialized.is_some()
                || value.is_empty()
                || !value.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(invalid("invalid udev initialization record"));
            }
            initialized = Some(
                value
                    .parse::<u64>()
                    .map_err(|_| invalid("invalid udev initialization time"))?
                    != 0,
            );
        } else if let Some(value) = line.strip_prefix("E:") {
            let (key, value) = value
                .split_once('=')
                .ok_or_else(|| invalid("invalid udev property record"))?;
            if key.is_empty()
                || key.len() > 256
                || properties.len() >= 1024
                || properties
                    .insert(key.to_owned(), value.to_owned())
                    .is_some()
            {
                return Err(invalid("duplicate or excessive udev properties"));
            }
        }
    }
    if properties.get("ID_SEAT").is_some_and(|seat| {
        seat.is_empty() || seat.len() > 256 || seat.chars().any(char::is_control)
    }) {
        return Err(invalid("invalid udev seat identity"));
    }
    Ok((initialized.unwrap_or(false), properties))
}

pub(super) fn same_description(left: &Device, right: &Device) -> bool {
    let mut expected = left.clone();
    expected.source.generation = right.source.generation;
    expected == *right
}

#[cfg(test)]
mod tests;
