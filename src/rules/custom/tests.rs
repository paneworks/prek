use super::*;
use crate::protocol::Epoch;
use std::sync::Arc;

fn state() -> State {
    State::new(
        &Config::preset("minimal").unwrap(),
        Some(Fence {
            broker: Epoch([1, 2]),
            configuration: 1,
            session: 1,
        }),
    )
    .unwrap()
    .unwrap()
}
fn input(code: u16) -> Input {
    Input::Frame {
        source: crate::keys::worker::tests::source(1),
        at: Duration::from_millis(10),
        sequence: 1,
        selectors: vec!["keys".into()],
        changes: vec![Change::Key {
            code,
            state: KeyState::Press,
        }],
        contacts: vec![],
        contacts_started: vec![],
        contacts_ended: vec![],
        ended_contacts: vec![],
    }
}
fn keys(event: usize) -> InputProof {
    let mut keys = presses::Set::empty(Arc::new(presses::Budget::default()));
    keys.insert(presses::Press {
        source: crate::keys::worker::tests::source(1),
        position: InputPosition { frame: 1, event },
        code: 30,
    })
    .unwrap();
    InputProof {
        tails: keys.related(),
        key_presses: keys,
    }
}
fn assert_bytes(state: &State) {
    let mut total = 0;
    for offer in &state.offers {
        let bytes = match &offer.input {
            OfferedInput::Frame(input) | OfferedInput::KeyFrame(input) => {
                recognition::encoded_size(input, recognition::MAX_FRAME_BYTES - 1024).unwrap()
                    + PROOF_BYTES
            }
            _ => panic!("expected frame offer"),
        };
        assert_eq!(offer.bytes, bytes);
        total += bytes;
    }
    assert_eq!(state.bytes, total);
}

#[test]
fn interleaved_key_atoms_charge_metadata_once_and_keep_the_latest_original_proof() {
    let mut state = state();
    for event in 0..32 {
        for candidate in [1, 2] {
            state
                .offer(
                    candidate,
                    keys(event),
                    OfferedInput::KeyFrame(input(30)),
                    Duration::from_millis(10 + event as u64),
                )
                .unwrap();
        }
        assert_bytes(&state);
    }
    assert_eq!(state.offers.len(), 2);
    for offer in &state.offers {
        let OfferedInput::KeyFrame(Input::Frame { changes, .. }) = &offer.input else {
            panic!("missing coalesced key frame")
        };
        assert_eq!(changes.len(), 32);
        assert_eq!(offer.at, Duration::from_millis(10));
        assert_eq!(
            offer
                .proof
                .key_presses
                .iter()
                .next()
                .unwrap()
                .position
                .event,
            31
        );
    }
}

#[test]
fn coalescing_never_crosses_an_original_frame_identity_or_a_full_frame_offer() {
    let mut state = state();
    state
        .offer(
            1,
            keys(0),
            OfferedInput::KeyFrame(input(30)),
            Duration::ZERO,
        )
        .unwrap();
    for field in 0..5 {
        let mut changed = input(31);
        let Input::Frame {
            source,
            sequence,
            at,
            selectors,
            ..
        } = &mut changed
        else {
            unreachable!()
        };
        match field {
            0 => source.device += 1,
            1 => source.generation += 1,
            2 => *sequence += 1,
            3 => *at += Duration::from_nanos(1),
            4 => selectors.push("other".into()),
            _ => unreachable!(),
        }
        state
            .offer(1, keys(1), OfferedInput::KeyFrame(changed), Duration::ZERO)
            .unwrap();
    }
    state
        .offer(
            2,
            keys(2),
            OfferedInput::KeyFrame(input(30)),
            Duration::ZERO,
        )
        .unwrap();
    state
        .offer(1, keys(3), OfferedInput::Frame(input(30)), Duration::ZERO)
        .unwrap();
    assert_eq!(state.offers.len(), 8);
    assert_bytes(&state);
    assert_eq!(
        state.offers[0]
            .proof
            .key_presses
            .iter()
            .next()
            .unwrap()
            .position
            .event,
        0
    );
}

