use super::*;
use crate::keys::producer::{Operation as Op, Request};

fn request(fixture: &Fixture, descriptor: &Descriptor, sequence: u64, operation: Op) -> Request {
    Request {
        fence: fixture.owner.fence,
        source: descriptor.source.clone(),
        marker: descriptor.marker.clone(),
        consumer: fixture.owner.consumer.clone().unwrap(),
        producer: Epoch([7, 1]),
        sequence,
        operation,
    }
}
impl Fixture {
    fn software(&mut self, request: Request) {
        self.send(Command::Software {
            request: Box::new(request),
        });
    }
}
fn flush(fixture: &mut Fixture) {
    for _ in 0..4 {
        fixture.stream.flush().unwrap();
    }
}
fn cleanup(fixture: &mut Fixture) {
    fixture.stream.teardown("seat0", monotonic().unwrap());
    fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
    let receipt = fixture
        .drain()
        .into_iter()
        .find_map(|message| match message {
            Message::KeyDrain { request } => Some(request),
            _ => None,
        })
        .unwrap();
    fixture.send(Command::KeyDrained {
        request: receipt,
        at: monotonic().unwrap(),
    });
    fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
    assert!(fixture.stream.closing_keys.is_empty());
    assert!(fixture.stream.closing_outputs.is_empty());
}

#[test]
fn actual_private_software_requests_write_only_the_bound_generated_regular_file() {
    let mut fixture = Fixture::new(profile(), owner());
    let (slot, descriptor, path) = slot(&fixture);
    let raw = physical_file(&mut fixture, &device(), "raw-bytes");
    fixture.stream.seats.get_mut("seat0").unwrap().keys = Some(slot);
    fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
    let expires = monotonic().unwrap() + Duration::from_millis(100);
    let mut wrong = request(&fixture, &descriptor, 1, Op::Open { expires });
    wrong.source = device().source;
    fixture.software(wrong);
    fixture.software(request(&fixture, &descriptor, 1, Op::Open { expires }));
    fixture.software(request(
        &fixture,
        &descriptor,
        2,
        Op::Press { id: 1, key: 42 },
    ));
    assert!(events(&path).is_empty());
    assert!(events(&raw).is_empty());
    flush(&mut fixture);
    assert_eq!(events(&path), [(42, 1)]);
    assert!(events(&raw).is_empty());
    let replies = fixture
        .drain()
        .into_iter()
        .filter_map(|message| match message {
            Message::SoftwareResult {
                producer,
                sequence,
                accepted,
                fence,
            } => {
                assert_eq!(producer, Epoch([7, 1]));
                assert_eq!(fence, fixture.owner.fence);
                Some((sequence, accepted))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(replies, [(1, false), (1, true), (2, true)]);
    fixture.software(request(&fixture, &descriptor, 3, Op::Repeat { id: 1 }));
    flush(&mut fixture);
    fixture.software(request(&fixture, &descriptor, 4, Op::Release { id: 1 }));
    flush(&mut fixture);
    assert_eq!(events(&path), [(42, 1), (42, 2), (42, 0)]);
    fixture.software(request(&fixture, &descriptor, 5, Op::Close {}));
    assert!(events(&raw).is_empty());
    cleanup(&mut fixture);
}

#[test]
fn private_software_leases_cannot_outlive_the_registered_seat() {
    let mut fixture = Fixture::new(profile(), owner());
    let (slot, descriptor, path) = slot(&fixture);
    fixture.stream.seats.get_mut("seat0").unwrap().keys = Some(slot);
    fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
    let now = monotonic().unwrap();
    let original = fixture.stream.seats["seat0"].expires;
    fixture.stream.seats.get_mut("seat0").unwrap().expires = now + Duration::from_millis(10);
    fixture.software(request(
        &fixture,
        &descriptor,
        1,
        Op::Open {
            expires: now + Duration::from_millis(100),
        },
    ));
    assert!(fixture.drain().iter().any(|message| matches!(
        message,
        Message::SoftwareResult {
            accepted: false,
            ..
        }
    )));
    fixture.stream.seats.get_mut("seat0").unwrap().expires = original;
    fixture.software(request(
        &fixture,
        &descriptor,
        1,
        Op::Open {
            expires: now + Duration::from_millis(100),
        },
    ));
    assert!(
        fixture
            .drain()
            .iter()
            .any(|message| matches!(message, Message::SoftwareResult { accepted: true, .. }))
    );
    assert!(events(&path).is_empty());
    cleanup(&mut fixture);
}
