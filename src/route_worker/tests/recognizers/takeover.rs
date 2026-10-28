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
    prepared_device(fixture, &device());
}

fn prepared_device(fixture: &mut Fixture, device: &Device) {
    let now = monotonic().unwrap();
    let descriptor = Descriptor::mirror(
        device,
        KernelIdentity {
            bus: 3,
            vendor: device.vendor,
            product: device.product,
            version: 1,
        },
        None,
        &BTreeMap::new(),
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
                at: now,
            },
            now,
        )
        .unwrap();
    fixture.stream.outputs.insert(
        device.source.clone(),
        Output {
            fence: fixture.owner.fence,
            virtual_device,
            state_probe: None,
            feedback: None,
            readiness: Some(readiness),
            consumer: None,
            offered: now,
            started: now,
            in_flight: None,
            queued: None,
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

#[test]
fn a_taken_over_touchscreen_maps_onto_the_only_output_and_dispatches_its_swipe() {
    use crate::input::{ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID};
    let mut touch = crate::transport::tests::touch();
    for axis in touch.axes.values_mut() {
        axis.fuzz = 0;
    }
    touch.identity = "test:touch".into();
    touch.name = "Prek test touchscreen".into();
    touch.physical = "fixture/touch".into();
    touch.path = String::new();
    let mut config = profile();
    config.policy.allowed_devices = vec!["touch".into()];
    config.devices = BTreeMap::from([(
        "touch".into(),
        Selector {
            identity: Some(touch.identity.clone()),
            ..Selector::default()
        },
    )]);
    let rule = &mut config.rules[0];
    rule.devices = vec!["touch".into()];
    rule.contexts = vec![crate::config::Context::Desktop];
    rule.delivery = Delivery::Buffer;
    rule.trigger = Trigger::Swipe {
        fingers: 3,
        direction: crate::config::Direction::Down,
        travel: crate::config::Distance::Millimetres(15.0),
        fallback_travel: crate::config::Distance::Normalized(0.1),
        continuous: false,
    };
    let mut owner = owner();
    owner.context.as_mut().unwrap().snapshot.outputs = vec![crate::context::OutputState {
        geometry: crate::gestures::Output {
            name: "DSI-1".into(),
            pixel_width: 600.0,
            pixel_height: 1200.0,
            scale: 1.0,
            origin: crate::gestures::Point { x: 0.0, y: 0.0 },
            transform: crate::gestures::Transform::Normal,
        },
        geometry_generation: 2,
        screen: crate::config::Screen::On,
    }];
    let mut fixture = Fixture::subscribed(config, owner);
    fixture
        .stream
        .push(Record::Device {
            device: touch.clone(),
            keys: BTreeSet::new(),
            contacts: vec![],
        })
        .unwrap();
    prepared_device(&mut fixture, &touch);
    let disposition = fixture
        .stream
        .take_over(&touch, &mut Fake::default())
        .unwrap();
    assert!(matches!(disposition, ReaderDisposition::Cutover(_)));
    fixture.stream.attach_takeover(&touch.source).unwrap();
    assert_eq!(
        fixture.stream.mapped[&touch.source],
        Some(("DSI-1".into(), 2))
    );
    let mut push = |sequence: u64, wait: u64, values: Vec<(u16, u16, i32)>| {
        std::thread::sleep(Duration::from_millis(wait));
        let mut events: Vec<_> = values
            .into_iter()
            .map(|(kind, code, value)| RawEvent { kind, code, value })
            .collect();
        events.push(RawEvent {
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        });
        fixture
            .stream
            .push(Record::Frame {
                frame: Frame {
                    source: touch.source.clone(),
                    at: monotonic().unwrap(),
                    sequence,
                    events,
                },
            })
            .unwrap();
    };
    let mut down = vec![(EV_KEY, 0x14a, 1)];
    let mut moved = Vec::new();
    let mut up = vec![(EV_KEY, 0x14a, 0)];
    for slot in 0..3 {
        down.extend([
            (crate::input::EV_ABS, ABS_MT_SLOT, slot),
            (crate::input::EV_ABS, ABS_MT_TRACKING_ID, slot + 10),
            (crate::input::EV_ABS, ABS_MT_POSITION_X, 1000 + slot * 500),
            (crate::input::EV_ABS, ABS_MT_POSITION_Y, 1000),
        ]);
        moved.extend([
            (crate::input::EV_ABS, ABS_MT_SLOT, slot),
            (crate::input::EV_ABS, ABS_MT_POSITION_Y, 2500),
        ]);
        up.extend([
            (crate::input::EV_ABS, ABS_MT_SLOT, slot),
            (crate::input::EV_ABS, ABS_MT_TRACKING_ID, -1),
        ]);
    }
    push(1, 0, down);
    push(2, 140, moved);
    push(3, 40, up);
    let messages = fixture.drain();
    assert!(
        messages.iter().any(|message| matches!(
            message,
            Message::Outcome {
                report: Report::Dispatch { .. }
            }
        )),
        "{messages:?}"
    );
}
