use super::*;
use crate::{
    ipc::Channel,
    protocol::Epoch,
    supervisor::WorkerIdentity,
    transport::{Consumer, KernelIdentity},
};
use std::{collections::BTreeMap, time::Duration};

fn fence() -> Fence {
    Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    }
}
fn physical() -> Descriptor {
    let mut device = crate::transport::tests::touch();
    device.source.device = 0;
    Descriptor::mirror(
        &device,
        KernelIdentity {
            bus: 3,
            vendor: 1,
            product: 2,
            version: 1,
        },
        None,
        &BTreeMap::new(),
        BTreeMap::new(),
    )
    .unwrap()
}
fn evidence(descriptor: &Descriptor) -> ConsumerEvidence {
    ConsumerEvidence {
        consumer: Consumer::IsolatedReceiver { pid: 1, start: 1 },
        marker: descriptor.marker.clone(),
        name: descriptor.name.clone(),
        class: descriptor.class,
        seat: descriptor.source.seat.clone(),
        settings: BTreeMap::new(),
        at: Duration::ZERO,
    }
}
fn ledger() -> (Ledger, Channel, Channel, WorkerIdentity) {
    let (channel, peer) = Channel::pair().unwrap();
    let worker =
        serde_json::from_value(serde_json::json!({"pid":channel.peer().pid,"birth":1,"epoch":1}))
            .unwrap();
    (
        Ledger::new(worker, channel.peer()).unwrap(),
        channel,
        peer,
        worker,
    )
}
fn admission() -> Admission {
    Admission {
        owners: vec![crate::morf::admission::Owner {
            output: "screen".into(),
            epoch: "one".into(),
            ui_generation: 1,
            ownership_generation: 1,
        }],
        expires: Some(Duration::from_secs(1)),
    }
}

#[test]
fn generated_transport_never_enrolls_original_release_or_morf_admission() {
    let physical = physical();
    let generated =
        Descriptor::generated_keyboard(physical.source.clone(), BTreeMap::new()).unwrap();
    let (mut ledger, channel, _peer, worker) = ledger();
    ledger.watch(fence(), "seat0").unwrap();
    let mut started = false;
    let command = reply(
        &generated,
        fence(),
        evidence(&generated),
        Some(admission()),
        Some(&mut ledger),
        &mut started,
    )
    .unwrap();
    assert!(!started);
    assert!(!ledger.prepared_by(&channel, worker, fence(), &physical.source));
    assert!(
        matches!(command,Command::Transport { admission: None,marker,.. } if marker == generated.marker)
    );
    let command = reply(
        &physical,
        fence(),
        evidence(&physical),
        Some(admission()),
        Some(&mut ledger),
        &mut started,
    )
    .unwrap();
    assert!(started);
    assert!(ledger.prepared_by(&channel, worker, fence(), &physical.source));
    assert!(
        matches!(command,Command::Transport { admission: Some(_),marker,.. } if marker == physical.marker)
    );
}

#[test]
fn generated_ack_does_not_close_the_later_physical_enrollment_window() {
    let physical = physical();
    let generated =
        Descriptor::generated_keyboard(physical.source.clone(), BTreeMap::new()).unwrap();
    let (mut ledger, _channel, _peer, _worker) = ledger();
    let mut started = false;
    reply(
        &generated,
        fence(),
        evidence(&generated),
        None,
        Some(&mut ledger),
        &mut started,
    )
    .unwrap();
    assert!(!started);
    ledger.watch(fence(), "seat0").unwrap();
}

#[test]
fn publication_identity_is_required_in_the_private_reply() {
    let descriptor = physical();
    let mut started = false;
    let command = reply(
        &descriptor,
        fence(),
        evidence(&descriptor),
        None,
        None,
        &mut started,
    )
    .unwrap();
    let mut value = serde_json::to_value(command).unwrap();
    assert_eq!(value["marker"], descriptor.marker);
    value.as_object_mut().unwrap().remove("marker");
    assert!(serde_json::from_value::<Command>(value).is_err());
}
