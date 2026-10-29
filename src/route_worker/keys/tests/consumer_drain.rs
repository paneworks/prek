use super::*;

fn closing() -> (Slot<ModelPreparation>, Rc<RefCell<State>>) {
    let (mut slot, state) = model();
    activate(&mut slot, &state, 0);
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    submit(&mut slot, &mut routing, 1, 10, 1);
    drain(&mut slot, &mut routing, 10);
    slot.retire(ms(20)).unwrap();
    (slot, state)
}

#[test]
fn no_request_or_acknowledgement_precedes_the_final_local_release() {
    let (mut slot, state) = closing();
    state.borrow_mut().blocked = true;
    slot.advance(ms(21)).unwrap();
    assert!(slot.drain_request().is_none());
    let (mut other, _) = closing();
    other.advance(ms(21)).unwrap();
    let foreign = other.drain_request().unwrap();
    assert!(!slot.acknowledge_drain(&foreign, ms(22), ms(22)));
    assert_eq!(state.borrow().dropped, 0);
    state.borrow_mut().blocked = false;
    slot.advance(ms(23)).unwrap();
    assert_eq!(keys(&state), [(42, 1), (42, 0)]);
    let unoffered = slot.drain.clone().unwrap();
    assert!(!slot.acknowledge_drain(&unoffered, ms(23), ms(23)));
    let request = slot.drain_request().unwrap();
    assert_eq!(request.requested_at, ms(23));
    assert!(slot.acknowledge_drain(&request, ms(24), ms(24)));
}

#[test]
fn every_challenge_identity_field_is_exact_without_poisoning_the_current_slot() {
    let (mut slot, state) = closing();
    slot.advance(ms(21)).unwrap();
    let request = slot.drain_request().unwrap();
    for field in 0..12 {
        let mut wrong = request.clone();
        match field {
            0 => wrong.fence.broker.0[0] ^= 1,
            1 => wrong.fence.configuration += 1,
            2 => wrong.fence.session += 1,
            3 => wrong.source.device += 1,
            4 => wrong.source.generation += 1,
            5 => wrong.source.seat.push('x'),
            6 => wrong.marker.push('x'),
            7 => wrong.consumer = Consumer::IsolatedReceiver { pid: 2, start: 1 },
            8 => wrong.consumer = Consumer::IsolatedReceiver { pid: 1, start: 2 },
            9 => wrong.nonce.0[0] ^= 1,
            10 => wrong.requested_at += ms(1),
            11 => wrong.deadline += ms(1),
            _ => unreachable!(),
        }
        assert!(
            !slot.acknowledge_drain(&wrong, ms(22), ms(22)),
            "field {field}"
        );
        assert!(!slot.failed);
        assert_eq!(state.borrow().dropped, 0);
    }
    assert!(slot.acknowledge_drain(&request, ms(22), ms(22)));
    assert_eq!(state.borrow().dropped, 1);
}

#[test]
fn acknowledgement_requires_a_post_quiescence_nonfuture_sample_before_the_deadline() {
    let (mut slot, state) = closing();
    slot.advance(ms(21)).unwrap();
    let request = slot.drain_request().unwrap();
    assert!(!slot.acknowledge_drain(&request, ms(20), ms(22)));
    assert!(!slot.acknowledge_drain(&request, ms(23), ms(22)));
    assert!(!slot.acknowledge_drain(&request, ms(270), ms(270)));
    assert_eq!(state.borrow().dropped, 0);
    assert!(slot.acknowledge_drain(&request, ms(22), ms(22)));
    assert!(!slot.acknowledge_drain(&request, ms(22), ms(22)));
}

