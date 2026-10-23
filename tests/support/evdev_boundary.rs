//! Kernel experiments on our grabbed, libinput-ignored synthetic keyboard.
//! Key events are emitted only under the test's exclusive grab.
use super::kernel_fixture::*;
use prek::{
    input::{EV_KEY, EV_SYN, SYN_DROPPED},
    kernel::Virtual,
    linux::{
        StateProbe,
        handoff::{Handoff, Preparation, ReadFailure},
    },
};

#[test]
#[ignore = "requires user-launched isolated root routing runner"]
fn pregrab_backlog_and_postgrab_input_share_the_reader_queue() {
    let registry = authorized();
    let device = fixture(false);
    let mut source =
        Virtual::create(registry.prepare_isolated(descriptor(&device)).unwrap()).unwrap();
    let mut reader = Receiver::open(&source, &device.source);
    let probe = StateProbe::new(&reader.raw).unwrap();
    send(&mut source, 1, &[(EV_KEY, 30, 1)]);
    send(&mut source, 2, &[(EV_KEY, 30, 0)]);
    assert!(probe.is_neutral(&device).unwrap());
    reader.raw.ungrab().unwrap();
    // Our sole producer is idle here: no console or compositor sees a key.
    reader.raw.grab().unwrap();
    send(&mut source, 3, &[(EV_KEY, 125, 1)]);
    send(&mut source, 4, &[(EV_KEY, 125, 0)]);
    assert!(probe.is_neutral(&device).unwrap());
    let values: Vec<_> = reader
        .raw
        .fetch_events()
        .unwrap()
        .map(|event| (event.event_type().0, event.code(), event.value()))
        .collect();
    let keys: Vec<_> = values
        .into_iter()
        .filter(|(kind, _, _)| *kind == EV_KEY)
        .collect();
    assert_eq!(
        keys,
        [
            (EV_KEY, 30, 1),
            (EV_KEY, 30, 0),
            (EV_KEY, 125, 1),
            (EV_KEY, 125, 0)
        ]
    );
}

#[test]
#[ignore = "requires user-launched isolated root routing runner"]
fn clock_change_with_backlog_reports_loss_before_fresh_input() {
    use std::os::fd::AsRawFd;
    nix::ioctl_write_ptr!(clock_id, b'E', 0xa0, nix::libc::c_int);
    let registry = authorized();
    let device = fixture(false);
    let mut source =
        Virtual::create(registry.prepare_isolated(descriptor(&device)).unwrap()).unwrap();
    let mut reader = Receiver::open(&source, &device.source);
    send(&mut source, 1, &[(EV_KEY, 30, 1)]);
    send(&mut source, 2, &[(EV_KEY, 30, 0)]);
    for clock in [nix::libc::CLOCK_BOOTTIME, nix::libc::CLOCK_MONOTONIC] {
        // SAFETY: our verified, grabbed synthetic evdev fd and a c_int clock.
        unsafe { clock_id(reader.raw.as_raw_fd(), &clock) }.unwrap();
    }
    send(&mut source, 3, &[(EV_KEY, 125, 1)]);
    send(&mut source, 4, &[(EV_KEY, 125, 0)]);
    let events: Vec<_> = reader.raw.fetch_events().unwrap().collect();
    assert!(
        events
            .iter()
            .any(|event| event.event_type().0 == EV_SYN && event.code() == SYN_DROPPED)
    );
    let keys: Vec<_> = events
        .iter()
        .filter(|event| event.event_type().0 == EV_KEY)
        .map(|event| (event.code(), event.value()))
        .collect();
    assert_eq!(keys, [(125, 1), (125, 0)]);
}

