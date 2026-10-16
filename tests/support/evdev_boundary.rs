//! Kernel experiments on our grabbed, libinput-ignored synthetic keyboard.
//! No producer writes occur while the source is temporarily ungrabbed.
use super::kernel_fixture::*;
use prek::{
    input::{EV_KEY, EV_SYN, SYN_DROPPED},
    kernel::Virtual,
    linux::StateProbe,
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
