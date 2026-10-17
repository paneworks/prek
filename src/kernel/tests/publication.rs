use super::*;
use crate::{
    linux::DevicePublication,
    protocol::Epoch,
    transport::{Consumer, ConsumerEvidence, Readiness},
};
use std::{os::unix::fs::PermissionsExt, time::Duration};

struct Database {
    _root: Temporary,
    node: File,
    directory: PathBuf,
}
impl Database {
    fn new() -> Self {
        let root = Temporary::new();
        let node = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(root.path.join("node"))
            .unwrap();
        let directory = root.path.join("database");
        fs::create_dir(&directory).unwrap();
        Self {
            _root: root,
            node,
            directory,
        }
    }
    fn path(&self) -> PathBuf {
        self.directory.join("c0:0")
    }
    fn read(&self, text: impl AsRef<[u8]>) -> io::Result<DevicePublication> {
        fs::write(self.path(), text)?;
        fs::set_permissions(self.path(), fs::Permissions::from_mode(0o600))?;
        DevicePublication::fixture(
            self.node.as_fd(),
            &self.directory,
            self.node.metadata()?.uid(),
        )
    }
    fn properties(&self, descriptor: &Descriptor, initialized: bool) -> DevicePublication {
        let mut text = if initialized {
            "I:1\n".to_owned()
        } else {
            String::new()
        };
        for (key, value) in &descriptor.udev {
            text.push_str(&format!("E:{key}={value}\n"));
        }
        self.read(text).unwrap()
    }
}

#[test]
fn recreation_has_fresh_phys_and_receipt_while_the_source_slot_remains_exclusive() {
    let temporary = Temporary::new();
    let registry = temporary.registry();
    let original = descriptor();
    let first = registry.prepare(original.clone()).unwrap();
    first.verify().unwrap();
    let old_marker = first.metadata.marker.clone();
    let old_descriptor = first.descriptor.clone();
    let claim = first.name.clone();
    assert_eq!(first.metadata.version, 2);
    assert_eq!(first.metadata.properties["PREK_PUBLICATION"], old_marker);
    assert_ne!(old_marker, original.marker);
    assert_eq!(
        filename(&old_marker).unwrap(),
        filename(&original.marker).unwrap()
    );
    assert!(registry.directory.read(&original.marker).is_err());
    assert_eq!(
        registry.prepare(original.clone()).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert!(registry.prepare(old_descriptor.clone()).is_err());
    drop(first);
    let next = registry.prepare(original).unwrap();
    assert_ne!(next.metadata.marker, old_marker);
    assert_eq!(next.name, claim);
    assert_eq!(
        next.metadata.properties["PREK_PUBLICATION"],
        next.metadata.marker
    );
    assert!(registry.directory.read(&old_marker).is_err());
    assert_eq!(
        registry.directory.read(&next.metadata.marker).unwrap(),
        next.metadata
    );
    let decoded: Descriptor =
        serde_json::from_slice(&serde_json::to_vec(&next.descriptor).unwrap()).unwrap();
    decoded.validate().unwrap();
    assert_eq!(decoded, next.descriptor);
    assert!(registry.prepare(decoded).is_err());
}

#[test]
fn stale_or_uncompleted_udev_receipts_never_acknowledge_a_recreated_virtual_device() {
    let temporary = Temporary::new();
    let registry = temporary.registry();
    let first = registry.prepare(descriptor()).unwrap();
    let old = first.descriptor.clone();
    drop(first);
    let next = registry.prepare(descriptor()).unwrap();
    let database = Database::new();
    assert!(!processed_publication(&database.properties(&old, true), &next.descriptor).unwrap());
    assert!(
        !processed_publication(
            &database.properties(&next.descriptor, false),
            &next.descriptor
        )
        .unwrap()
    );
    assert!(
        !processed_publication(
            &database.read("I:1\nE:PREK_VIRTUAL=1\n").unwrap(),
            &next.descriptor
        )
        .unwrap()
    );
    assert!(
        processed_publication(
            &database.properties(&next.descriptor, true),
            &next.descriptor
        )
        .unwrap()
    );
    assert!(
        processed_publication(&database.properties(&next.descriptor, true), &descriptor()).is_err()
    );
    let mut wrong = next.descriptor.clone();
    wrong.udev.insert("ID_SEAT".into(), "seat1".into());
    let changed = database.properties(&wrong, true);
    assert!(processed_publication(&changed, &next.descriptor).is_err());
    assert!(
        database
            .read(format!(
                "I:1\nE:PREK_PUBLICATION={}\nE:PREK_PUBLICATION={}\n",
                old.marker, next.descriptor.marker
            ))
            .is_err()
    );
    assert!(
        database
            .read("I:1\nE:ID_SEAT=seat1\nE:ID_SEAT=seat0\n")
            .is_err()
    );
    assert!(
        database
            .read(format!("I:1\nE:X={}\n", "x".repeat(65_536)))
            .is_err()
    );
    assert!(database.read([0xff, b'\n']).is_err());
}

#[test]
fn private_registry_record_replacement_cannot_survive_a_verified_publication_check() {
    let temporary = Temporary::new();
    let registry = temporary.registry();
    let publication = registry.prepare(descriptor()).unwrap();
    publication.verify().unwrap();
    let path = temporary.path.join(publication.name.to_str().unwrap());
    let replacement = temporary.path.join("replacement");
    fs::write(
        &replacement,
        serde_json::to_vec(&publication.metadata).unwrap(),
    )
    .unwrap();
    fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600)).unwrap();
    fs::rename(replacement, &path).unwrap();
    let inode = fs::metadata(&path).unwrap().ino();
    assert_ne!(inode, publication.inode);
    assert!(publication.verify().is_err());
    drop(publication);
    assert_eq!(fs::metadata(path).unwrap().ino(), inode);
}