#[test]
fn a_full_offer_queue_can_merge_an_existing_frame_but_cannot_add_another_candidate() {
    let mut state = state();
    for candidate in 1..=recognition::MAX_PENDING as u64 {
        state
            .offer(
                candidate,
                keys(0),
                OfferedInput::KeyFrame(input(30)),
                Duration::ZERO,
            )
            .unwrap();
    }
    state
        .offer(
            1,
            keys(1),
            OfferedInput::KeyFrame(input(31)),
            Duration::ZERO,
        )
        .unwrap();
    let before = state.bytes;
    assert!(
        state
            .offer(
                65,
                keys(2),
                OfferedInput::KeyFrame(input(32)),
                Duration::ZERO
            )
            .is_err()
    );
    assert_eq!(state.offers.len(), recognition::MAX_PENDING);
    assert_eq!(state.bytes, before);
    assert_eq!(
        state.offers[0]
            .proof
            .key_presses
            .iter()
            .next()
            .unwrap()
            .position
            .event,
        1
    );
    assert_bytes(&state);
}

#[test]
fn merged_frame_byte_exhaustion_does_not_change_its_input_or_proof() {
    let mut state = state();
    let mut accepted = 0;
    for event in 0..4096 {
        let before = state.bytes;
        if state
            .offer(
                1,
                keys(event),
                OfferedInput::KeyFrame(input(30)),
                Duration::ZERO,
            )
            .is_err()
        {
            assert_eq!(state.bytes, before);
            break;
        }
        accepted += 1;
    }
    assert!(accepted > 1 && accepted < 4096);
    assert_eq!(state.offers.len(), 1);
    let OfferedInput::KeyFrame(Input::Frame { changes, .. }) = &state.offers[0].input else {
        unreachable!()
    };
    assert_eq!(changes.len(), accepted);
    assert_eq!(
        state.offers[0]
            .proof
            .key_presses
            .iter()
            .next()
            .unwrap()
            .position
            .event,
        accepted - 1
    );
    assert_bytes(&state);
}

#[test]
fn aggregate_byte_exhaustion_is_atomic_for_both_new_frames_and_merged_atoms() {
    let mut state = state();
    let mut large = input(30);
    let Input::Frame { selectors, .. } = &mut large else {
        unreachable!()
    };
    *selectors = vec!["x".repeat(60_000)];
    for candidate in 1..=recognition::MAX_PENDING as u64 {
        let before = state.bytes;
        if state
            .offer(
                candidate,
                keys(0),
                OfferedInput::KeyFrame(large.clone()),
                Duration::ZERO,
            )
            .is_err()
        {
            assert_eq!(state.bytes, before);
            break;
        }
    }
    assert!(state.offers.len() < recognition::MAX_PENDING);
    let mut accepted = 0;
    let mut chunk = input(31);
    let Input::Frame { changes, .. } = &mut chunk else {
        unreachable!()
    };
    *changes = vec![
        Change::Key {
            code: 31,
            state: KeyState::Press
        };
        10
    ];
    loop {
        let before = state.bytes;
        if state
            .offer(
                100,
                keys(accepted),
                OfferedInput::KeyFrame(chunk.clone()),
                Duration::ZERO,
            )
            .is_err()
        {
            assert_eq!(state.bytes, before);
            break;
        }
        accepted += 1;
        assert!(accepted < 4096);
    }
    assert!(accepted > 1);
    let last = state.offers.back().unwrap();
    assert_eq!(
        last.proof.key_presses.iter().next().unwrap().position.event,
        accepted - 1
    );
    assert!(last.bytes < recognition::MAX_FRAME_BYTES - 1024);
    let Input::Frame { changes, .. } = &chunk else {
        unreachable!()
    };
    let additional = recognition::encoded_size(changes, recognition::MAX_FRAME_BYTES).unwrap() - 1;
    assert!(last.bytes + additional <= recognition::MAX_FRAME_BYTES - 1024 + PROOF_BYTES);
    assert!(state.bytes + additional > recognition::MAX_BYTES);
    assert_bytes(&state);
}
