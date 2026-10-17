use super::*;
use std::{
    os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
    sync::atomic::{AtomicU64, Ordering},
};

struct Fixture {
    root: PathBuf,
    node: File,
    path: PathBuf,
    data: PathBuf,
    owner: u32,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "prek-discovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let path = root.join("node");
        let node = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let data = root.join("database");
        fs::create_dir(&data).unwrap();
        let owner = node.metadata().unwrap().uid();
        Self {
            root,
            node,
            path,
            data,
            owner,
        }
    }
    fn database(&self) -> PathBuf {
        Node::read(self.node.as_fd()).unwrap().database(&self.data)
    }
    fn publish(&self, text: impl AsRef<[u8]>) {
        let staging = self.root.join("next");
        fs::write(&staging, text).unwrap();
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o600)).unwrap();
        fs::rename(staging, self.database()).unwrap();
    }
    fn read(&self) -> io::Result<Publication> {
        Publication::read_at(self.node.as_fd(), &self.data, self.owner)
    }
    fn current(&self, value: &Publication) -> bool {
        value
            .current_at(self.node.as_fd(), Some(&self.path), &self.data, self.owner)
            .unwrap_or(false)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn device() -> Device {
    Device {
        identity: "test keyboard".into(),
        source: Source {
            device: 1,
            generation: 1,
            seat: "seat0".into(),
        },
        name: "keyboard".into(),
        path: "private-node".into(),
        physical: "private/1".into(),
        unique: "id".into(),
        vendor: 1,
        product: 2,
        properties: vec![1],
        capabilities: BTreeMap::from([(input::EV_KEY, vec![30])]),
        axes: BTreeMap::from([(
            input::ABS_MT_POSITION_X,
            Axis {
                minimum: 0,
                maximum: 100,
                resolution: 1,
                fuzz: 0,
                flat: 0,
            },
        )]),
        virtual_device: false,
    }
}

#[test]
fn database_publication_parses_initialized_properties_without_inventing_completion() {
    let (ready, properties) =
        parse("I:123\nE:ID_SEAT=seat1\nE:ID_SERIAL=a=b\nG:seat\nV:1\n").unwrap();
    assert!(ready);
    assert_eq!(properties["ID_SEAT"], "seat1");
    assert_eq!(properties["ID_SERIAL"], "a=b");
    assert!(!parse("E:ID_SEAT=seat0\n").unwrap().0);
    assert!(!parse("I:0\n").unwrap().0);
    for text in [
        "I:1",
        "I:+1\n",
        "I:1\nI:2\n",
        "E:X=1\nE:X=2\n",
        "E:ID_SEAT=\n",
        "E:ID_SEAT=a\tb\n",
        "E:bad\n",
        "I:18446744073709551616\n",
        "E:X=a\0b\n",
    ] {
        assert!(parse(text).is_err(), "accepted {text:?}");
    }
    assert!(parse(&format!("E:X={}\n", "x".repeat(MAX_DATABASE as usize + 1))).is_err());
}

#[test]
fn managed_admission_rejects_every_changed_description_but_not_its_fresh_generation() {
    let fixture = Fixture::new();
    fixture.publish("I:1\nE:ID_SEAT=seat0\n");
    let publication = fixture.read().unwrap();
    let expected = device();
    let mut opened = expected.clone();
    opened.source.generation = 2;
    assert!(publication.admits(&publication, &opened, &expected));
    for field in 0..14 {
        let mut changed = opened.clone();
        match field {
            0 => changed.identity.push('x'),
            1 => changed.source.device += 1,
            2 => changed.source.seat = "seat1".into(),
            3 => changed.name.push('x'),
            4 => changed.path.push('x'),
            5 => changed.physical.push('x'),
            6 => changed.unique.push('x'),
            7 => changed.vendor += 1,
            8 => changed.product += 1,
            9 => changed.properties.push(2),
            10 => changed
                .capabilities
                .get_mut(&input::EV_KEY)
                .unwrap()
                .push(31),
            11 => {
                changed
                    .axes
                    .get_mut(&input::ABS_MT_POSITION_X)
                    .unwrap()
                    .maximum += 1
            }
            12 => {
                changed
                    .axes
                    .get_mut(&input::ABS_MT_POSITION_X)
                    .unwrap()
                    .fuzz += 1
            }
            _ => changed.virtual_device = true,
        }
        assert!(
            !publication.admits(&publication, &changed, &expected),
            "admitted changed field {field}"
        );
    }
    fixture.publish("E:ID_SEAT=seat0\n");
    let incomplete = fixture.read().unwrap();
    assert!(!incomplete.admits(&incomplete, &opened, &expected));
}

#[test]
fn actual_atomic_publication_replacement_retires_even_identical_properties_and_deletion() {
    let fixture = Fixture::new();
    let missing = fixture.read().unwrap();
    assert!(!missing.initialized);
    assert!(missing.properties.is_empty());
    assert!(fixture.current(&missing));
    fixture.publish("I:1\nE:ID_SEAT=seat0\n");
    assert!(!fixture.current(&missing));
    let first = fixture.read().unwrap();
    assert!(fixture.current(&first));
    fixture.publish("I:1\nE:ID_SEAT=seat0\n");
    let same_properties = fixture.read().unwrap();
    assert_eq!(first.properties, same_properties.properties);
    assert!(!fixture.current(&first));
    assert!(!same_properties.admits(&first, &device(), &device()));
    fixture.publish("I:1\nE:ID_SEAT=seat1\n");
    let changed = fixture.read().unwrap();
    assert_eq!(changed.properties["ID_SEAT"], "seat1");
    assert!(!fixture.current(&same_properties));
    assert!(!changed.admits(&same_properties, &device(), &device()));
    fs::remove_file(fixture.database()).unwrap();
    assert!(!fixture.current(&changed));
    assert!(!fixture.read().unwrap().initialized);
}

#[test]
fn actual_node_replacement_is_not_the_owned_descriptor_or_a_matching_publication() {
    let fixture = Fixture::new();
    fixture.publish("I:1\nE:ID_SEAT=seat0\n");
    let first = fixture.read().unwrap();
    let replacement = fixture.root.join("replacement");
    fs::write(&replacement, b"another connection").unwrap();
    let next = File::open(&replacement).unwrap();
    fs::rename(replacement, &fixture.path).unwrap();
    assert!(!fixture.current(&first));
    let next_publication =
        Publication::read_at(next.as_fd(), &fixture.data, fixture.owner).unwrap();
    assert!(!next_publication.admits(&first, &device(), &device()));
    assert!(
        next_publication
            .current_at(
                next.as_fd(),
                Some(&fixture.path),
                &fixture.data,
                fixture.owner
            )
            .unwrap()
    );
    assert_ne!(
        Node::read(fixture.node.as_fd()).unwrap(),
        Node::read(next.as_fd()).unwrap()
    );
    let mut number = Node::read(fixture.node.as_fd()).unwrap();
    number.number = libc::makedev(13, 67);
    assert_eq!(
        number.database(Path::new("/private")),
        PathBuf::from("/private/c13:67")
    );
}

#[test]
fn private_database_rejects_wrong_owners_modes_links_nonfiles_and_unbounded_or_invalid_text() {
    let fixture = Fixture::new();
    fixture.publish("I:1\n");
    assert!(Publication::read_at(fixture.node.as_fd(), &fixture.data, fixture.owner ^ 1).is_err());
    if fixture.owner != 0 {
        assert!(Stamp::protected(&fs::metadata(fixture.database()).unwrap(), 0).is_err());
    }
    for mode in [0o620, 0o602, 0o666] {
        fs::set_permissions(fixture.database(), fs::Permissions::from_mode(mode)).unwrap();
        assert!(fixture.read().is_err());
    }
    fixture.publish([0xff, b'\n']);
    assert!(fixture.read().is_err());
    fixture.publish(vec![b'x'; MAX_DATABASE as usize + 1]);
    assert!(fixture.read().is_err());
    fs::remove_file(fixture.database()).unwrap();
    symlink(&fixture.path, fixture.database()).unwrap();
    assert!(fixture.read().is_err());
    fs::remove_file(fixture.database()).unwrap();
    fs::create_dir(fixture.database()).unwrap();
    assert!(fixture.read().is_err());
    fs::remove_dir(fixture.database()).unwrap();
    nix::unistd::mkfifo(
        &fixture.database(),
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
    assert!(fixture.read().is_err());
}

#[test]
fn publication_retirement_cancels_old_holds_and_pending_frames_before_fresh_held_state() {
    use crate::{
        config::{Config, Key, Millis, Selector, Trigger},
        linux::ingress,
        rules::{Engine, Event},
    };
    let ms = Duration::from_millis;
    let fixture = Fixture::new();
    fixture.publish("I:1\nE:ID_SEAT=seat0\n");
    let publication = fixture.read().unwrap();
    let mut config = Config::preset("desktop").unwrap();
    config.policy.allowed_seats.push("seat1".into());
    config.devices.insert(
        "keyboard".into(),
        Selector {
            keys: vec![Key(30)],
            ..Selector::default()
        },
    );
    config.rules[0].enabled = true;
    config.rules[0].trigger = Trigger::ButtonHold {
        key: Key(30),
        after: Millis(600),
        repeat: None,
    };
    struct Sink(Engine);
    impl InputSink for Sink {
        fn push(&mut self, record: Record) -> io::Result<()> {
            self.0.push(record)
        }
        fn deadline(&self) -> Option<Duration> {
            self.0.deadline()
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut sink = Sink(Engine::new(config).unwrap());
    sink.push(Record::Header { version: 1 }).unwrap();
    let mut original = device();
    original.axes.clear();
    let source = original.source.clone();
    let mut other = original.clone();
    other.source.device = 2;
    other.identity = "other keyboard".into();
    let register = |device, keys| Record::Device {
        device,
        keys,
        contacts: vec![],
    };
    sink.push(register(original.clone(), BTreeSet::new()))
        .unwrap();
    sink.push(register(other.clone(), BTreeSet::new())).unwrap();
    let mut cursors = BTreeMap::from([
        (source.clone(), Cursor::new(source.clone())),
        (other.source.clone(), Cursor::new(other.source.clone())),
    ]);
    let events = |at: u64, down: bool| {
        [
            (input::EV_KEY, 30, i32::from(down)),
            (input::EV_SYN, input::SYN_REPORT, 0),
        ]
        .into_iter()
        .map(|(kind, code, value)| {
            InputEvent::from(libc::input_event {
                time: libc::timeval {
                    tv_sec: (at / 1000) as _,
                    tv_usec: ((at % 1000) * 1000) as _,
                },
                type_: kind,
                code,
                value,
            })
        })
        .collect()
    };
    let mut pending = ingress::Pending::default();
    for (source, at) in [(&source, 10), (&other.source, 20)] {
        pending
            .extend(
                cursors
                    .get_mut(source)
                    .unwrap()
                    .ingest(events(at, true))
                    .unwrap(),
            )
            .unwrap();
    }
    let mut input_at = Duration::ZERO;
    ingress::dispatch(&mut pending, &mut cursors, &mut sink, ms(20), &mut input_at).unwrap();
    let old_tail = cursors
        .get_mut(&source)
        .unwrap()
        .ingest(events(5000, false))
        .unwrap();
    pending.extend(old_tail.clone()).unwrap();
    fixture.publish("I:1\nE:ID_SEAT=seat1\n");
    assert!(!fixture.current(&publication));
    ingress::retire_source(&mut pending, &mut cursors, &mut sink, &source, ms(100)).unwrap();
    cursors.remove(&source);
    assert!(pending.is_empty());
    assert!(cursors.contains_key(&other.source));
    assert!(
        sink.0
            .push(register(original.clone(), BTreeSet::new()))
            .is_err()
    );
    let next_publication = fixture.read().unwrap();
    let mut scanned = original;
    scanned.source.seat = next_publication.properties["ID_SEAT"].clone();
    let mut fresh = scanned.clone();
    fresh.source.generation = 2;
    assert!(next_publication.admits(&next_publication, &fresh, &scanned));
    sink.push(register(fresh.clone(), BTreeSet::from([30])))
        .unwrap();
    assert!(
        sink.0
            .push(Record::Frame {
                frame: old_tail[0].clone()
            })
            .is_err()
    );
    sink.push(Record::Tick { at: ms(1000) }).unwrap();
    let matches = |events: Vec<Event>| {
        events
            .into_iter()
            .filter_map(|event| match event {
                Event::Matched { source, .. } => Some(source),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(matches(sink.0.take_events()), vec![other.source]);
    let mut cursor = Cursor::new(fresh.source.clone());
    for (at, down) in [(1100, false), (1200, true)] {
        for frame in cursor.ingest(events(at, down)).unwrap() {
            sink.push(Record::Frame { frame }).unwrap();
        }
    }
    sink.push(Record::Tick { at: ms(1800) }).unwrap();
    assert_eq!(matches(sink.0.take_events()), vec![fresh.source]);
}
