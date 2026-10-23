use super::*;
use crate::{
    linux::handoff::{Cut, Preparation},
    route_worker::takeover::Cutover,
};
use evdev::InputEvent;
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt};

#[derive(Default)]
struct Fake {
    observe: Vec<InputEvent>,
    refuse_grab: bool,
    fail_classify: bool,
    observed: Vec<InputEvent>,
    retained: Vec<InputEvent>,
    grabbed: bool,
    released: bool,
    committed: bool,
}
impl Cutover for Fake {
    fn prepare(&mut self) -> io::Result<Preparation> {
        Ok(if self.observe.is_empty() {
            Preparation::Armed
        } else {
            Preparation::Observe(std::mem::take(&mut self.observe))
        })
    }
    fn grab(&mut self) -> io::Result<()> {
        if self.refuse_grab {
            return Err(io::Error::from(io::ErrorKind::ResourceBusy));
        }
        self.grabbed = true;
        Ok(())
    }
    fn classify(&mut self) -> io::Result<Cut> {
        if self.fail_classify {
            return Err(io::Error::other("evdev witness and original prefix differ"));
        }
        Ok(Cut {
            observed: self.observed.clone(),
            retained: self.retained.clone(),
        })
    }
    fn commit(&mut self) -> io::Result<Cut> {
        self.committed = true;
        Ok(Cut {
            observed: self.observed.clone(),
            retained: self.retained.clone(),
        })
    }
    fn release(&mut self) {
        self.released = true;
        self.grabbed = false;
    }
}

fn events(values: &[(u16, u16, i32)]) -> Vec<InputEvent> {
    values
        .iter()
        .map(|&(kind, code, value)| InputEvent::new(kind, code, value))
        .collect()
}

fn prepared(fixture: &mut Fixture) {
    let now = monotonic().unwrap();
    let descriptor = Descriptor::mirror(
        &device(),
        KernelIdentity {
            bus: 3,
            vendor: 1,
            product: 1,
            version: 1,
        },
        None,
        &BTreeMap::new(),
        BTreeMap::new(),
    )
    .unwrap();
    let publication = fixture
        .stream
        .registry
        .prepare_isolated(descriptor)
        .unwrap();
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(fixture.directory.join("mirror-bytes"))
        .unwrap();
    let virtual_device = Virtual::ordinary_file_fixture(publication, file).unwrap();
    let descriptor = virtual_device.descriptor().clone();
    let consumer = fixture.owner.consumer.clone().unwrap();
    let mut readiness = Readiness::new(descriptor.clone(), consumer.clone(), now).unwrap();
    readiness.metadata(&descriptor, now).unwrap();
    readiness
        .consumer(
            &ConsumerEvidence {
                consumer,
                marker: descriptor.marker.clone(),
                name: descriptor.name.clone(),
                class: descriptor.class,
                seat: "seat0".into(),
                settings: BTreeMap::new(),
                at: now,
            },
            now,
        )
        .unwrap();
    let worker =
        serde_json::from_value(serde_json::json!({"pid":std::process::id(),"birth":1,"epoch":1}))
            .unwrap();
    let mut admission =
        crate::morf::admission::Gate::new(worker, fixture.owner.fence, device().source);
    admission.update(None, now).unwrap();
    fixture.stream.outputs.insert(
        device().source,
        Output {
            fence: fixture.owner.fence,
            admission,
            virtual_device,
            state_probe: None,
            feedback: None,
            readiness: Some(readiness),
            consumer: None,
            offered: now,
            started: now,
            in_flight: None,
            sequence: 0,
        },
    );
}

