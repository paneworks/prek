use super::*;
use crate::transport::{Consumer, ConsumerEvidence};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

struct Fixture {
    directory: PathBuf,
    registry: Registry,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "prek-native-preparation-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let registry = Registry::fixture(&directory).unwrap();
        Self {
            directory,
            registry,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}
fn descriptor() -> Descriptor {
    let mut device = crate::transport::tests::touch();
    device.identity = "test:prepared-keyboard".into();
    device.name = "Prek test prepared keyboard".into();
    device.physical = "fixture/prepared-keyboard".into();
    device.path.clear();
    device.axes.clear();
    device.properties.clear();
    device.capabilities = BTreeMap::from([(0, vec![0]), (1, vec![30]), (17, vec![0])]);
    Descriptor::mirror(
        &device,
        KernelIdentity {
            bus: 3,
            vendor: device.vendor,
            product: device.product,
            version: 1,
        },
        None,
        &BTreeMap::new(),
        BTreeMap::new(),
    )
    .unwrap()
}
fn consumer() -> Consumer {
    Consumer::Hyprland {
        pid: 10,
        start: 20,
        instance: "private-model".into(),
    }
}
fn evidence(descriptor: &Descriptor, consumer: Consumer) -> ConsumerEvidence {
    ConsumerEvidence {
        consumer,
        marker: descriptor.marker.clone(),
        name: descriptor.name.clone(),
        class: descriptor.class,
        seat: descriptor.source.seat.clone(),
        settings: descriptor.consumer_settings.clone(),
        at: Duration::ZERO,
    }
}

#[test]
fn native_preparation_binds_feedback_and_readiness_to_the_actual_publication() {
    let fixture = Fixture::new();
    let base = descriptor();
    let publication = fixture.registry.prepare_isolated(base.clone()).unwrap();
    let mut preparation = NativePreparation::new(&publication, consumer(), Duration::ZERO).unwrap();
    assert_eq!(&preparation.descriptor, publication.descriptor());
    assert_ne!(preparation.descriptor.marker, base.marker);
    assert_eq!(
        preparation.descriptor.udev["PREK_PUBLICATION"],
        preparation.descriptor.marker
    );
    assert_eq!(preparation.descriptor.capabilities[&17], [0]);
    preparation
        .readiness
        .metadata(publication.descriptor(), Duration::ZERO)
        .unwrap();
    let unbound = base.for_isolated().unwrap();
    let mut stale = NativePreparation::new(&publication, consumer(), Duration::ZERO).unwrap();
    assert!(stale.readiness.metadata(&unbound, Duration::ZERO).is_err());
}

#[test]
fn only_stock_hyprland_has_a_native_consumer_takeover_boundary() {
    let fixture = Fixture::new();
    let publication = fixture.registry.prepare_isolated(descriptor()).unwrap();
    for (consumer, admitted) in [
        (consumer(), true),
        (
            Consumer::ConsoleKeyboard {
                handler: "kbd0".into(),
            },
            false,
        ),
        (Consumer::IsolatedReceiver { pid: 10, start: 20 }, false),
    ] {
        let mut preparation =
            NativePreparation::new(&publication, consumer.clone(), Duration::ZERO).unwrap();
        preparation.readiness.neutral(true, Duration::ZERO).unwrap();
        let evidence = evidence(&preparation.descriptor, consumer);
        let boundary = consumer_boundary(
            &mut preparation.readiness,
            &preparation.descriptor,
            &evidence,
            true,
            Duration::ZERO,
        );
        match admitted {
            true => boundary.unwrap(),
            false => assert_eq!(boundary.unwrap_err().kind(), io::ErrorKind::Unsupported),
        }
        assert!(preparation.readiness.finish(Duration::ZERO).is_ok());
    }
}

#[test]
fn source_neutrality_is_not_promoted_to_a_consumer_neutral_acknowledgement() {
    let fixture = Fixture::new();
    let publication = fixture.registry.prepare_isolated(descriptor()).unwrap();
    let mut preparation = NativePreparation::new(&publication, consumer(), Duration::ZERO).unwrap();
    let evidence = evidence(&preparation.descriptor, consumer());
    consumer_boundary(
        &mut preparation.readiness,
        &preparation.descriptor,
        &evidence,
        true,
        Duration::ZERO,
    )
    .unwrap();
    assert!(preparation.readiness.finish(Duration::ZERO).is_err());
}

#[test]
fn delayed_consumer_receipts_do_not_prepare_a_recreated_native_output() {
    let fixture = Fixture::new();
    let publication = fixture.registry.prepare_isolated(descriptor()).unwrap();
    let previous = NativePreparation::new(&publication, consumer(), Duration::ZERO).unwrap();
    let previous = evidence(&previous.descriptor, consumer());
    drop(publication);
    let publication = fixture.registry.prepare_isolated(descriptor()).unwrap();
    let mut current = NativePreparation::new(&publication, consumer(), Duration::ZERO).unwrap();
    assert_ne!(current.descriptor.marker, previous.marker);
    assert_eq!(
        consumer_boundary(
            &mut current.readiness,
            &current.descriptor,
            &previous,
            true,
            Duration::ZERO
        )
        .unwrap_err()
        .kind(),
        io::ErrorKind::InvalidData
    );
}
