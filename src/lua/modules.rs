//! Approved module sources: capture once outside Luna, then load from memory.
use super::{ConfigError, MAX_CONFIG_BYTES, error};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_MODULES: usize = 64;
pub const MAX_IMPORT_DEPTH: usize = 16;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Module {
    #[serde(with = "super::runtime::source_bytes")]
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Sources {
    pub name: String,
    #[serde(with = "super::runtime::source_bytes")]
    pub source: String,
    #[serde(default)]
    pub modules: BTreeMap<String, Module>,
}
impl Sources {
    pub fn single(source: String, name: String) -> Self {
        Self {
            name,
            source,
            modules: BTreeMap::new(),
        }
    }
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate(&self.name, &self.source, &self.modules)
    }
}
pub(super) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name != "prek"
        && !name.starts_with("prek.")
        && name.split('.').count() <= MAX_IMPORT_DEPTH
        && name.split('.').all(valid_segment)
}
fn valid_segment(part: &str) -> bool {
    let mut bytes = part.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}
pub(super) fn validate(
    name: &str,
    source: &str,
    modules: &BTreeMap<String, Module>,
) -> Result<(), ConfigError> {
    if name.is_empty()
        || name.len() > 4096
        || name.contains('\0')
        || modules.len() > MAX_MODULES
        || modules.keys().any(|name| !valid_name(name))
        || modules
            .values()
            .try_fold(source.len(), |total, module| {
                total.checked_add(module.source.len())
            })
            .is_none_or(|total| total > MAX_CONFIG_BYTES)
    {
        return Err(error(
            "invalid module name or approved Lua source set exceeds its bounds",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
impl Sources {
    /// Only `modules/<identifier>/.../<identifier>.lua` beside the entry file.
    /// Directories and modules must belong to the entry owner and exclude
    /// group/other writes. Every child is opened relative to its directory fd.
    pub fn capture(source: String, path: &std::path::Path) -> std::io::Result<Self> {
        use nix::fcntl::{OFlag, openat};
        use nix::sys::stat::Mode;
        use std::{
            fs::File,
            os::unix::fs::{MetadataExt, OpenOptionsExt},
        };
        let mut sources = Self::single(source, path.to_string_lossy().into_owned());
        sources.validate().map_err(std::io::Error::other)?;
        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open(parent)?;
        let uid = match std::fs::metadata(path) {
            Ok(metadata) => metadata.uid(),
            // SAFETY: geteuid has no arguments or pointer access.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => unsafe {
                nix::libc::geteuid()
            },
            Err(error) => return Err(error),
        };
        let modules = match openat(
            &directory,
            "modules",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => File::from(fd),
            Err(nix::errno::Errno::ENOENT) => return Ok(sources),
            Err(error) => return Err(error.into()),
        };
        let mut capture = Capture {
            uid,
            bytes: sources.source.len(),
            entries: 0,
            retained: Vec::new(),
            modules: &mut sources.modules,
        };
        capture.directory(modules, "", 0)?;
        for (file, before) in &capture.retained {
            if stamp(&file.metadata()?) != *before {
                return Err(std::io::Error::other(
                    "Lua modules changed during snapshot capture",
                ));
            }
        }
        sources.validate().map_err(std::io::Error::other)?;
        Ok(sources)
    }
}

#[cfg(target_os = "linux")]
type Stamp = (u64, u64, u64, i64, i64, i64, i64);
#[cfg(target_os = "linux")]
fn stamp(metadata: &std::fs::Metadata) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}
#[cfg(target_os = "linux")]
struct Capture<'a> {
    uid: u32,
    bytes: usize,
    entries: usize,
    retained: Vec<(std::fs::File, Stamp)>,
    modules: &'a mut BTreeMap<String, Module>,
}
#[cfg(target_os = "linux")]
impl Capture<'_> {
    fn directory(
        &mut self,
        directory: std::fs::File,
        prefix: &str,
        depth: usize,
    ) -> std::io::Result<()> {
        use nix::{
            fcntl::{OFlag, openat},
            sys::stat::Mode,
        };
        use std::{
            fs::File,
            io::{self, Read},
            os::fd::AsRawFd,
        };
        if depth >= MAX_IMPORT_DEPTH || self.retained.len() > 2 * MAX_MODULES {
            return Err(io::Error::other(
                "Lua module directory depth/count exceeded",
            ));
        }
        let metadata = directory.metadata()?;
        self.protected(&metadata)?;
        let before = stamp(&metadata);
        let mut names = Vec::new();
        for entry in std::fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))? {
            self.entries += 1;
            if self.entries > 4 * MAX_MODULES {
                return Err(io::Error::other(
                    "Lua module directory entry limit exceeded",
                ));
            }
            let entry = entry?;
            let kind = entry.file_type()?;
            if !kind.is_dir() && !kind.is_file() {
                return Err(io::Error::other(
                    "Lua module trees cannot contain symlinks or special files",
                ));
            }
            names.push((entry.file_name(), kind));
        }
        names.sort_by(|left, right| left.0.cmp(&right.0));
        for (name, kind) in names {
            let label = name
                .to_str()
                .ok_or_else(|| io::Error::other("Lua module names must be ASCII identifiers"))?;
            if kind.is_file() && !label.ends_with(".lua") {
                continue;
            }
            let file = File::from(
                openat(
                    &directory,
                    name.as_os_str(),
                    OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .map_err(io::Error::from)?,
            );
            let metadata = file.metadata()?;
            if metadata.is_dir() != kind.is_dir() || metadata.is_file() != kind.is_file() {
                return Err(io::Error::other("Lua module entry changed during capture"));
            }
            if metadata.is_dir() {
                let next = if prefix.is_empty() {
                    label.into()
                } else {
                    format!("{prefix}.{label}")
                };
                if !valid_segment(label) || !valid_name(&next) {
                    return Err(io::Error::other("invalid Lua module directory name"));
                }
                self.directory(file, &next, depth + 1)?;
            } else if let Some(stem) = label.strip_suffix(".lua") {
                self.protected(&metadata)?;
                if !metadata.is_file() || self.modules.len() >= MAX_MODULES {
                    return Err(io::Error::other(
                        "Lua modules require bounded regular files",
                    ));
                }
                let name = if prefix.is_empty() {
                    stem.into()
                } else {
                    format!("{prefix}.{stem}")
                };
                if !valid_segment(stem) || !valid_name(&name) {
                    return Err(io::Error::other("invalid Lua module name"));
                }
                let mut source = String::new();
                (&file)
                    .take((MAX_CONFIG_BYTES - self.bytes + 1) as u64)
                    .read_to_string(&mut source)?;
                self.bytes += source.len();
                if self.bytes > MAX_CONFIG_BYTES {
                    return Err(io::Error::other("approved Lua sources exceed 1 MiB"));
                }
                self.modules.insert(name, Module { source });
                self.retained.push((file, stamp(&metadata)));
            }
        }
        self.retained.push((directory, before));
        Ok(())
    }
    fn protected(&self, metadata: &std::fs::Metadata) -> std::io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != self.uid || metadata.mode() & 0o022 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Lua modules require the entry file owner and protected permissions",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::{Definition, EventKind};
    fn sources(entry: &str, modules: &[(&str, &str)]) -> Sources {
        let mut sources = Sources::single(entry.into(), "approved.lua".into());
        sources.modules = modules
            .iter()
            .map(|(name, source)| {
                (
                    (*name).into(),
                    Module {
                        source: (*source).into(),
                    },
                )
            })
            .collect();
        sources
    }
    #[test]
    fn modules_preserve_cached_values_closures_and_snapshot_identity() {
        let sources = sources(
            "local p=require('prek'); local f=require('lib.counter'); assert(f==require('lib.counter')); p.events.on('ready',f); return require('settings')",
            &[
                (
                    "lib.counter",
                    "local p=require('prek'); local count=0; return function()count=count+1; p.log.info('module',{count=count})end",
                ),
                (
                    "settings",
                    "return require('prek').config.preset('minimal')",
                ),
            ],
        );
        let definition = Definition::compile_sources(&sources).unwrap();
        let encoded = serde_json::to_vec(&definition).unwrap();
        let mut decoded: Definition = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.modules, sources.modules);
        let mut runtime = decoded.instantiate().unwrap();
        for count in 1..=2 {
            let logs = runtime
                .dispatch(EventKind::Ready, &serde_json::json!({}))
                .unwrap();
            assert_eq!(logs[0].fields, serde_json::json!({"count":count}));
        }
        decoded.modules.get_mut("settings").unwrap().source =
            "return require('prek').config.preset('desktop')".into();
        assert!(
            decoded.instantiate().is_err(),
            "changed approved model must be refused"
        );
    }
    #[test]
    fn imports_reject_cycles_missing_modules_invalid_names_and_depth() {
        let cycle = sources(
            "return require('a')",
            &[("a", "return require('b')"), ("b", "return require('a')")],
        );
        let error = Definition::compile_sources(&cycle).unwrap_err().to_string();
        assert!(
            error.contains("cycle") && error.contains("a -> b -> a"),
            "{error}"
        );
        let mut replaced_globals = cycle.clone();
        replaced_globals.source = "assert=function()end; pcall=function()return true end; type=function()return 'string' end; return require('a')".into();
        assert!(
            Definition::compile_sources(&replaced_globals)
                .unwrap_err()
                .to_string()
                .contains("module cycle")
        );
        for name in ["io", "../escape", "lib/no", "missing"] {
            let error =
                Definition::compile_sources(&sources(&format!("return require('{name}')"), &[]))
                    .unwrap_err()
                    .to_string();
            assert!(error.contains("approved source snapshot"), "{error}");
        }
        for name in ["", "prek", "prek.extra", "a..b", "../file", "a/b", "0name"] {
            assert!(
                sources("return {version=1}", &[(name, "return true")])
                    .validate()
                    .is_err()
            );
        }
        let mut deep = Sources::single("return require('m1')".into(), "depth.lua".into());
        for index in 1..=MAX_IMPORT_DEPTH + 1 {
            let source = if index == MAX_IMPORT_DEPTH + 1 {
                "return {version=1}".into()
            } else {
                format!("return require('m{}')", index + 1)
            };
            deep.modules.insert(format!("m{index}"), Module { source });
        }
        assert!(
            Definition::compile_sources(&deep)
                .unwrap_err()
                .to_string()
                .contains("import depth")
        );
    }
    #[test]
    fn nil_and_false_modules_are_cached_and_import_failure_can_be_caught() {
        let sources = sources(
            "assert(require('empty')==true); assert(require('empty')==true); assert(require('false_value')==false); assert(require('false_value')==false); assert(calls==2); local ok=pcall(require,'bad'); assert(not ok); assert(require('settings').version==1); return require('settings')",
            &[
                ("empty", "calls=(calls or 0)+1"),
                ("false_value", "calls=(calls or 0)+1; return false"),
                ("bad", "error('module failure')"),
                ("settings", "return {version=1}"),
            ],
        );
        Definition::compile_sources(&sources).unwrap();
    }
    #[test]
    fn all_module_syntax_and_combined_source_bounds_are_checked() {
        assert!(
            Definition::compile_sources(&sources(
                "return {version=1}",
                &[("unused", "local = bad")]
            ))
            .is_err()
        );
        let mut bounded = sources("return {version=1}", &[]);
        bounded.modules.insert(
            "large".into(),
            Module {
                source: "x".repeat(MAX_CONFIG_BYTES),
            },
        );
        assert!(bounded.validate().is_err());
        bounded.modules.clear();
        for index in 0..=MAX_MODULES {
            bounded.modules.insert(
                format!("m{index}"),
                Module {
                    source: "return true".into(),
                },
            );
        }
        assert!(bounded.validate().is_err());
    }

    #[test]
    fn lazy_module_imports_obey_callback_limits_and_retire_failed_runtimes() {
        for body in ["return require('late')", "while true do end"] {
            let sources = sources(
                "local p=require('prek'); p.events.on('ready',function() require('late') end); return {version=1}",
                &[("late", body)],
            );
            let mut runtime = Definition::compile_sources(&sources)
                .unwrap()
                .instantiate()
                .unwrap();
            assert!(
                runtime
                    .dispatch(EventKind::Ready, &serde_json::json!({}))
                    .is_err()
            );
            assert!(
                runtime
                    .dispatch(EventKind::Ready, &serde_json::json!({}))
                    .unwrap_err()
                    .to_string()
                    .contains("retired")
            );
        }
    }
}