#[test]
fn published_marker_shape_is_not_permission_to_choose_or_reuse_a_kernel_publication() {
    let base = descriptor();
    assert!(base.clone().bind_publication(Epoch([0, 0])).is_err());
    let bound = base.clone().bind_publication(Epoch([1, 2])).unwrap();
    bound.validate().unwrap();
    assert!(bound.clone().bind_publication(Epoch([3, 4])).is_err());
    for nonce in [
        "",
        "0",
        "00000000000000000000000000000000",
        "0000000000000000000000000000000A",
        "00000000000000000000000000000001/more",
    ] {
        let mut changed = base.clone();
        changed.marker = format!("{}/{nonce}", base.marker);
        changed
            .udev
            .insert("PREK_PUBLICATION".into(), changed.marker.clone());
        assert!(changed.validate().is_err(), "accepted nonce {nonce:?}");
        assert!(filename(&changed.marker).is_err());
    }
    let mut forged = base;
    forged
        .udev
        .insert("PREK_PUBLICATION".into(), bound.marker.clone());
    assert!(forged.validate().is_err());
    let mut mismatch = bound.clone();
    mismatch
        .udev
        .insert("PREK_PUBLICATION".into(), "different".into());
    assert!(mismatch.validate().is_err());
    let temporary = Temporary::new();
    assert!(temporary.registry().prepare(bound).is_err());
}

#[test]
fn previous_publication_consumer_evidence_cannot_complete_the_new_gate() {
    let temporary = Temporary::new();
    let registry = temporary.registry();
    let first = registry.prepare_isolated(descriptor()).unwrap();
    let old = first.descriptor.clone();
    drop(first);
    let next = registry.prepare_isolated(descriptor()).unwrap();
    let consumer = Consumer::IsolatedReceiver {
        pid: std::process::id(),
        start: 1,
    };
    let now = Duration::ZERO;
    let mut gate = Readiness::new(next.descriptor.clone(), consumer.clone(), now).unwrap();
    assert!(gate.metadata(&old, now).is_err());
    let mut gate = Readiness::new(next.descriptor.clone(), consumer.clone(), now).unwrap();
    gate.metadata(&next.descriptor, now).unwrap();
    assert!(
        gate.consumer(
            &ConsumerEvidence {
                consumer,
                marker: old.marker,
                name: old.name,
                class: old.class,
                seat: old.source.seat,
                settings: old.consumer_settings,
                at: now,
            },
            now
        )
        .is_err()
    );
    assert!(gate.finish(now).is_err());
}

#[test]
fn legacy_registry_versions_are_preserved_instead_of_being_imported_or_recovered() {
    let temporary = Temporary::new();
    let registry = temporary.registry();
    let publication = registry.prepare(descriptor()).unwrap();
    let marker = publication.metadata.marker.clone();
    let path = temporary.path.join(publication.name.to_str().unwrap());
    let mut legacy = publication.metadata.clone();
    legacy.version = 1;
    drop(publication);
    fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(registry.directory.read(&marker).is_err());
    assert!(registry.recover(None, false).is_err());
    assert!(path.exists());
}
