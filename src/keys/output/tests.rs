use super::*;
use crate::keys::SourceKey;
use std::{cell::RefCell, collections::VecDeque, rc::Rc};

mod software;

enum Step {
    Prefix(usize),
    Complete,
    Error(io::ErrorKind),
}
#[derive(Default)]
struct State {
    frames: Vec<Frame>,
    events: Vec<RawEvent>,
    steps: VecDeque<Step>,
    pending: Option<(Frame, usize)>,
    enqueue_error: bool,
    flushes: usize,
    drops: usize,
    abort_override: Option<Result<Vec<RawEvent>, io::ErrorKind>>,
}
struct TestSink {
    source: Source,
    state: Rc<RefCell<State>>,
}
impl Sink for TestSink {
    fn source(&self) -> &Source {
        &self.source
    }
    fn supports_key(&self, code: u16) -> bool {
        (1..767).contains(&code)
    }
    fn enqueue(&mut self, frame: &Frame) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        assert!(state.pending.is_none());
        assert_eq!(frame.source, self.source);
        state.frames.push(frame.clone());
        if state.enqueue_error {
            return Err(io::Error::other("enqueue failed"));
        }
        state.pending = Some((frame.clone(), 0));
        Ok(())
    }
    fn flush(&mut self) -> io::Result<Progress> {
        let mut state = self.state.borrow_mut();
        state.flushes += 1;
        let (frame, offset) = state.pending.take().unwrap();
        let step = state.steps.pop_front().unwrap_or(Step::Complete);
        let through = match step {
            Step::Prefix(count) => (offset + count).min(frame.events.len()),
            Step::Complete => frame.events.len(),
            Step::Error(kind) => {
                state.pending = Some((frame, offset));
                return Err(io::Error::new(kind, "scripted sink failure"));
            }
        };
        state
            .events
            .extend_from_slice(&frame.events[offset..through]);
        if matches!(step, Step::Complete) {
            Ok(Progress::Complete)
        } else {
            state.pending = Some((frame, through));
            Ok(Progress::Pending)
        }
    }
    fn abort(&mut self) -> io::Result<Vec<RawEvent>> {
        let mut state = self.state.borrow_mut();
        if let Some(result) = state.abort_override.take() {
            return result.map_err(io::Error::from);
        }
        let (frame, offset) = state.pending.take().unwrap();
        Ok(frame.events[..offset].to_vec())
    }
}
impl Drop for TestSink {
    fn drop(&mut self) {
        self.state.borrow_mut().drops += 1;
    }
}
fn source(device: u64) -> Source {
    Source {
        device,
        generation: 1,
        seat: "seat0".into(),
    }
}
fn physical(device: u64, code: u16) -> Claim {
    Claim {
        owner: Owner::Physical(SourceKey {
            source: source(device),
            code,
        }),
        origin: Some(source(device)),
        configuration: 1,
        serial: u64::from(code),
    }
}
fn synthetic(id: u64, generation: u64, origin: Option<Source>) -> Claim {
    Claim {
        owner: Owner::Synthetic {
            seat: "seat0".into(),
            generation,
            id,
        },
        origin,
        configuration: generation,
        serial: id,
    }
}
fn model() -> (Output<TestSink>, Rc<RefCell<State>>) {
    let state = Rc::new(RefCell::new(State::default()));
    (
        Output::new(TestSink {
            source: source(100),
            state: state.clone(),
        })
        .unwrap(),
        state,
    )
}
fn drain(output: &mut Output<TestSink>) {
    for _ in 0..16 {
        if output.flush().unwrap() == Progress::Complete {
            assert!(output.settled());
            return;
        }
    }
    panic!("key output did not drain");
}
fn events(values: &[(u16, i32)]) -> Vec<RawEvent> {
    values
        .iter()
        .map(|&(code, value)| RawEvent {
            kind: EV_KEY,
            code,
            value,
        })
        .chain([RawEvent {
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        }])
        .collect()
}

fn remap(id: u64, generation: u64, device: u64) -> Claim {
    let key = SourceKey {
        source: source(device),
        code: 30,
    };
    Claim {
        owner: Owner::Remap {
            key,
            generation,
            id,
        },
        origin: Some(source(device)),
        configuration: generation,
        serial: id,
    }
}