#[test]
fn a_balanced_cut_commits_and_attaches_the_source_at_its_grab() {
    let mut fixture = Fixture::subscribed(profile(), owner());
    prepared(&mut fixture);
    let mut cutover = Fake {
        observed: events(&[
            (EV_KEY, 30, 1),
            (EV_SYN, 0, 0),
            (EV_KEY, 30, 0),
            (EV_SYN, 0, 0),
        ]),
        retained: events(&[(EV_KEY, 48, 1), (EV_SYN, 0, 0)]),
        ..Fake::default()
    };
    let source = device().source;
    let disposition = fixture.stream.take_over(&device(), &mut cutover).unwrap();
    assert!(matches!(&disposition, ReaderDisposition::Cutover(cut) if cut.retained.len() == 2));
    assert!(cutover.committed && !cutover.released);
    assert!(!fixture.stream.grabbed.contains(&source));
    fixture.stream.attach_takeover(&source).unwrap();
    assert!(fixture.stream.grabbed.contains(&source));
    assert!(fixture.stream.seats["seat0"].routing.has_source(&source));
    assert!(fixture.stream.attach_takeover(&source).is_err());
    assert!(fixture.stream.refused.is_empty());
}

#[test]
fn an_unbalanced_prefix_releases_the_grab_and_returns_every_consumed_event() {
    let mut fixture = Fixture::subscribed(profile(), owner());
    prepared(&mut fixture);
    let observed = events(&[(EV_KEY, 30, 1), (EV_SYN, 0, 0)]);
    let retained = events(&[(EV_KEY, 30, 0), (EV_SYN, 0, 0)]);
    let mut cutover = Fake {
        observed: observed.clone(),
        retained: retained.clone(),
        ..Fake::default()
    };
    let disposition = fixture.stream.take_over(&device(), &mut cutover).unwrap();
    let mut consumed = observed;
    consumed.extend(retained);
    assert!(matches!(disposition, ReaderDisposition::Observe(events) if events == consumed));
    assert!(cutover.released && !cutover.committed);
    assert!(fixture.stream.committed.is_empty() && fixture.stream.refused.is_empty());
    assert!(fixture.stream.outputs[&device().source].readiness.is_some());
}

#[test]
fn pending_input_is_observed_before_any_grab() {
    let mut fixture = Fixture::subscribed(profile(), owner());
    prepared(&mut fixture);
    let pending = events(&[(EV_KEY, 48, 1), (EV_SYN, 0, 0)]);
    let mut cutover = Fake {
        observe: pending.clone(),
        ..Fake::default()
    };
    let disposition = fixture.stream.take_over(&device(), &mut cutover).unwrap();
    assert!(matches!(disposition, ReaderDisposition::Observe(events) if events == pending));
    assert!(!cutover.grabbed && !cutover.committed);
    assert!(fixture.stream.outputs.contains_key(&device().source));
}

#[test]
fn a_failed_grab_classification_or_readiness_refuses_the_source_and_removes_its_mirror() {
    for failure in 0..3 {
        let mut fixture = Fixture::subscribed(profile(), owner());
        prepared(&mut fixture);
        let mut cutover = Fake {
            refuse_grab: failure == 0,
            fail_classify: failure == 1,
            observed: events(&[
                (EV_KEY, 48, 1),
                (EV_SYN, 0, 0),
                (EV_KEY, 48, 0),
                (EV_SYN, 0, 0),
            ]),
            ..Fake::default()
        };
        if failure == 2 {
            fixture
                .stream
                .outputs
                .get_mut(&device().source)
                .unwrap()
                .readiness = None;
        }
        let disposition = fixture.stream.take_over(&device(), &mut cutover).unwrap();
        assert!(!cutover.committed && !cutover.grabbed);
        assert!(fixture.stream.refused.contains(&device().source));
        assert!(!fixture.stream.outputs.contains_key(&device().source));
        assert!(fixture.stream.committed.is_empty());
        match failure {
            2 => assert!(
                matches!(disposition, ReaderDisposition::Observe(events) if events.len() == 4)
            ),
            _ => assert!(matches!(disposition, ReaderDisposition::Keep)),
        }
    }
}
