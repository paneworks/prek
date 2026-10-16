#![cfg(target_os = "linux")]
#[path = "support/recovery_control.rs"]
mod control;
use control::{Directory, Identity, Process};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

struct Temporary(PathBuf);
impl Temporary {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "prek-recovery-control-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn directory(&self) -> Directory {
        Directory::from_file(
            File::open(&self.0).unwrap(),
            self.0.clone(),
            fs::metadata(&self.0).unwrap().uid(),
        )
        .unwrap()
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn control_round_trip_tracks_observed_child_files_and_cleans_only_its_case() {
    let temporary = Temporary::new();
    let base = temporary.directory();
    let child = base.child("case-0").unwrap();
    let identity = child.identity().unwrap();
    child
        .write("setup", &serde_json::json!({"owned":true}))
        .unwrap();
    let writer = Directory::from_file(
        File::open(child.path()).unwrap(),
        child.path().to_owned(),
        fs::metadata(child.path()).unwrap().uid(),
    )
    .unwrap();
    writer.write("prepared", &17).unwrap();
    assert_eq!(child.read::<u64>("prepared").unwrap(), Some(17));
    assert_eq!(child.read::<bool>("absent").unwrap(), None);
    child.cleanup().unwrap();
    base.remove_child("case-0", identity).unwrap();
    assert_eq!(fs::read_dir(&temporary.0).unwrap().count(), 0);
}

#[test]
fn control_names_cannot_escape_the_retained_directory() {
    let temporary = Temporary::new();
    let directory = temporary.directory();
    for name in ["", ".", "..", "../outside", "/etc/passwd", "a/b", "null\0"] {
        assert!(directory.write(name, &true).is_err());
        assert!(directory.read::<bool>(name).is_err());
        assert!(directory.child(name).is_err());
    }
    assert!(directory.write(&"a".repeat(129), &true).is_err());
    assert_eq!(fs::read_dir(&temporary.0).unwrap().count(), 0);
}

#[test]
fn control_rejects_symlinks_hardlinks_and_shared_permissions() {
    let temporary = Temporary::new();
    let directory = temporary.directory();
    let outside = temporary.0.join("outside");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&outside)
        .unwrap();
    fs::write(&outside, "true").unwrap();
    let link = temporary.0.join("setup");
    symlink(&outside, &link).unwrap();
    assert!(directory.read::<bool>("setup").is_err());
    fs::remove_file(&link).unwrap();
    fs::hard_link(&outside, &link).unwrap();
    assert!(directory.read::<bool>("setup").is_err());
    fs::remove_file(&link).unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(directory.read::<bool>("outside").is_err());
    assert_eq!(fs::read_to_string(&outside).unwrap(), "true");
}

#[test]
fn control_cleanup_preserves_unknown_or_replaced_files_before_any_unlink() {
    let temporary = Temporary::new();
    let directory = temporary.directory();
    directory.write("setup", &true).unwrap();
    directory.write("prepared", &true).unwrap();
    let setup = temporary.0.join("setup");
    fs::rename(&setup, temporary.0.join("retained-original")).unwrap();
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&setup)
        .unwrap();
    fs::write(&setup, "false").unwrap();
    assert!(directory.read::<bool>("setup").is_err());
    assert!(directory.cleanup().is_err());
    assert!(temporary.0.join("prepared").exists());
    assert_eq!(fs::read_to_string(setup).unwrap(), "false");
}

#[test]
fn control_directory_replacement_does_not_redirect_cleanup_or_remove_foreign_files() {
    let temporary = Temporary::new();
    let base = temporary.directory();
    let child = base.child("case-0").unwrap();
    let identity = child.identity().unwrap();
    child.write("setup", &true).unwrap();
    fs::rename(child.path(), temporary.0.join("old-case")).unwrap();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(child.path())
        .unwrap();
    fs::write(child.path().join("foreign"), "retained").unwrap();
    child.cleanup().unwrap();
    assert!(base.remove_child("case-0", identity).is_err());
    assert_eq!(
        fs::read_to_string(child.path().join("foreign")).unwrap(),
        "retained"
    );
    assert_eq!(
        fs::read_dir(temporary.0.join("old-case")).unwrap().count(),
        0
    );
}

#[test]
fn control_bounds_partial_records_bytes_and_file_counts() {
    let temporary = Temporary::new();
    let directory = temporary.directory();
    let path = temporary.0.join("setup");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    fs::write(&path, "{\"ready\":").unwrap();
    assert!(
        directory
            .read::<serde_json::Value>("setup")
            .unwrap()
            .is_none()
    );
    fs::write(&path, "{\"ready\":true}").unwrap();
    assert_eq!(
        directory
            .read::<serde_json::Value>("setup")
            .unwrap()
            .unwrap()["ready"],
        true
    );
    assert!(directory.write("oversized", &"x".repeat(65536)).is_err());
    for index in 0..7 {
        directory.write(&format!("file-{index}"), &true).unwrap();
    }
    assert!(directory.write("ninth", &true).is_err());
    fs::write(&path, "x".repeat(65537)).unwrap();
    assert!(directory.read::<serde_json::Value>("setup").is_err());
}

#[test]
fn control_rejects_wrong_ownership_nonprivate_directories_and_noncanonical_identities() {
    let temporary = Temporary::new();
    let owner = fs::metadata(&temporary.0).unwrap().uid();
    assert!(
        Directory::from_file(
            File::open(&temporary.0).unwrap(),
            temporary.0.clone(),
            owner + 1
        )
        .is_err()
    );
    fs::set_permissions(&temporary.0, fs::Permissions::from_mode(0o750)).unwrap();
    assert!(
        Directory::from_file(
            File::open(&temporary.0).unwrap(),
            temporary.0.clone(),
            owner
        )
        .is_err()
    );
    for text in ["", "1", "0:0", "01:2", "1:02", "1:2:3", "-1:2"] {
        assert!(Identity::parse(text).is_err());
    }
    assert_eq!(Identity::parse("1:2").unwrap().encode(), "1:2");
}

#[test]
fn pidfd_signals_are_bound_to_the_owned_process_and_check_birth() {
    let mut child = Command::new("sleep").arg("10").spawn().unwrap();
    let stat = fs::read_to_string(format!("/proc/{}/stat", child.id())).unwrap();
    let birth = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!(Process::pin(child.id(), Some(birth + 1)).is_err());
    let process = Process::pin(child.id(), Some(birth)).unwrap();
    process.signal(nix::libc::SIGTERM).unwrap();
    assert!(!child.wait().unwrap().success());
    assert!(process.signal(nix::libc::SIGTERM).is_err());
    assert!(Process::pin(0, None).is_err());
}
