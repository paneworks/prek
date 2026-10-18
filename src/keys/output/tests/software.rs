use super::*;

fn software(id: u64, generation: u64, producer: u64) -> Claim {
    Claim {
        owner: Owner::Software {
            seat: "seat0".into(),
            producer: [producer, 1],
            generation,
            id,
        },
        origin: None,
        configuration: generation,
        serial: id,
    }
}
fn keys(state: &Rc<RefCell<State>>) -> Vec<(u16, i32)> {
    state
        .borrow()
        .events
        .iter()
        .filter(|event| event.kind == EV_KEY)
        .map(|event| (event.code, event.value))
        .collect()
}

#[test]
fn explicit_held_repeat_never_needs_a_fabricated_physical_source() {
    let (mut output, state) = model();
    let claim = software(1, 1, 7);
    output.hold(claim.clone(), &[30], Duration::ZERO).unwrap();
    drain(&mut output);
    for _ in 0..2 {
        output.repeat(&claim, Duration::ZERO).unwrap();
        drain(&mut output);
    }
    output.release(&claim, Duration::ZERO).unwrap();
    drain(&mut output);
    assert_eq!(keys(&state), [(30, 1), (30, 2), (30, 2), (30, 0)]);
    assert!(!output.down(30));
    assert_eq!(
        output.repeat(&claim, Duration::ZERO).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn retiring_one_producer_preserves_software_siblings_and_remap_modifiers() {
    let (mut output, state) = model();
    let physical = remap(1, 1, 1);
    let one = software(2, 1, 7);
    let two = software(3, 1, 8);
    for claim in [&physical, &one, &two] {
        output.hold(claim.clone(), &[42], Duration::ZERO).unwrap();
        drain(&mut output);
    }
    output.retire_producer([7, 1], Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(output.down(42));
    output.repeat(&two, Duration::ZERO).unwrap();
    drain(&mut output);
    output.retire_producer([8, 1], Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(output.down(42));
    output.release(&physical, Duration::ZERO).unwrap();
    drain(&mut output);
    assert_eq!(keys(&state), [(42, 1), (42, 2), (42, 0)]);
}

#[test]
fn software_claims_reject_physical_origins_empty_producers_and_ambiguous_chords() {
    let (mut output, state) = model();
    let valid = software(1, 1, 7);
    let mut forged = valid.clone();
    forged.origin = Some(source(1));
    assert!(output.hold(forged, &[30], Duration::ZERO).is_err());
    let mut empty = valid.clone();
    if let Owner::Software { producer, .. } = &mut empty.owner {
        *producer = [0, 0];
    }
    assert!(output.hold(empty, &[30], Duration::ZERO).is_err());
    let mut wrong = valid.clone();
    wrong.serial += 1;
    assert!(output.hold(wrong, &[30], Duration::ZERO).is_err());
    assert!(
        output
            .hold(valid.clone(), &[42, 30], Duration::ZERO)
            .is_err()
    );
    assert!(output.tap(valid.clone(), &[30], Duration::ZERO).is_err());
    assert!(state.borrow().events.is_empty());
    output.hold(valid, &[30], Duration::ZERO).unwrap();
    drain(&mut output);
    assert_eq!(keys(&state), [(30, 1)]);
}

#[test]
fn a_retired_producer_cannot_reappear_in_a_new_configuration() {
    let (mut output, state) = model();
    output.retire_producer([7, 1], Duration::ZERO).unwrap();
    assert_eq!(
        output
            .hold(software(1, 1, 7), &[30], Duration::ZERO)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        output
            .hold(software(2, 2, 7), &[30], Duration::ZERO)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    output
        .hold(software(3, 2, 8), &[30], Duration::ZERO)
        .unwrap();
    drain(&mut output);
    assert_eq!(keys(&state), [(30, 1)]);
}

#[test]
fn producer_revocation_reconciles_written_prefixes_without_new_presses() {
    for prefix in [0, 1] {
        let (mut output, state) = model();
        state.borrow_mut().steps.push_back(Step::Prefix(prefix));
        output
            .hold(software(1, 1, 7), &[30], Duration::ZERO)
            .unwrap();
        assert_eq!(output.flush().unwrap(), Progress::Pending);
        output.retire_producer([7, 1], Duration::ZERO).unwrap();
        drain(&mut output);
        assert_eq!(
            keys(&state),
            if prefix == 0 {
                vec![]
            } else {
                vec![(30, 1), (30, 0)]
            }
        );
        assert!(!output.down(30));
        assert_eq!(output.retired_producers.len(), 1);
    }
}

#[test]
fn physical_source_retirement_does_not_revoke_an_unrelated_software_provider() {
    let (mut output, state) = model();
    let claim = software(1, 1, 7);
    output.hold(claim.clone(), &[30], Duration::ZERO).unwrap();
    drain(&mut output);
    output.retire_source(&source(1), Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(output.down(30));
    output.repeat(&claim, Duration::ZERO).unwrap();
    drain(&mut output);
    output.retire_generation(1, Duration::ZERO).unwrap();
    drain(&mut output);
    assert_eq!(keys(&state), [(30, 1), (30, 2), (30, 0)]);
}

#[test]
fn exhausted_producer_tombstones_seal_instead_of_recycling_identity() {
    let (mut output, state) = model();
    output
        .hold(software(1, 1, 10000), &[30], Duration::ZERO)
        .unwrap();
    drain(&mut output);
    assert!(output.retire_producer([0, 0], Duration::ZERO).is_err());
    for id in 1..=MAX_PRODUCERS {
        output
            .retire_producer([id as u64, 1], Duration::ZERO)
            .unwrap();
    }
    output.retire_producer([20000, 1], Duration::ZERO).unwrap();
    drain(&mut output);
    assert!(output.retiring());
    assert_eq!(output.retired_producers.len(), MAX_PRODUCERS);
    assert_eq!(keys(&state), [(30, 1), (30, 0)]);
}

#[test]
fn a_partial_software_repeat_is_not_replayed_when_its_producer_is_revoked() {
    let (mut output, state) = model();
    let claim = software(1, 1, 7);
    output.hold(claim.clone(), &[30], Duration::ZERO).unwrap();
    drain(&mut output);
    state.borrow_mut().steps.push_back(Step::Prefix(1));
    output.repeat(&claim, Duration::ZERO).unwrap();
    assert_eq!(output.flush().unwrap(), Progress::Pending);
    output.retire_producer([7, 1], Duration::ZERO).unwrap();
    drain(&mut output);
    assert_eq!(keys(&state), [(30, 1), (30, 2), (30, 0)]);
    assert!(output.repeat(&claim, Duration::ZERO).is_err());
    assert_eq!(keys(&state), [(30, 1), (30, 2), (30, 0)]);
}

#[test]
fn logical_generation_cleanup_keeps_other_providers_and_physical_keys_distinct() {
    let mut logical = crate::keys::LogicalKeys::default();
    let first = software(1, 1, 7).owner;
    let second = software(2, 2, 8).owner;
    let physical = Owner::Physical(SourceKey {
        source: source(1),
        code: 42,
    });
    logical.update(first, 42, true).unwrap();
    logical.update(second, 30, true).unwrap();
    logical.update(physical, 42, true).unwrap();
    assert!(logical.cancel_generation("seat0", 1).is_empty());
    assert!(logical.down("seat0", 42));
    assert!(logical.down("seat0", 30));
    let released = logical.cancel_source(&source(1));
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].code, 42);
    assert!(!logical.down("seat0", 42));
    assert!(logical.down("seat0", 30));
    assert_eq!(logical.cancel_generation("seat0", 2)[0].code, 30);
    assert!(!logical.down("seat0", 30));
}