#[test]
fn an_unacknowledged_drain_removes_the_output_at_the_shared_retirement_deadline() {
    let (mut slot, state) = closing();
    state.borrow_mut().blocked = true;
    slot.advance(ms(21)).unwrap();
    slot.retire(ms(260)).unwrap();
    state.borrow_mut().blocked = false;
    slot.advance(ms(269)).unwrap();
    assert_eq!(slot.deadline(), Some(ms(270)));
    let request = slot.drain_request().unwrap();
    assert_eq!(request.deadline, ms(270));
    assert!(!slot.closed());
    assert_eq!(state.borrow().dropped, 0);
    slot.advance(ms(270)).unwrap();
    assert!(slot.closed());
    assert!(
        slot.take_notice()
            .is_some_and(|notice| notice.contains("removed after writing"))
    );
    assert!(!slot.acknowledge_drain(&request, ms(270), ms(270)));
    assert_eq!(keys(&state), [(42, 1), (42, 0)]);
    assert_eq!(state.borrow().dropped, 1);
}

#[test]
fn local_neutral_does_not_release_any_same_seat_raw_barrier() {
    let (mut slot, _) = closing();
    slot.advance(ms(21)).unwrap();
    let routing = routing(vec![]);
    let same = source(2);
    let mut other = same.clone();
    other.seat = "seat1".into();
    assert!(slot.blocks_input(&routing, &same, 99, None));
    assert!(!slot.blocks_input(&routing, &other, 99, None));
    let request = slot.drain_request().unwrap();
    assert!(slot.acknowledge_drain(&request, ms(22), ms(22)));
    assert!(!slot.blocks_input(&routing, &same, 99, None));
}

#[test]
fn even_a_reused_publication_cannot_accept_an_older_slot_challenge() {
    let (mut old, _) = closing();
    old.advance(ms(21)).unwrap();
    let old_request = old.drain_request().unwrap();
    let (mut new, state) = closing();
    new.advance(ms(21)).unwrap();
    let new_request = new.drain_request().unwrap();
    assert_ne!(old_request.nonce, new_request.nonce);
    assert!(!new.acknowledge_drain(&old_request, ms(22), ms(22)));
    assert_eq!(state.borrow().dropped, 0);
    assert!(new.acknowledge_drain(&new_request, ms(22), ms(22)));
}

#[test]
fn preparation_without_injection_closes_without_a_consumer_drain() {
    let (mut slot, state) = model();
    slot.retire(ms(1)).unwrap();
    assert!(slot.closed());
    assert!(slot.drain_request().is_none());
    assert_eq!(state.borrow().dropped, 1);
    assert!(state.borrow().events.is_empty());
}

#[test]
fn inventory_freshness_cannot_substitute_for_consumer_drain() {
    let (mut slot, state) = closing();
    slot.advance(ms(21)).unwrap();
    let request = slot.drain_request().unwrap();
    let fresh = evidence(&slot, 22);
    assert!(
        !slot
            .observe(
                request.fence,
                &request.source,
                &request.marker,
                &fresh,
                ms(22)
            )
            .unwrap()
    );
    assert!(!slot.closed());
    assert_eq!(state.borrow().dropped, 0);
    assert!(slot.acknowledge_drain(&request, ms(23), ms(23)));
}

#[test]
fn private_drain_packets_require_the_exact_challenge_schema() {
    let (mut slot, _) = closing();
    slot.advance(ms(21)).unwrap();
    let request = slot.drain_request().unwrap();
    let command = crate::configuration::Command::KeyDrained {
        request: request.clone(),
        at: ms(22),
    };
    let encoded = serde_json::to_value(&command).unwrap();
    assert!(serde_json::from_value::<crate::configuration::Command>(encoded.clone()).is_ok());
    for key in encoded["request"].as_object().unwrap().keys() {
        let mut missing = encoded.clone();
        missing["request"].as_object_mut().unwrap().remove(key);
        assert!(serde_json::from_value::<crate::configuration::Command>(missing).is_err());
    }
    let mut unknown = encoded;
    unknown["request"]["inventory_is_neutral"] = serde_json::json!(true);
    assert!(serde_json::from_value::<crate::configuration::Command>(unknown).is_err());
    let mut message = serde_json::to_value(crate::worker::Message::KeyDrain { request }).unwrap();
    message["request"]["extra"] = serde_json::json!(true);
    assert!(serde_json::from_value::<crate::worker::Message>(message).is_err());
}