#[test]
#[ignore = "requires user-launched isolated root routing runner"]
fn witness_prefix_retains_concurrent_motion_after_the_grab() {
    use prek::input::EV_REL;
    let registry = authorized();
    let mut device = fixture(false);
    device.name = format!("Prek test motion {}", device.source.device);
    device.capabilities =
        std::collections::BTreeMap::from([(EV_SYN, vec![0]), (EV_REL, vec![0, 1])]);
    let mut source =
        Virtual::create(registry.prepare_isolated(descriptor(&device)).unwrap()).unwrap();
    let mut receiver = Receiver::open(&source, &device.source);
    assert!(receiver.raw.supported_keys().is_none());
    receiver.raw.ungrab().unwrap();
    let mut handoff = Handoff::new(&mut receiver.raw).unwrap();
    assert_eq!(handoff.prepare().unwrap(), Preparation::Armed);
    send(&mut source, 1, &[(EV_REL, 0, 11)]);
    let start = std::sync::Arc::new(std::sync::Barrier::new(2));
    let worker_start = start.clone();
    let producer = std::thread::spawn(move || {
        worker_start.wait();
        for sequence in 2..=9 {
            send(&mut source, sequence, &[(EV_REL, 0, sequence as i32 + 10)]);
        }
        source
    });
    start.wait();
    handoff.grab().unwrap();
    handoff.classify().unwrap();
    let cut = handoff.commit().unwrap();
    let mut source = producer.join().unwrap();
    send(&mut source, 10, &[(EV_REL, 0, 99)]);
    let tuples = |events: &[evdev::InputEvent]| {
        events
            .iter()
            .map(|event| (event.event_type().0, event.code(), event.value()))
            .collect::<Vec<_>>()
    };
    assert!(!cut.observed.is_empty());
    assert_eq!(cut.observed.len() % 2, 0);
    assert!(
        cut.observed
            .iter()
            .all(|event| event.event_type().0 != EV_REL || event.value() < 99)
    );
    let mut actual = cut.observed;
    actual.extend(cut.retained);
    loop {
        match receiver.raw.fetch_events() {
            Ok(events) => {
                let events: Vec<_> = events.collect();
                if events.is_empty() {
                    break;
                }
                actual.extend(events);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("own motion fixture read failed: {error}"),
        }
    }
    let expected: Vec<_> = (11..=19)
        .chain([99])
        .flat_map(|value| [(EV_REL, 0, value), (EV_SYN, 0, 0)])
        .collect();
    assert_eq!(tuples(&actual), expected);
    assert!(receiver.raw.is_grabbed());
    receiver.raw.ungrab().unwrap();
    let mut handoff = Handoff::new(&mut receiver.raw).unwrap();
    assert_eq!(handoff.prepare().unwrap(), Preparation::Armed);
    send(&mut source, 100, &[(EV_REL, 0, 101)]);
    handoff.grab().unwrap();
    send(&mut source, 101, &[(EV_REL, 0, 102)]);
    let cut = handoff.classify().unwrap();
    let expected: Vec<_> = cut.observed.iter().chain(&cut.retained).copied().collect();
    let matched = cut.observed.len();
    let error = handoff.classify().unwrap_err();
    let failure = error
        .into_inner()
        .unwrap()
        .downcast::<ReadFailure>()
        .unwrap();
    let (_, journal) = failure.into_parts();
    assert_eq!(journal.original(), expected);
    assert_eq!(journal.matched_prefix(), matched);
    assert!(journal.witness_frozen() && journal.grab_acquired());
    assert!(handoff.commit().is_err());
    assert!(!receiver.raw.is_grabbed());
    let mut actual = journal.into_original();
    match receiver.raw.fetch_events() {
        Ok(events) => actual.extend(events),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(error) => panic!("unread recovery tail: {error}"),
    }
    assert_eq!(
        tuples(&actual),
        [
            (EV_REL, 0, 101),
            (EV_SYN, 0, 0),
            (EV_REL, 0, 102),
            (EV_SYN, 0, 0)
        ]
    );
}

#[test]
#[ignore = "requires user-launched isolated root routing runner"]
fn witness_without_prefix_preserves_a_postgrab_held_key() {
    let registry = authorized();
    let device = fixture(false);
    let mut source =
        Virtual::create(registry.prepare_isolated(descriptor(&device)).unwrap()).unwrap();
    let mut receiver = Receiver::open(&source, &device.source);
    receiver.raw.ungrab().unwrap();
    let mut handoff = Handoff::new(&mut receiver.raw).unwrap();
    assert_eq!(handoff.prepare().unwrap(), Preparation::Armed);
    handoff.grab().unwrap();
    send(&mut source, 1, &[(EV_KEY, 30, 1)]);
    let cut = handoff.classify().unwrap();
    assert!(cut.observed.is_empty() && cut.retained.is_empty());
    handoff.commit().unwrap();
    send(&mut source, 2, &[(EV_KEY, 30, 0)]);
    let events: Vec<_> = receiver.raw.fetch_events().unwrap().collect();
    let keys: Vec<_> = events
        .iter()
        .filter(|event| event.event_type().0 == EV_KEY)
        .map(|event| (event.code(), event.value()))
        .collect();
    assert_eq!(keys, [(30, 1), (30, 0)]);
    assert!(receiver.raw.is_grabbed());
}

#[test]
#[ignore = "requires user-launched isolated root routing runner"]
fn an_active_switch_keeps_the_source_out_of_a_neutral_handoff() {
    // SW_CAMERA_LENS_COVER.
    const LENS_COVER: u16 = 9;
    let registry = authorized();
    let mut device = fixture(false);
    device
        .capabilities
        .insert(prek::input::EV_SW, vec![LENS_COVER]);
    let mut source =
        Virtual::create(registry.prepare_isolated(descriptor(&device)).unwrap()).unwrap();
    let reader = Receiver::open(&source, &device.source);
    assert!(reader.raw.is_grabbed());
    let probe = StateProbe::new(&reader.raw).unwrap();
    assert!(probe.is_neutral(&device).unwrap());
    send(&mut source, 1, &[(prek::input::EV_SW, LENS_COVER, 1)]);
    assert!(!probe.is_neutral(&device).unwrap());
    send(&mut source, 2, &[(prek::input::EV_SW, LENS_COVER, 0)]);
    assert!(probe.is_neutral(&device).unwrap());
}
