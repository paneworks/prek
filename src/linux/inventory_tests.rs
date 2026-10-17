use super::*;
use std::{
    os::unix::{
        ffi::OsStringExt,
        fs::{PermissionsExt, symlink},
    },
    sync::atomic::{AtomicU64, Ordering},
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "prek-inventory-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    fn failures(&self) {
        fs::write(self.0.join("event2"), b"unreadable owned file").unwrap();
        fs::set_permissions(self.0.join("event2"), fs::Permissions::from_mode(0o0)).unwrap();
        fs::write(self.0.join("event10"), b"not an input device").unwrap();
        symlink(self.0.join("gone"), self.0.join("event1")).unwrap();
    }
    fn entries(&self) -> Vec<Entry> {
        discover_in(&self.0)
            .unwrap()
            .into_iter()
            .map(|value| value.entry)
            .collect()
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn unreadable_invalid_and_vanished_entries_remain_explicit_and_sorted() {
    let directory = Directory::new();
    directory.failures();
    let entries = directory.entries();
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.path.file_name().unwrap().to_str().unwrap())
            .collect::<Vec<_>>(),
        ["event1", "event10", "event2"]
    );
    assert!(entries.iter().all(|entry| !entry.available
        && entry.device.is_none()
        && entry.error.as_ref().is_some_and(|error| !error.is_empty())));
    if let Err(error) = fs::File::open(directory.0.join("event2")) {
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            entries[2].error.as_deref(),
            Some(error.to_string().as_str())
        );
    }
}

#[test]
fn only_decimal_event_names_are_candidates_and_missing_directories_are_empty() {
    let directory = Directory::new();
    for name in ["event", "event-1", "event2extra", "mouse0", "joystick1"] {
        fs::write(directory.0.join(name), b"ignored").unwrap();
    }
    fs::write(
        directory
            .0
            .join(std::ffi::OsString::from_vec(b"event\xff".to_vec())),
        b"ignored",
    )
    .unwrap();
    assert!(directory.entries().is_empty());
    assert!(discover_in(&directory.0.join("absent")).unwrap().is_empty());
}

fn mixed(directory: &Directory) -> Vec<Entry> {
    let mut device = crate::transport::tests::touch();
    let path = directory.0.join("event0");
    device.path = path.to_str().unwrap().to_owned();
    vec![
        Entry {
            path,
            available: true,
            device: Some(device),
            error: None,
        },
        directory.entries().remove(0),
    ]
}

#[test]
fn json_and_text_inventory_keep_failed_entries_without_hiding_readable_models() {
    let directory = Directory::new();
    directory.failures();
    let mut output = Vec::new();
    write_inventory(&mut output, true, mixed(&directory)).unwrap();
    let parsed: Vec<Entry> = serde_json::from_slice(&output).unwrap();
    assert_eq!(output.last(), Some(&b'\n'));
    assert_eq!(parsed.len(), 2);
    assert!(parsed[0].available && parsed[0].device.is_some() && parsed[0].error.is_none());
    assert!(!parsed[1].available && parsed[1].device.is_none() && parsed[1].error.is_some());
    output.clear();
    write_inventory(&mut output, false, mixed(&directory)).unwrap();
    let text = String::from_utf8(output).unwrap();
    assert_eq!(text.lines().count(), 2);
    assert!(text.contains("event0") && text.contains("event1  unavailable:"));
    let mut empty = Vec::new();
    write_inventory(&mut empty, true, Vec::new()).unwrap();
    assert_eq!(empty, b"[]\n");
    empty.clear();
    write_inventory(&mut empty, false, Vec::new()).unwrap();
    assert_eq!(empty, b"No event devices found in /dev/input.\n");
}

struct Fails {
    remaining: usize,
    kind: io::ErrorKind,
}
impl Write for Fails {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Err(io::Error::new(self.kind, "owned output failure"));
        }
        let length = self.remaining.min(bytes.len());
        self.remaining -= length;
        Ok(length)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn structured_output_preserves_io_kinds_after_partial_json_or_text_writes() {
    let directory = Directory::new();
    directory.failures();
    for json in [false, true] {
        for remaining in [0, 1, 8] {
            for kind in [
                io::ErrorKind::BrokenPipe,
                io::ErrorKind::TimedOut,
                io::ErrorKind::WouldBlock,
                io::ErrorKind::WriteZero,
                io::ErrorKind::PermissionDenied,
            ] {
                let mut output = Fails { remaining, kind };
                assert_eq!(
                    write_inventory(&mut output, json, mixed(&directory))
                        .unwrap_err()
                        .kind(),
                    kind
                );
                let mut output = Fails { remaining, kind };
                let observation = Observation::Button {
                    at: Duration::ZERO,
                    gesture: crate::Gesture::Taps {
                        button: crate::Button {
                            device: 1,
                            code: 115,
                        },
                        count: 2,
                    },
                };
                assert_eq!(
                    print_observation(
                        &mut output,
                        &observation,
                        &ObserveOptions {
                            gestures_only: false,
                            json,
                            duration: None
                        }
                    )
                    .unwrap_err()
                    .kind(),
                    kind
                );
                let mut output = Fails { remaining, kind };
                assert_eq!(
                    crate::replay::write_record(&mut output, &Record::Header { version: 1 })
                        .unwrap_err()
                        .kind(),
                    kind
                );
            }
        }
    }
}

#[test]
fn repeated_failed_scans_do_not_retain_descriptors_to_private_files() {
    let directory = Directory::new();
    directory.failures();
    let root = fs::canonicalize(&directory.0).unwrap();
    for _ in 0..32 {
        assert_eq!(directory.entries().len(), 3);
        assert!(
            fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(|entry| fs::read_link(entry.unwrap().path()).ok())
                .all(|target| !target.starts_with(&root))
        );
    }
}
