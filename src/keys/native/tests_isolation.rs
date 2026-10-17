use super::*;

#[test]
fn production_generated_preparation_cannot_use_the_test_receiver_or_reverse_its_scope() {
    let source = Source {
        device: 0,
        generation: 1,
        seat: "seat0".into(),
    };
    let descriptor = Descriptor::generated_keyboard(source, BTreeMap::new()).unwrap();
    let isolated = Consumer::IsolatedReceiver {
        pid: 123,
        start: 456,
    };
    assert!(Admission::new(descriptor.clone(), isolated.clone(), Duration::ZERO).is_err());
    let descriptor = descriptor.for_isolated().unwrap();
    assert!(
        Admission::new(
            descriptor.clone(),
            Consumer::ConsoleKeyboard {
                handler: "kbd0".into()
            },
            Duration::ZERO
        )
        .is_err()
    );
    assert!(
        Admission::new(
            descriptor.clone(),
            Consumer::Hyprland {
                pid: 123,
                start: 456,
                instance: "owned-model".into()
            },
            Duration::ZERO
        )
        .is_err()
    );
    let mut admission =
        Admission::new(descriptor.clone(), isolated.clone(), Duration::ZERO).unwrap();
    admission
        .observe(
            &ConsumerEvidence {
                consumer: isolated,
                marker: descriptor.marker.clone(),
                name: descriptor.name.clone(),
                class: descriptor.class,
                seat: descriptor.source.seat.clone(),
                settings: BTreeMap::new(),
                at: Duration::ZERO,
            },
            Duration::ZERO,
        )
        .unwrap();
    assert!(
        admission
            .poll(&descriptor, true, Duration::ZERO)
            .unwrap()
            .unwrap()
            .matches_generated(&descriptor)
    );
}
