use super::*;
use crate::{
    protocol::Epoch,
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
        at: Duration::ZERO,
    }
}

#[test]
fn generated_transport_does_not_start_the_physical_takeover() {
    let physical = physical();
    let generated = Descriptor::generated_keyboard(physical.source.clone()).unwrap();
    let mut started = false;
    let command = reply(&generated, fence(), evidence(&generated), &mut started).unwrap();
    assert!(!started);
    assert!(matches!(command,Command::Transport { marker,.. } if marker == generated.marker));
    let command = reply(&physical, fence(), evidence(&physical), &mut started).unwrap();
    assert!(started);
    assert!(matches!(command,Command::Transport { marker,.. } if marker == physical.marker));
}

#[test]
fn publication_identity_is_required_in_the_private_reply() {
    let descriptor = physical();
    let mut started = false;
    let command = reply(&descriptor, fence(), evidence(&descriptor), &mut started).unwrap();
    let mut value = serde_json::to_value(command).unwrap();
    assert_eq!(value["marker"], descriptor.marker);
    value.as_object_mut().unwrap().remove("marker");
    assert!(serde_json::from_value::<Command>(value).is_err());
}
