use super::*;
use crate::{ipc::Channel, protocol::Epoch};

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}
fn worker() -> WorkerIdentity {
    // Explicit process-identity model; not a kernel ownership claim.
    serde_json::from_value(serde_json::json!({"pid":std::process::id(),"birth":1,"epoch":1}))
        .unwrap()
}
fn fence() -> Fence {
    Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    }
}
fn source() -> Source {
    Source {
        device: 1,
        generation: 1,
        seat: "seat0".into(),
    }
}
fn owner() -> Owner {
    Owner {
        output: "DSI-1".into(),
        epoch: "a".repeat(32),
        ui_generation: 2,
        ownership_generation: 3,
    }
}
fn admission(expires: Option<Duration>) -> Option<Admission> {
    Some(Admission {
        owners: vec![owner()],
        expires,
    })
}
fn gate() -> Gate {
    Gate::new(worker(), fence(), source())
}
fn ready() -> (Gate, Activation) {
    let mut gate = gate();
    gate.update(admission(None), ms(0)).unwrap();
    let request = gate.request(ms(1)).unwrap().unwrap();
    (gate, request)
}
#[test]
fn consumer_evidence_and_native_lease_cannot_replace_neutral_admission() {
    let mut gate = gate();
    assert!(!gate.allowed(ms(0)));
    gate.update(admission(Some(ms(200))), ms(0)).unwrap();
    assert!(!gate.allowed(ms(1)));
    let request = gate.request(ms(1)).unwrap().unwrap();
    assert!(gate.request(ms(2)).unwrap().is_none());
    assert!(gate.grant(
        &Grant {
            activation: request,
            expires: ms(200)
        },
        ms(3)
    ));
    assert!(gate.allowed(ms(199)));
    assert!(!gate.allowed(ms(200)));
}
#[test]
fn neutral_change_retires_pending_and_pregrab_grants() {
    let (mut gate, old) = ready();
    gate.cancel_pending();
    let new = gate.request(ms(2)).unwrap().unwrap();
    assert!(new.attempt > old.attempt);
    assert!(!gate.grant(
        &Grant {
            activation: old,
            expires: ms(100)
        },
        ms(3)
    ));
    assert!(gate.grant(
        &Grant {
            activation: new,
            expires: ms(100)
        },
        ms(3)
    ));
    gate.cancel_pending();
    assert!(!gate.allowed(ms(4)));
    assert!(gate.request(ms(4)).unwrap().is_some());
}
#[test]
fn grants_are_exact_and_bounded_and_cannot_be_replayed() {
    let (mut gate, request) = ready();
    let mut variants = Vec::new();
    let mut changed = request.clone();
    changed.attempt = 0;
    variants.push(changed);
    let mut changed = request.clone();
    changed.source.generation += 1;
    variants.push(changed);
    let mut changed = request.clone();
    changed.worker =
        serde_json::from_value(serde_json::json!({"pid":std::process::id(),"birth":1,"epoch":2}))
            .unwrap();
    variants.push(changed);
    let mut changed = request.clone();
    changed.fence.session += 1;
    variants.push(changed);
    let mut changed = request.clone();
    changed.owners[0].ui_generation += 1;
    variants.push(changed);
    let mut changed = request.clone();
    changed.observed_at = ms(2);
    variants.push(changed);
    for changed in variants {
        assert!(!gate.grant(
            &Grant {
                activation: changed,
                expires: ms(100)
            },
            ms(2)
        ));
    }
    assert!(!gate.grant(
        &Grant {
            activation: request.clone(),
            expires: ms(2)
        },
        ms(2)
    ));
    assert!(!gate.grant(
        &Grant {
            activation: request.clone(),
            expires: ms(252)
        },
        ms(2)
    ));
    assert!(!gate.grant(
        &Grant {
            activation: request.clone(),
            expires: ms(251)
        },
        ms(251)
    ));
    let grant = Grant {
        activation: request,
        expires: ms(100),
    };
    assert!(gate.grant(&grant, ms(2)));
    assert!(!gate.grant(&grant, ms(3)));
}
#[test]
fn expired_authority_cannot_be_revived_by_renewal() {
    let (mut gate, request) = ready();
    gate.grant(
        &Grant {
            activation: request,
            expires: ms(100),
        },
        ms(2),
    );
    gate.update(admission(Some(ms(200))), ms(50)).unwrap();
    assert!(gate.allowed(ms(199)));
    gate.update(admission(Some(ms(300))), ms(200)).unwrap();
    assert!(!gate.allowed(ms(201)));
    assert!(gate.request(ms(201)).unwrap().is_some());
    gate.update(admission(None), ms(202)).unwrap();
    assert!(!gate.allowed(ms(202)));
}
#[test]
fn requirements_cannot_be_removed_or_replaced_within_a_source_lifetime() {
    let (mut gate, _) = ready();
    assert!(gate.update(None, ms(2)).is_err());
    let mut changed = owner();
    changed.ownership_generation += 1;
    assert!(
        gate.update(
            Some(Admission {
                owners: vec![changed],
                expires: None
            }),
            ms(2)
        )
        .is_err()
    );
    let mut generic = Gate::new(worker(), fence(), source());
    generic.update(None, ms(0)).unwrap();
    assert!(generic.allowed(ms(1)));
    assert!(generic.update(admission(None), ms(2)).is_err());
}
#[test]
fn ownership_sets_and_clocks_are_validated_before_queueing() {
    assert!(!valid_owners(&[]));
    assert!(!valid_owners(&vec![owner(); 9]));
    assert!(!valid_owners(&[owner(), owner()]));
    let mut owner = owner();
    owner.epoch = "A".repeat(32);
    assert!(!valid_owners(&[owner]));
    let mut gate = gate();
    assert!(
        gate.update(
            Some(Admission {
                owners: vec![],
                expires: None
            }),
            ms(0)
        )
        .is_err()
    );
    assert!(gate.update(admission(Some(ms(251))), ms(0)).is_err());
    gate.update(admission(Some(ms(0))), ms(1)).unwrap();
    assert!(!gate.allowed(ms(1)));
    let request = gate.request(ms(10)).unwrap().unwrap();
    assert!(!request.fresh(ms(9)));
    assert!(request.fresh(ms(259)));
    assert!(!request.fresh(ms(260)));
}
#[test]
fn activation_messages_use_only_the_private_channel() {
    let (_, activation) = ready();
    let message = crate::worker::Message::MorfReady {
        activation: activation.clone(),
    };
    let encoded = serde_json::to_value(&message).unwrap();
    assert!(serde_json::from_value::<crate::protocol::Request>(encoded.clone()).is_err());
    let mut unknown = encoded.clone();
    unknown["activation"]["authority"] = serde_json::json!(true);
    assert!(serde_json::from_value::<crate::worker::Message>(unknown).is_err());
    let (mut sender, mut receiver) = Channel::pair().unwrap();
    let now = crate::ipc::monotonic().unwrap();
    assert!(sender.send(&message, now).unwrap());
    let crate::worker::Message::MorfReady {
        activation: received,
    } = receiver.receive().unwrap().unwrap().message
    else {
        panic!("wrong private message")
    };
    assert_eq!(received, activation);
}
