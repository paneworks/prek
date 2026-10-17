use super::*;
use std::{collections::VecDeque, mem::size_of};

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}
fn values(a: i32, b: i32) -> Values {
    BTreeMap::from([(0, a), (1, b)])
}
fn relay() -> Relay {
    Relay {
        codes: vec![0, 1],
        desired: None,
        applied: values(0, 0),
        flight: None,
        active: false,
        last: Duration::ZERO,
        failed: false,
    }
}
enum Reply {
    Block,
    Interrupt,
    Write(usize),
    Zero,
    Fault,
}
#[derive(Default)]
struct Port {
    replies: VecDeque<Reply>,
    bytes: Vec<u8>,
    calls: usize,
}
impl Write for Port {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.calls += 1;
        match self
            .replies
            .pop_front()
            .unwrap_or(Reply::Write(bytes.len()))
        {
            Reply::Block => Err(io::ErrorKind::WouldBlock.into()),
            Reply::Interrupt => Err(io::ErrorKind::Interrupted.into()),
            Reply::Zero => Ok(0),
            Reply::Fault => Err(io::ErrorKind::BrokenPipe.into()),
            Reply::Write(length) => {
                self.bytes.extend_from_slice(&bytes[..length]);
                Ok(length)
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn pregrab_stages_latest_leds_without_writing_or_starting_a_deadline() {
    let mut relay = relay();
    let mut port = Port::default();
    relay.snapshot(values(1, 0), values(0, 0), ms(1)).unwrap();
    assert!(relay.step(&mut port, false, ms(2)).unwrap().is_none());
    relay
        .snapshot(values(0, 1), values(0, 0), ms(2000))
        .unwrap();
    assert!(relay.step(&mut port, false, ms(3000)).unwrap().is_none());
    assert_eq!(port.calls, 0);
    assert!(!relay.pending());
    let written = relay.step(&mut port, true, ms(3001)).unwrap().unwrap();
    assert_eq!(written, BTreeMap::from([(1, 1)]));
    assert_eq!(
        port.bytes,
        crate::kernel::encode_events(&[RawEvent {
            kind: 17,
            code: 1,
            value: 1
        }])
        .unwrap()
    );
    relay.acknowledge(&written, values(0, 1)).unwrap();
    assert!(!relay.pending());
}

#[test]
fn aligned_backpressure_keeps_the_exact_cursor_then_sends_the_newest_desired_state() {
    let mut relay = relay();
    let event = size_of::<libc::input_event>();
    let mut port = Port {
        replies: VecDeque::from([Reply::Write(event), Reply::Block]),
        ..Default::default()
    };
    relay.snapshot(values(1, 1), values(0, 0), ms(0)).unwrap();
    assert!(relay.step(&mut port, true, ms(1)).unwrap().is_none());
    assert!(relay.pending());
    relay.snapshot(values(0, 1), values(1, 0), ms(2)).unwrap();
    let written = relay.step(&mut port, true, ms(3)).unwrap().unwrap();
    assert_eq!(written, values(1, 1));
    relay.acknowledge(&written, values(1, 1)).unwrap();
    assert!(relay.pending());
    let written = relay.step(&mut port, true, ms(4)).unwrap().unwrap();
    relay.acknowledge(&written, values(0, 1)).unwrap();
    assert_eq!(port.bytes.len(), event * 3);
    assert!(!relay.pending());
}

#[test]
fn repeated_updates_do_not_reset_a_stalled_physical_write_deadline() {
    let mut relay = relay();
    let mut port = Port {
        replies: VecDeque::from([Reply::Block, Reply::Block]),
        ..Default::default()
    };
    relay.snapshot(values(1, 0), values(0, 0), ms(0)).unwrap();
    relay.step(&mut port, true, ms(1)).unwrap();
    relay.snapshot(values(0, 1), values(0, 0), ms(249)).unwrap();
    relay.step(&mut port, true, ms(250)).unwrap();
    let calls = port.calls;
    assert_eq!(
        relay.step(&mut port, true, ms(251)).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    assert_eq!(port.calls, calls);
    assert!(relay.step(&mut port, true, ms(252)).is_err());
}

#[test]
fn bad_snapshots_reject_the_entire_request_without_any_source_write() {
    for bad in [BTreeMap::from([(30, 1)]), values(2, 0), BTreeMap::new()] {
        let mut relay = relay();
        let mut port = Port::default();
        assert!(relay.snapshot(bad, values(0, 0), ms(0)).is_err());
        assert!(relay.step(&mut port, true, ms(1)).is_err());
        assert_eq!(port.calls, 0);
    }
}

#[test]
fn frame_truncation_zero_progress_and_writer_fault_poison_the_bridge() {
    for reply in [Reply::Write(1), Reply::Zero, Reply::Fault] {
        let mut relay = relay();
        let mut port = Port {
            replies: VecDeque::from([reply]),
            ..Default::default()
        };
        relay.snapshot(values(1, 0), values(0, 0), ms(0)).unwrap();
        assert!(relay.step(&mut port, true, ms(1)).is_err());
        let calls = port.calls;
        assert!(relay.step(&mut port, true, ms(2)).is_err());
        assert_eq!(port.calls, calls);
    }
}

#[test]
fn interrupt_work_is_bounded_and_retried_from_the_same_batch() {
    let mut relay = relay();
    let mut port = Port {
        replies: (0..17).map(|_| Reply::Interrupt).collect(),
        ..Default::default()
    };
    relay.snapshot(values(1, 1), values(0, 0), ms(0)).unwrap();
    assert!(relay.step(&mut port, true, ms(1)).unwrap().is_none());
    assert_eq!(port.calls, 16);
    assert!(relay.pending());
    assert_eq!(
        relay.step(&mut port, true, ms(2)).unwrap().unwrap(),
        values(1, 1)
    );
    assert_eq!(port.bytes.len(), 2 * size_of::<libc::input_event>());
}

#[test]
fn bad_readback_clock_regression_and_pause_cannot_turn_into_readiness() {
    let mut relay = relay();
    let mut port = Port::default();
    relay.snapshot(values(1, 0), values(0, 0), ms(5)).unwrap();
    let written = relay.step(&mut port, true, ms(6)).unwrap().unwrap();
    assert!(relay.acknowledge(&written, values(0, 0)).is_err());
    assert!(!relay.pending());
    assert!(relay.step(&mut port, true, ms(7)).is_err());
    let mut relay = super::tests::relay();
    relay.snapshot(values(0, 0), values(0, 0), ms(5)).unwrap();
    relay.step(&mut port, true, ms(6)).unwrap();
    assert!(relay.step(&mut port, false, ms(7)).is_err());
    let mut relay = super::tests::relay();
    assert!(relay.advance(Duration::MAX).is_err());
    let mut relay = super::tests::relay();
    relay.advance(ms(5)).unwrap();
    assert!(relay.advance(ms(4)).is_err());
}

#[test]
fn complete_snapshot_recovers_latest_led_state_even_without_a_producer_request() {
    let mut relay = relay();
    let mut port = Port::default();
    relay.snapshot(values(1, 0), values(0, 0), ms(0)).unwrap();
    let written = relay.step(&mut port, true, ms(1)).unwrap().unwrap();
    relay.acknowledge(&written, values(1, 0)).unwrap();
    relay.snapshot(values(0, 1), values(1, 0), ms(251)).unwrap();
    let written = relay.step(&mut port, true, ms(252)).unwrap().unwrap();
    relay.acknowledge(&written, values(0, 1)).unwrap();
    assert!(!relay.pending());
}

#[test]
fn source_led_echo_cannot_overwrite_virtual_consumer_intent_or_change_key_frames() {
    let mut frame = Frame {
        source: crate::input::Source {
            device: 9,
            generation: 3,
            seat: "seat0".into(),
        },
        sequence: 42,
        at: ms(7),
        events: vec![
            RawEvent {
                kind: 1,
                code: 30,
                value: 1,
            },
            RawEvent {
                kind: 17,
                code: 0,
                value: 0,
            },
            RawEvent {
                kind: 1,
                code: 30,
                value: 0,
            },
            RawEvent {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 1,
            },
        ],
    };
    let mut expected = frame.clone();
    expected.events.remove(1);
    assert_eq!(remove_echo(&frame, &[0, 1]).unwrap().as_ref(), &expected);
    assert!(frame.events.iter().any(|event| event.kind == 17));
    assert!(matches!(
        remove_echo(&expected, &[0, 1]).unwrap(),
        Cow::Borrowed(_)
    ));
    frame.events = vec![
        RawEvent {
            kind: 17,
            code: 1,
            value: 1,
        },
        *expected.events.last().unwrap(),
    ];
    assert_eq!(
        remove_echo(&frame, &[0, 1]).unwrap().events,
        [*expected.events.last().unwrap()]
    );
}

#[test]
fn invalid_feedback_and_partial_frames_are_not_partially_filtered_or_given_a_report() {
    for events in [
        vec![RawEvent {
            kind: 17,
            code: 0,
            value: 1,
        }],
        vec![
            RawEvent {
                kind: 17,
                code: 0,
                value: 1,
            },
            RawEvent {
                kind: 17,
                code: 2,
                value: 1,
            },
            RawEvent {
                kind: 0,
                code: 0,
                value: 0,
            },
        ],
        vec![
            RawEvent {
                kind: 17,
                code: 0,
                value: 2,
            },
            RawEvent {
                kind: 0,
                code: 0,
                value: 0,
            },
        ],
    ] {
        let frame = Frame {
            source: crate::input::Source {
                device: 9,
                generation: 3,
                seat: "seat0".into(),
            },
            sequence: 3,
            at: ms(2),
            events,
        };
        let original = frame.clone();
        assert!(remove_echo(&frame, &[0, 1]).is_err());
        assert_eq!(frame, original);
    }
}