#[test]
fn remap_instances_share_target_counts_without_aliasing_same_source_owners() {
    let (mut output, state) = model();
    let one = remap(1, 1, 0);
    let two = remap(2, 1, 0);
    output.hold(one.clone(), &[42], Duration::ZERO).unwrap();
    drain(&mut output);
    output.hold(two.clone(), &[42], Duration::ZERO).unwrap();
    drain(&mut output);
    output.release(&one, Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(output.down(42));
    output.repeat(&two, Duration::ZERO).unwrap();
    drain(&mut output);
    output.cancel(&one, Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(output.down(42));
    output.release(&two, Duration::ZERO).unwrap();
    drain(&mut output);
    assert_eq!(
        state.borrow().events,
        [events(&[(42, 1)]), events(&[(42, 2)]), events(&[(42, 0)])].concat()
    );
}

#[test]
fn remap_instances_require_exact_origin_generation_serial_and_one_target() {
    for variant in 0..5 {
        let (mut output, state) = model();
        let mut claim = remap(1, 1, 0);
        match variant {
            0 => claim.origin = Some(source(2)),
            1 => claim.origin = None,
            2 => claim.configuration = 2,
            3 => claim.serial = 2,
            _ => {}
        }
        let keys = if variant == 4 { vec![42, 30] } else { vec![42] };
        assert!(output.hold(claim, &keys, Duration::ZERO).is_err());
        assert!(state.borrow().frames.is_empty());
    }
    let (mut output, _) = model();
    assert!(output.tap(remap(1, 1, 0), &[30], Duration::ZERO).is_err());
}

#[test]
fn generation_retirement_releases_only_its_matching_remap_instances() {
    let (mut output, _) = model();
    output.hold(remap(1, 1, 1), &[42], Duration::ZERO).unwrap();
    drain(&mut output);
    output.hold(remap(1, 2, 1), &[42], Duration::ZERO).unwrap();
    drain(&mut output);
    output.retire_generation(1, Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(output.down(42));
    output.retire_source(&source(1), Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(!output.down(42));
}

#[test]
fn t46_partial_frames_commit_once_only_after_the_whole_frame() {
    let (mut output, state) = model();
    let claim = physical(1, 30);
    state
        .borrow_mut()
        .steps
        .extend([Step::Prefix(1), Step::Prefix(0), Step::Complete]);
    assert_eq!(
        output.hold(claim.clone(), &[42], Duration::ZERO).unwrap(),
        Progress::Pending
    );
    assert!(!output.down(42));
    assert!(!output.settled());
    for _ in 0..2 {
        assert_eq!(output.flush().unwrap(), Progress::Pending);
        assert!(!output.down(42));
        assert_eq!(
            output
                .hold(physical(2, 31), &[42], Duration::ZERO)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
    }
    drain(&mut output);
    assert!(output.down(42));
    assert_eq!(state.borrow().events, events(&[(42, 1)]));
    assert_eq!(state.borrow().frames.len(), 1);
    output.release(&claim, Duration::ZERO).unwrap();
    assert!(output.down(42));
    drain(&mut output);
    assert!(!output.down(42));
    assert_eq!(state.borrow().frames[1].events, events(&[(42, 0)]));
}

#[test]
fn t46_overlapping_remaps_keep_the_last_owner_and_duplicate_edges_do_not_rebind() {
    for (device, code) in [(1, 31), (2, 30), (2, 31)] {
        let (mut output, state) = model();
        let a = physical(1, 30);
        let mut b = physical(device, code);
        b.serial = 31;
        output.hold(a.clone(), &[125], Duration::ZERO).unwrap();
        drain(&mut output);
        assert_eq!(
            output.hold(a.clone(), &[125], Duration::ZERO).unwrap(),
            Progress::Complete
        );
        assert_eq!(
            output.hold(b.clone(), &[125], Duration::ZERO).unwrap(),
            Progress::Complete
        );
        assert!(output.hold(a.clone(), &[42], Duration::ZERO).is_err());
        assert_eq!(
            output.release(&a, Duration::ZERO).unwrap(),
            Progress::Complete
        );
        assert!(output.down(125));
        assert_eq!(state.borrow().frames.len(), 1);
        output.release(&b, Duration::ZERO).unwrap();
        drain(&mut output);
        assert!(!output.down(125));
        assert_eq!(state.borrow().frames.len(), 2);
    }
}

#[test]
fn t46_chord_press_then_reverse_release_and_receipt_deduplication() {
    let (mut output, state) = model();
    let claim = synthetic(7, 1, Some(source(1)));
    output
        .tap(claim.clone(), &[29, 30], Duration::ZERO)
        .unwrap();
    assert_eq!(output.flush().unwrap(), Progress::Pending);
    assert!(output.down(29) && output.down(30));
    assert_eq!(state.borrow().flushes, 1);
    assert_eq!(state.borrow().frames.len(), 2);
    assert_eq!(state.borrow().frames[0].events, events(&[(29, 1), (30, 1)]));
    assert_eq!(state.borrow().frames[1].events, events(&[(30, 0), (29, 0)]));
    drain(&mut output);
    assert!(!output.down(29) && !output.down(30));
    assert_eq!(
        output
            .tap(claim, &[29, 30], Duration::ZERO)
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(state.borrow().frames.len(), 2);
    assert!(output.into_sink().is_ok());
}

#[test]
fn t46_generated_chord_never_releases_an_overlapping_remap_modifier() {
    let (mut output, state) = model();
    let remap = physical(1, 30);
    output.hold(remap.clone(), &[42], Duration::ZERO).unwrap();
    drain(&mut output);
    output
        .tap(synthetic(32, 1, Some(source(2))), &[42, 30], Duration::ZERO)
        .unwrap();
    drain(&mut output);
    assert!(output.down(42));
    assert!(!output.down(30));
    assert_eq!(state.borrow().frames[1].events, events(&[(30, 1)]));
    assert_eq!(state.borrow().frames[2].events, events(&[(30, 0)]));
    output.release(&remap, Duration::ZERO).unwrap();
    drain(&mut output);
    assert_eq!(state.borrow().frames[3].events, events(&[(42, 0)]));
}

#[test]
fn t46_source_retirement_survives_backpressure_and_cancels_source_bound_synthetic_keys() {
    let (mut output, state) = model();
    output.hold(physical(2, 31), &[42], Duration::ZERO).unwrap();
    drain(&mut output);
    let claim = synthetic(32, 1, Some(source(1)));
    output
        .hold(claim.clone(), &[29, 30], Duration::from_millis(10))
        .unwrap();
    state
        .borrow_mut()
        .steps
        .extend([Step::Prefix(1), Step::Complete]);
    output.flush().unwrap();
    assert_eq!(
        output.retire_source(&source(1), Duration::ZERO).unwrap(),
        Progress::Pending
    );
    drain(&mut output);
    assert!(output.down(42));
    assert!(!output.down(29) && !output.down(30));
    assert_eq!(state.borrow().frames[2].events, events(&[(29, 0)]));
    assert_eq!(state.borrow().frames[2].at, Duration::from_millis(10));
    let old = synthetic(2, 2, Some(source(1)));
    assert_eq!(
        output
            .hold(old, &[29], Duration::from_millis(10))
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    let mut fresh = source(1);
    fresh.generation = 2;
    output
        .hold(
            synthetic(2, 2, Some(fresh)),
            &[29],
            Duration::from_millis(10),
        )
        .unwrap();
    drain(&mut output);
    assert!(output.down(29));
}

#[test]
fn t46_generation_retirement_during_a_press_drains_and_prevents_stale_reentry() {
    let (mut output, state) = model();
    let claim = synthetic(1, 1, None);
    output
        .hold(claim.clone(), &[42], Duration::from_secs(1))
        .unwrap();
    output.retire_generation(1, Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(!output.down(42));
    assert!(output.receipts.is_empty());
    assert!(state.borrow().events.is_empty());
    assert_eq!(
        output
            .hold(claim, &[42], Duration::from_secs(1))
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    output
        .hold(synthetic(1, 2, None), &[42], Duration::from_secs(1))
        .unwrap();
    drain(&mut output);
    assert!(output.down(42));
}

#[test]
fn t46_retirement_drains_a_pending_chord_once_and_seals_new_work() {
    let (mut output, state) = model();
    output
        .tap(synthetic(1, 1, None), &[42, 30], Duration::from_secs(1))
        .unwrap();
    output.retire(Duration::ZERO).unwrap();
    drain(&mut output);
    assert_eq!(state.borrow().frames.len(), 1);
    assert!(state.borrow().events.is_empty());
    assert!(!output.down(42) && !output.down(30));
    assert_eq!(
        output
            .hold(physical(1, 30), &[42], Duration::from_secs(1))
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(output.into_sink().is_ok());
}

#[test]
fn t46_remap_repeats_preserve_ownership_and_never_become_new_presses() {
    let (mut output, state) = model();
    let claim = physical(1, 30);
    assert_eq!(
        output.repeat(&claim, Duration::ZERO).unwrap(),
        Progress::Complete
    );
    assert!(state.borrow().frames.is_empty());
    output.hold(claim.clone(), &[46], Duration::ZERO).unwrap();
    drain(&mut output);
    output.repeat(&claim, Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(output.down(46));
    assert_eq!(state.borrow().frames[1].events, events(&[(46, 2)]));
    output.release(&claim, Duration::ZERO).unwrap();
    drain(&mut output);
    assert_eq!(
        output
            .repeat(&synthetic(1, 1, None), Duration::ZERO)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
}

#[test]
fn t46_io_failure_is_terminal_retains_the_uncertain_frame_and_never_retries() {
    for kind in [
        io::ErrorKind::BrokenPipe,
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::WriteZero,
    ] {
        let (mut output, state) = model();
        output.hold(physical(1, 30), &[42], Duration::ZERO).unwrap();
        state
            .borrow_mut()
            .steps
            .extend([Step::Prefix(1), Step::Error(kind)]);
        output.flush().unwrap();
        assert_eq!(output.flush().unwrap_err().kind(), kind);
        assert!(output.failed());
        assert!(!output.settled());
        assert!(!output.down(42));
        assert_eq!(output.uncertain_frame().unwrap().events, events(&[(42, 1)]));
        assert_eq!(state.borrow().drops, 1);
        assert_eq!(state.borrow().events, vec![edge(42, true)]);
        for error in [
            output.flush().unwrap_err(),
            output.retire(Duration::ZERO).unwrap_err(),
            output
                .hold(physical(2, 30), &[29], Duration::ZERO)
                .unwrap_err(),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        }
        assert_eq!(state.borrow().frames.len(), 1);
        assert_eq!(state.borrow().flushes, 2);
        assert!(output.into_sink().is_err());
    }
}

#[test]
fn t46_enqueue_failure_is_also_uncertain_and_drops_the_sink() {
    let (mut output, state) = model();
    state.borrow_mut().enqueue_error = true;
    assert!(output.hold(physical(1, 30), &[42], Duration::ZERO).is_err());
    assert!(output.failed());
    assert!(output.uncertain_frame().is_some());
    assert_eq!(state.borrow().drops, 1);
    assert_eq!(state.borrow().flushes, 0);
}

#[test]
fn t46_invalid_claims_codes_and_times_cannot_modify_an_existing_owner() {
    let (mut output, state) = model();
    let claim = physical(1, 30);
    output
        .hold(claim.clone(), &[42], Duration::from_secs(1))
        .unwrap();
    drain(&mut output);
    let mut wrong_origin = physical(2, 30);
    wrong_origin.origin = Some(source(3));
    let mut wrong_seat = synthetic(1, 1, Some(source(1)));
    wrong_seat.origin.as_mut().unwrap().seat = "seat1".into();
    let mut wrong_owner = physical(3, 30);
    let Owner::Physical(key) = &mut wrong_owner.owner else {
        unreachable!()
    };
    key.source.seat = "seat1".into();
    let mut zero_source = synthetic(1, 1, Some(source(1)));
    zero_source.origin.as_mut().unwrap().generation = 0;
    for bad in [
        wrong_origin,
        wrong_seat,
        wrong_owner,
        zero_source,
        synthetic(0, 1, None),
        synthetic(1, 0, None),
    ] {
        assert!(output.hold(bad, &[29], Duration::from_secs(1)).is_err());
    }
    for codes in [
        vec![],
        vec![0],
        vec![768],
        vec![u16::MAX],
        vec![767],
        vec![29, 29],
        (1..34).collect(),
    ] {
        assert!(
            output
                .hold(synthetic(1, 1, None), &codes, Duration::from_secs(1))
                .is_err()
        );
    }
    assert!(
        output
            .hold(physical(2, 30), &[29, 30], Duration::from_secs(1))
            .is_err()
    );
    assert!(output.hold(physical(2, 30), &[29], Duration::ZERO).is_err());
    assert!(
        output
            .tap(physical(2, 30), &[29], Duration::from_secs(1))
            .is_err()
    );
    assert!(output.down(42));
    assert!(!output.failed());
    assert_eq!(state.borrow().frames.len(), 1);
    output.release(&claim, Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(!output.down(42));
}

#[test]
fn t46_bad_output_identity_and_sequence_exhaustion_are_not_reusable() {
    for source in [
        Source {
            seat: "s".repeat(129),
            ..source(1)
        },
        Source {
            generation: 0,
            ..source(1)
        },
        Source {
            seat: String::new(),
            ..source(1)
        },
        Source {
            seat: "seat\n0".into(),
            ..source(1)
        },
    ] {
        assert!(
            Output::new(TestSink {
                source,
                state: Rc::new(RefCell::new(State::default()))
            })
            .is_err()
        );
    }
    let (mut output, state) = model();
    output.sequence = u64::MAX;
    assert!(output.hold(physical(1, 30), &[42], Duration::ZERO).is_err());
    assert!(output.failed());
    assert_eq!(state.borrow().drops, 1);
    assert!(state.borrow().frames.is_empty());
}

#[test]
fn t46_owner_and_receipt_limits_are_bounded_and_retirement_reclaims_them() {
    let (mut output, state) = model();
    for id in 1..=MAX_OWNERS as u64 {
        output
            .hold(synthetic(id, 1, None), &[42], Duration::ZERO)
            .unwrap();
        drain(&mut output);
    }
    assert!(
        output
            .hold(
                synthetic(MAX_OWNERS as u64 + 1, 1, None),
                &[42],
                Duration::ZERO
            )
            .is_err()
    );
    assert_eq!(state.borrow().frames.len(), 1);
    output.retire_generation(1, Duration::ZERO).unwrap();
    drain(&mut output);
    assert_eq!(state.borrow().frames.len(), 2);
    assert!(!output.down(42));
    output
        .tap(synthetic(1, 2, None), &[29, 30], Duration::ZERO)
        .unwrap();
    drain(&mut output);
}

#[test]
fn t46_source_retirement_registry_is_bounded() {
    let (mut output, state) = model();
    for id in 1..=MAX_SOURCES as u64 {
        output.retire_source(&source(id), Duration::ZERO).unwrap();
    }
    output
        .hold(physical(MAX_SOURCES as u64 + 10, 30), &[42], Duration::ZERO)
        .unwrap();
    drain(&mut output);
    output
        .retire_source(&source(MAX_SOURCES as u64 + 1), Duration::ZERO)
        .unwrap();
    assert!(output.retiring());
    assert!(
        output
            .hold(physical(MAX_SOURCES as u64 + 2, 30), &[42], Duration::ZERO)
            .is_err()
    );
    drain(&mut output);
    assert!(!output.down(42));
    assert_eq!(state.borrow().frames.len(), 2);
    assert_eq!(output.retired_sources.len(), MAX_SOURCES);
}

#[cfg(target_os = "linux")]
#[test]
fn t46_native_virtual_output_implements_the_same_sink_without_opening_a_device() {
    fn check<T: Sink>() {}
    check::<crate::kernel::Virtual>();
}

#[test]
fn t46_native_key_role_refuses_a_physical_counterpart_before_any_io() {
    let generated =
        crate::transport::Descriptor::generated_keyboard(source(0), BTreeMap::new()).unwrap();
    native_key_role(&generated).unwrap();
    let device = crate::transport::tests::touch();
    let mut physical = crate::transport::Descriptor::mirror(
        &device,
        crate::transport::KernelIdentity {
            bus: 3,
            vendor: device.vendor,
            product: device.product,
            version: 1,
        },
        None,
        &BTreeMap::new(),
        BTreeMap::new(),
    )
    .unwrap();
    physical.class = crate::transport::Class::Keyboard;
    assert_eq!(
        native_key_role(&physical).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    let mut changed = generated;
    changed.class = crate::transport::Class::Mouse;
    assert!(native_key_role(&changed).is_err());
}

#[test]
fn t46_cancellation_never_finishes_an_unwritten_chord_tail() {
    for count in 0..=3 {
        let (mut output, state) = model();
        output
            .tap(synthetic(1, 1, None), &[42, 30], Duration::ZERO)
            .unwrap();
        state.borrow_mut().steps.push_back(Step::Prefix(count));
        assert_eq!(output.flush().unwrap(), Progress::Pending);
        output.retire(Duration::ZERO).unwrap();
        drain(&mut output);
        let original = events(&[(42, 1), (30, 1)]);
        let prefix = &original[..count];
        let mut expected = prefix.to_vec();
        if count > 0 {
            expected.extend(
                prefix
                    .iter()
                    .rev()
                    .filter(|event| event.kind == EV_KEY)
                    .map(|event| edge(event.code, false)),
            );
            expected.push(RawEvent {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            });
        }
        assert_eq!(state.borrow().events, expected);
        assert!(!output.down(42) && !output.down(30));
    }
}

#[test]
fn t46_bad_abort_prefix_or_abort_error_stops_without_guessing_releases() {
    for result in [Ok(vec![edge(125, true)]), Err(io::ErrorKind::BrokenPipe)] {
        let (mut output, state) = model();
        output
            .tap(synthetic(1, 1, None), &[42, 30], Duration::ZERO)
            .unwrap();
        state.borrow_mut().abort_override = Some(result);
        assert!(output.retire(Duration::ZERO).is_err());
        assert!(output.failed());
        assert!(output.uncertain_frame().is_some());
        assert_eq!(state.borrow().frames.len(), 1);
        assert_eq!(state.borrow().drops, 1);
        assert!(state.borrow().events.is_empty());
    }
}

#[test]
fn t46_unrelated_pending_owner_survives_another_sources_retirement() {
    let (mut output, state) = model();
    output.hold(physical(1, 30), &[42], Duration::ZERO).unwrap();
    drain(&mut output);
    output.hold(physical(2, 31), &[29], Duration::ZERO).unwrap();
    output.retire_source(&source(1), Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(output.down(29));
    assert!(!output.down(42));
    assert_eq!(state.borrow().frames[1].events, events(&[(29, 1)]));
    assert_eq!(state.borrow().frames[2].events, events(&[(42, 0)]));
}

#[test]
fn t46_cancelling_a_repeat_keeps_only_its_written_prefix_then_releases_the_owner() {
    let (mut output, state) = model();
    let claim = physical(1, 30);
    output.hold(claim.clone(), &[46], Duration::ZERO).unwrap();
    drain(&mut output);
    output.repeat(&claim, Duration::ZERO).unwrap();
    state.borrow_mut().steps.push_back(Step::Prefix(1));
    output.flush().unwrap();
    output.retire_source(&source(1), Duration::ZERO).unwrap();
    drain(&mut output);
    assert_eq!(
        state.borrow().events,
        [events(&[(46, 1)]), events(&[(46, 2)]), events(&[(46, 0)])].concat()
    );
    assert!(!output.down(46));
}

#[test]
fn t46_configuration_retirement_removes_old_remaps_but_keeps_new_owners() {
    let (mut output, state) = model();
    let old = physical(1, 30);
    let mut fresh = physical(2, 31);
    fresh.configuration = 2;
    output.hold(old.clone(), &[42], Duration::ZERO).unwrap();
    drain(&mut output);
    output.hold(fresh, &[42], Duration::ZERO).unwrap();
    assert_eq!(
        output.retire_generation(1, Duration::ZERO).unwrap(),
        Progress::Complete
    );
    assert!(output.down(42));
    assert_eq!(state.borrow().frames.len(), 1);
    assert_eq!(
        output.hold(old, &[42], Duration::ZERO).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    let mut wrong = synthetic(1, 2, None);
    wrong.configuration = 3;
    assert!(output.hold(wrong, &[29], Duration::ZERO).is_err());
    output.retire(Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(!output.down(42));
}

#[test]
fn t46_completed_actions_keep_bounded_receipts_without_disabling_a_long_lived_generation() {
    let (mut output, _) = model();
    for id in 1..=5000 {
        output
            .tap(synthetic(id, 1, None), &[30], Duration::ZERO)
            .unwrap();
        drain(&mut output);
    }
    assert_eq!(output.receipts.len(), 1);
    assert_eq!(
        output
            .tap(synthetic(1, 1, None), &[30], Duration::ZERO)
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(
        output
            .tap(synthetic(4999, 1, None), &[30], Duration::ZERO)
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
}

#[test]
fn t46_release_and_repeat_cannot_cross_a_rebound_configuration_claim() {
    let (mut output, state) = model();
    let old = physical(1, 30);
    output.hold(old.clone(), &[42], Duration::ZERO).unwrap();
    drain(&mut output);
    let mut fresh = old.clone();
    fresh.configuration = 2;
    assert_eq!(
        output.release(&fresh, Duration::ZERO).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    output.release(&old, Duration::ZERO).unwrap();
    drain(&mut output);
    output.hold(fresh.clone(), &[29], Duration::ZERO).unwrap();
    drain(&mut output);
    let count = state.borrow().frames.len();
    assert_eq!(
        output.release(&old, Duration::ZERO).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        output.repeat(&old, Duration::ZERO).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(output.down(29));
    assert_eq!(state.borrow().frames.len(), count);
    output.release(&fresh, Duration::ZERO).unwrap();
    drain(&mut output);
}

#[test]
fn t46_configuration_receipt_count_is_bounded_and_retirement_reclaims_old_generations() {
    let (mut output, _) = model();
    for generation in 1..=MAX_RECEIPT_GENERATIONS as u64 {
        output
            .tap(synthetic(1, generation, None), &[30], Duration::ZERO)
            .unwrap();
        drain(&mut output);
    }
    let next = MAX_RECEIPT_GENERATIONS as u64 + 1;
    assert!(
        output
            .tap(synthetic(1, next, None), &[30], Duration::ZERO)
            .is_err()
    );
    assert_eq!(output.receipts.len(), MAX_RECEIPT_GENERATIONS);
    output.retire_generation(next - 1, Duration::ZERO).unwrap();
    output
        .tap(synthetic(1, next, None), &[30], Duration::ZERO)
        .unwrap();
    drain(&mut output);
    assert_eq!(output.receipts.len(), 1);
}

#[test]
fn t46_old_source_releases_do_not_touch_a_reconnected_remap() {
    let (mut output, state) = model();
    let old = physical(1, 30);
    output.hold(old.clone(), &[42], Duration::ZERO).unwrap();
    drain(&mut output);
    output.retire_source(&source(1), Duration::ZERO).unwrap();
    drain(&mut output);
    let mut fresh = old.clone();
    fresh.origin.as_mut().unwrap().generation = 2;
    fresh.serial += 1;
    let Owner::Physical(key) = &mut fresh.owner else {
        unreachable!()
    };
    key.source.generation = 2;
    output.hold(fresh.clone(), &[29], Duration::ZERO).unwrap();
    drain(&mut output);
    let frames = state.borrow().frames.len();
    output.release(&old, Duration::ZERO).unwrap();
    assert_eq!(state.borrow().frames.len(), frames);
    assert!(output.down(29));
    output.release(&fresh, Duration::ZERO).unwrap();
    drain(&mut output);
}

#[test]
fn t46_retiring_during_a_partial_release_finishes_only_the_remaining_releases() {
    let (mut output, state) = model();
    let claim = synthetic(1, 1, Some(source(1)));
    output
        .hold(claim.clone(), &[42, 30], Duration::ZERO)
        .unwrap();
    drain(&mut output);
    output.release(&claim, Duration::ZERO).unwrap();
    state.borrow_mut().steps.push_back(Step::Prefix(1));
    output.flush().unwrap();
    output.retire_source(&source(1), Duration::ZERO).unwrap();
    drain(&mut output);
    assert_eq!(state.borrow().frames.len(), 2);
    assert_eq!(
        state.borrow().events,
        [events(&[(42, 1), (30, 1)]), events(&[(30, 0), (42, 0)])].concat()
    );
    assert!(!output.down(42) && !output.down(30));
}

#[test]
fn t46_zero_device_identity_remains_valid_with_a_nonzero_incarnation() {
    let state = Rc::new(RefCell::new(State::default()));
    let mut output = Output::new(TestSink {
        source: source(0),
        state: state.clone(),
    })
    .unwrap();
    let claim = physical(0, 30);
    output.hold(claim, &[42], Duration::ZERO).unwrap();
    drain(&mut output);
    output.retire_source(&source(0), Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(!output.down(42));
    assert_eq!(state.borrow().frames.len(), 2);
}

#[test]
fn t46_exact_task_cancellation_discards_every_unwritten_chord_prefix_and_keeps_a_sibling() {
    for prefix in 0..=3 {
        let (mut output, state) = model();
        let sibling = synthetic(1, 1, Some(source(1)));
        let task = synthetic(2, 1, Some(source(1)));
        output.hold(sibling.clone(), &[42], Duration::ZERO).unwrap();
        drain(&mut output);
        output.tap(task.clone(), &[29, 30], Duration::ZERO).unwrap();
        state.borrow_mut().steps.push_back(Step::Prefix(prefix));
        output.flush().unwrap();
        output.cancel(&task, Duration::ZERO).unwrap();
        drain(&mut output);
        let mut expected = events(&[(42, 1)]);
        let submitted = events(&[(29, 1), (30, 1)]);
        expected.extend_from_slice(&submitted[..prefix]);
        if prefix > 0 {
            let ups = submitted[..prefix]
                .iter()
                .rev()
                .filter(|event| event.kind == EV_KEY)
                .map(|event| (event.code, 0))
                .collect::<Vec<_>>();
            expected.extend(events(&ups));
        }
        assert_eq!(state.borrow().events, expected, "prefix {prefix}");
        assert!(output.down(42));
        assert!(!output.down(29) && !output.down(30));
        assert!(!output.retiring());
        assert!(output.cancelled_claims.is_empty());
        assert_eq!(
            output
                .tap(task, &[29, 30], Duration::ZERO)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        output
            .tap(synthetic(3, 1, Some(source(1))), &[30], Duration::ZERO)
            .unwrap();
        drain(&mut output);
        assert!(output.down(42));
        output.cancel(&sibling, Duration::ZERO).unwrap();
        drain(&mut output);
        assert!(!output.down(42));
    }
}

#[test]
fn t46_task_cancellation_releases_only_its_overlapping_remap_and_allows_a_new_press() {
    let (mut output, state) = model();
    let a = physical(1, 30);
    let b = physical(1, 31);
    output.hold(a.clone(), &[42], Duration::ZERO).unwrap();
    drain(&mut output);
    output.hold(b.clone(), &[42], Duration::ZERO).unwrap();
    assert_eq!(
        output.cancel(&a, Duration::ZERO).unwrap(),
        Progress::Complete
    );
    assert!(output.down(42));
    assert!(output.cancelled_claims.is_empty());
    assert_eq!(state.borrow().frames.len(), 1);
    output.cancel(&b, Duration::ZERO).unwrap();
    drain(&mut output);
    let mut fresh = a.clone();
    fresh.serial = 32;
    output.hold(fresh.clone(), &[29], Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(output.down(29));
    assert_eq!(
        output.cancel(&a, Duration::ZERO).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    output.cancel(&fresh, Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(output.into_sink().is_ok());
}

#[test]
fn t46_stale_cancellation_cannot_abort_a_rebound_pending_press() {
    let (mut output, state) = model();
    let old = physical(1, 30);
    output.hold(old.clone(), &[42], Duration::ZERO).unwrap();
    drain(&mut output);
    output.release(&old, Duration::ZERO).unwrap();
    drain(&mut output);
    let mut fresh = old.clone();
    fresh.serial += 1;
    output.hold(fresh.clone(), &[29], Duration::ZERO).unwrap();
    state.borrow_mut().steps.push_back(Step::Prefix(1));
    output.flush().unwrap();
    let frames = state.borrow().frames.len();
    for mut stale in [old.clone(), fresh.clone()] {
        if stale.serial == fresh.serial {
            stale.configuration += 1;
        }
        assert_eq!(
            output.cancel(&stale, Duration::ZERO).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(state.borrow().pending.is_some());
        assert_eq!(state.borrow().frames.len(), frames);
    }
    assert!(output.cancelled_claims.is_empty());
    drain(&mut output);
    assert!(output.down(29));
    output.cancel(&fresh, Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(!output.down(29));
}

#[test]
fn t46_cancel_all_compensates_a_partial_add_then_releases_all_current_owners() {
    let (mut output, state) = model();
    output
        .hold(synthetic(1, 1, None), &[42, 29], Duration::from_secs(1))
        .unwrap();
    drain(&mut output);
    let old = physical(1, 30);
    output
        .hold(old.clone(), &[42], Duration::from_secs(1))
        .unwrap();
    output
        .hold(
            synthetic(31, 1, Some(source(1))),
            &[29, 30],
            Duration::from_secs(1),
        )
        .unwrap();
    state.borrow_mut().steps.push_back(Step::Prefix(1));
    output.flush().unwrap();
    output.cancel_all(Duration::ZERO).unwrap();
    assert!(!output.settled());
    drain(&mut output);
    assert_eq!(state.borrow().frames[2].events, events(&[(30, 0)]));
    assert_eq!(state.borrow().frames[3].events, events(&[(29, 0), (42, 0)]));
    assert_eq!(state.borrow().frames[3].at, Duration::from_secs(1));
    assert!(output.held.is_empty() && output.cancelled_claims.is_empty());
    assert!(output.logical.counts.is_empty() && output.retired_sources.is_empty());
    assert_eq!(output.retired_generation, 0);
    assert!(!output.retiring());
    let mut fresh = old.clone();
    fresh.serial = 32;
    output
        .hold(fresh.clone(), &[42], Duration::from_secs(1))
        .unwrap();
    drain(&mut output);
    assert_eq!(
        output.release(&old, Duration::ZERO).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    output.cancel_all(Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(output.into_sink().is_ok());
}

#[test]
fn t46_cancel_all_during_a_partial_release_finishes_each_up_once() {
    let (mut output, state) = model();
    let claim = synthetic(1, 1, None);
    output
        .hold(claim.clone(), &[42, 29], Duration::ZERO)
        .unwrap();
    drain(&mut output);
    output.release(&claim, Duration::ZERO).unwrap();
    state.borrow_mut().steps.push_back(Step::Prefix(1));
    output.flush().unwrap();
    output.cancel_all(Duration::ZERO).unwrap();
    drain(&mut output);
    assert_eq!(
        state.borrow().events,
        [events(&[(42, 1), (29, 1)]), events(&[(29, 0), (42, 0)])].concat()
    );
    assert_eq!(state.borrow().frames.len(), 2);
    assert!(output.cancelled_claims.is_empty() && output.held.is_empty());
    assert!(!output.retiring());
}

#[test]
fn t46_task_cancellation_during_repeat_does_not_retire_the_physical_source() {
    for prefix in 0..=2 {
        let (mut output, state) = model();
        let old = physical(1, 30);
        output.hold(old.clone(), &[42], Duration::ZERO).unwrap();
        drain(&mut output);
        output.repeat(&old, Duration::ZERO).unwrap();
        state.borrow_mut().steps.push_back(Step::Prefix(prefix));
        output.flush().unwrap();
        output.cancel(&old, Duration::ZERO).unwrap();
        drain(&mut output);
        assert_eq!(
            state
                .borrow()
                .events
                .iter()
                .filter(|event| event.kind == EV_KEY && event.value == 2)
                .count(),
            usize::from(prefix > 0)
        );
        assert_eq!(
            state
                .borrow()
                .events
                .iter()
                .filter(|event| event.kind == EV_KEY && event.value == 0)
                .count(),
            1
        );
        assert!(!output.down(42));
        let mut fresh = old;
        fresh.serial += 1;
        output.hold(fresh, &[42], Duration::ZERO).unwrap();
        drain(&mut output);
        assert!(output.down(42));
        assert!(output.cancelled_claims.is_empty() && output.retired_sources.is_empty());
    }
}

#[test]
fn t46_cancelling_a_sibling_preserves_an_unrelated_pending_claim_on_the_same_source() {
    let (mut output, state) = model();
    let old = synthetic(1, 1, Some(source(1)));
    let pending = synthetic(2, 1, Some(source(1)));
    output.hold(old.clone(), &[42], Duration::ZERO).unwrap();
    drain(&mut output);
    output.hold(pending.clone(), &[29], Duration::ZERO).unwrap();
    state.borrow_mut().steps.push_back(Step::Prefix(1));
    output.flush().unwrap();
    assert_eq!(
        output.cancel(&old, Duration::ZERO).unwrap(),
        Progress::Pending
    );
    assert!(state.borrow().pending.is_some());
    drain(&mut output);
    assert!(!output.down(42) && output.down(29));
    assert_eq!(state.borrow().frames.len(), 3);
    assert_eq!(
        state.borrow().events,
        [events(&[(42, 1)]), events(&[(29, 1)]), events(&[(42, 0)])].concat()
    );
    output.cancel(&pending, Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(output.cancelled_claims.is_empty());
}

#[test]
fn t46_unknown_task_cancellations_do_not_allocate_receipts_or_tracking() {
    let (mut output, state) = model();
    for serial in 1..=5000 {
        assert_eq!(
            output
                .cancel(&synthetic(serial, 1, None), Duration::ZERO)
                .unwrap(),
            Progress::Complete
        );
        assert!(output.cancelled_claims.is_empty() && output.receipts.is_empty());
    }
    assert!(state.borrow().frames.is_empty());
}

#[test]
fn t46_press_serials_fence_delayed_release_repeat_cancel_and_hold_replay() {
    let (mut output, state) = model();
    let old = physical(1, 30);
    output.hold(old.clone(), &[42], Duration::ZERO).unwrap();
    drain(&mut output);
    output.release(&old, Duration::ZERO).unwrap();
    drain(&mut output);
    let mut fresh = old.clone();
    fresh.serial += 1;
    output.hold(fresh.clone(), &[42], Duration::ZERO).unwrap();
    drain(&mut output);
    let frames = state.borrow().frames.len();
    assert_eq!(
        output.release(&old, Duration::ZERO).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        output.repeat(&old, Duration::ZERO).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        output.cancel(&old, Duration::ZERO).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(output.hold(old.clone(), &[42], Duration::ZERO).is_err());
    assert!(output.down(42));
    assert_eq!(state.borrow().frames.len(), frames);
    output.release(&fresh, Duration::ZERO).unwrap();
    drain(&mut output);
    assert_eq!(
        output.hold(old, &[42], Duration::ZERO).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(output.receipts.len(), 1);
}

#[test]
fn t46_physical_press_receipts_are_bounded_across_actions_and_configurations() {
    let (mut output, _) = model();
    let mut claim = physical(1, 30);
    for serial in 1..=5000 {
        claim.serial = serial;
        output.hold(claim.clone(), &[42], Duration::ZERO).unwrap();
        drain(&mut output);
        output.cancel(&claim, Duration::ZERO).unwrap();
        drain(&mut output);
        assert_eq!(output.receipts.len(), 1);
        assert!(output.cancelled_claims.is_empty());
    }
    for configuration in 2..=MAX_RECEIPT_GENERATIONS as u64 {
        claim.configuration = configuration;
        claim.serial = 1;
        output.hold(claim.clone(), &[42], Duration::ZERO).unwrap();
        drain(&mut output);
        output.cancel_all(Duration::ZERO).unwrap();
        drain(&mut output);
    }
    claim.configuration += 1;
    assert!(output.hold(claim.clone(), &[42], Duration::ZERO).is_err());
    output.retire_generation(1, Duration::ZERO).unwrap();
    output.hold(claim, &[42], Duration::ZERO).unwrap();
    drain(&mut output);
    assert_eq!(output.receipts.len(), MAX_RECEIPT_GENERATIONS);
    assert!(output.down(42));
}
