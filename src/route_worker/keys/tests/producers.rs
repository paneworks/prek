use super::*;
use crate::keys::producer::{Operation as Op, Request};

fn request(slot: &Slot<ModelPreparation>, sequence: u64, operation: Op) -> Request {
    Request {
        fence: fence(),
        source: slot.source().clone(),
        marker: slot.descriptor.marker.clone(),
        consumer: slot.consumer.clone(),
        producer: Epoch([7, 1]),
        sequence,
        operation,
    }
}
fn setup() -> (Slot<ModelPreparation>, Rc<RefCell<State>>, Routing) {
    let (mut slot, state) = model();
    activate(&mut slot, &state, 0);
    let routing = crate::keys::worker::tests::projected(vec![]);
    (slot, state, routing)
}

#[test]
fn every_output_scope_field_is_exact_before_software_admission() {
    let (mut slot, state, mut routing) = setup();
    let valid = request(&slot, 1, Op::Open { expires: ms(200) });
    for field in 0..9 {
        let mut wrong = valid.clone();
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
            _ => unreachable!(),
        }
        assert!(
            !slot.producer_request(&routing, &wrong, ms(0)).unwrap(),
            "field {field}"
        );
        assert!(slot.available(ms(0)));
    }
    assert!(slot.producer_request(&routing, &valid, ms(0)).unwrap());
    let press = request(&slot, 2, Op::Press { id: 1, key: 42 });
    assert!(slot.producer_request(&routing, &press, ms(10)).unwrap());
    drain(&mut slot, &mut routing, 10);
    assert_eq!(keys(&state), [(42, 1)]);
}

#[test]
fn software_renewal_cannot_outlive_consumer_readiness_or_revive_an_expired_lease() {
    let (mut slot, state, mut routing) = setup();
    crate::keys::worker::tests::context(&mut routing, 100, 2, crate::context::State::Desktop);
    let open = request(&slot, 1, Op::Open { expires: ms(300) });
    assert!(!slot.producer_request(&routing, &open, ms(100)).unwrap());
    observe(&mut slot, 100);
    assert!(slot.producer_request(&routing, &open, ms(100)).unwrap());
    let close = request(&slot, 2, Op::Close {});
    assert!(slot.producer_request(&routing, &close, ms(101)).unwrap());
    let mut fresh = open;
    fresh.producer = Epoch([8, 1]);
    fresh.operation = Op::Open { expires: ms(120) };
    assert!(slot.producer_request(&routing, &fresh, ms(101)).unwrap());
    observe(&mut slot, 121);
    let mut renew = fresh;
    renew.sequence = 2;
    renew.operation = Op::Renew { expires: ms(200) };
    assert!(!slot.producer_request(&routing, &renew, ms(121)).unwrap());
    drain(&mut slot, &mut routing, 121);
    assert!(keys(&state).is_empty());
}

#[test]
fn software_retirement_still_retains_the_writer_until_consumer_drain_acknowledgement() {
    let (mut slot, state, mut routing) = setup();
    let open = request(&slot, 1, Op::Open { expires: ms(200) });
    assert!(slot.producer_request(&routing, &open, ms(0)).unwrap());
    let press = request(&slot, 2, Op::Press { id: 1, key: 42 });
    assert!(slot.producer_request(&routing, &press, ms(10)).unwrap());
    drain(&mut slot, &mut routing, 10);
    slot.retire(ms(20)).unwrap();
    slot.advance(ms(21)).unwrap();
    assert_eq!(keys(&state), [(42, 1), (42, 0)]);
    assert_eq!(state.borrow().dropped, 0);
    assert!(!slot.producer_request(&routing, &press, ms(21)).unwrap());
    let receipt = slot.drain_request().unwrap();
    assert!(slot.acknowledge_drain(&receipt, ms(22), ms(22)));
    assert_eq!(state.borrow().dropped, 1);
}

#[test]
fn private_software_packets_reject_unknown_and_missing_fields_including_empty_close() {
    let (slot, _, _) = setup();
    let request = request(&slot, 1, Op::Close {});
    let value = serde_json::to_value(crate::configuration::Command::Software {
        request: Box::new(request),
    })
    .unwrap();
    assert!(serde_json::from_value::<crate::configuration::Command>(value.clone()).is_ok());
    for key in value["request"].as_object().unwrap().keys() {
        let mut missing = value.clone();
        missing["request"].as_object_mut().unwrap().remove(key);
        assert!(serde_json::from_value::<crate::configuration::Command>(missing).is_err());
    }
    for container in ["request", "operation"] {
        let mut unknown = value.clone();
        if container == "request" {
            unknown["request"]["physical_origin"] = serde_json::json!(1);
        } else {
            unknown["request"]["operation"]["extra"] = serde_json::json!(1);
        }
        assert!(serde_json::from_value::<crate::configuration::Command>(unknown).is_err());
    }
}
