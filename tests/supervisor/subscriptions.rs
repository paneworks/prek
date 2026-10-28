use super::*;
use prek::{
    configuration, context, gestures, ipc, protocol, replay::Record, subscription_transfer, worker,
};
use std::collections::BTreeSet;

pub(super) fn child() {
    let mut health = Heartbeat::connect().unwrap();
    let mut channel = health.take_control().unwrap();
    let config = worker::bootstrap(&mut channel, &mut health).unwrap();
    let mut stream = worker::Stream::new(config, channel).unwrap();
    stream.push(Record::Header { version: 1 }).unwrap();
    loop {
        match health.progress() {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return,
            Err(error) => panic!("subscription worker health failed: {error}"),
        }
        stream.service().unwrap();
        stream
            .push(Record::Tick {
                at: ipc::monotonic().unwrap(),
            })
            .unwrap();
        stream.flush().unwrap();
        thread::sleep(Duration::from_millis(1));
    }
}

fn subscription(now: Duration) -> worker::Subscription {
    worker::Subscription {
        seat: "seat0".into(),
        fence: protocol::Fence {
            broker: protocol::Epoch([1, 2]),
            configuration: 1,
            session: 1,
        },
        capabilities: BTreeSet::new(),
        consumer: None,
        context: Some(Box::new(worker::ProvidedContext {
            binding: context::Binding {
                seat: "seat0".into(),
                session: "process-fixture".into(),
                uid: 1000,
                compositor_pid: 42,
                compositor_start: 123,
                kind: context::SessionKind::User,
            },
            snapshot: context::Snapshot {
                generation: 1,
                revision: 1,
                captured_at: now,
                state: context::State::Desktop,
                outputs: (0..256)
                    .map(|index| context::OutputState {
                        geometry: gestures::Output {
                            name: format!("{}-{index}", "o".repeat(200)),
                            pixel_width: 1000.0,
                            pixel_height: 800.0,
                            scale: 1.0,
                            origin: gestures::Point {
                                x: f64::from(index) * 1000.0,
                                y: 0.0,
                            },
                            transform: gestures::Transform::Normal,
                        },
                        geometry_generation: 1,
                        screen: prek::config::Screen::On,
                    })
                    .collect(),
            },
        })),
    }
}

pub(super) fn parent() {
    let (mut worker, mut channel) = Supervisor::spawn_controlled(command("subscriptions")).unwrap();
    assert_eq!(channel.peer().pid, worker.worker_pid());
    let config = prek::config::Config::preset("desktop").unwrap();
    let mut transfer = configuration::Transfer::new(toml::to_string(&config).unwrap(), 1).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut sent = false;
    loop {
        assert!(worker.check().unwrap().is_none());
        if !sent {
            sent = transfer
                .send(&mut channel, ipc::monotonic().unwrap())
                .unwrap();
        }
        if let Some(packet) = channel.receive::<worker::Message>().unwrap() {
            assert!(matches!(
                packet.message,
                worker::Message::Configuration {
                    generation: 1,
                    error: None
                }
            ));
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    let now = ipc::monotonic().unwrap();
    let update = worker::Subscriptions {
        revision: 1,
        seats: vec![subscription(now)],
    };
    assert!(serde_json::to_vec(&update).unwrap().len() > protocol::MAX_PACKET);
    let mut sender = subscription_transfer::Sender::default();
    sender.replace(update, now).unwrap();
    loop {
        sender
            .send(&mut channel, ipc::monotonic().unwrap())
            .unwrap();
        assert!(worker.check().unwrap().is_none());
        if let Some(packet) = channel.receive::<worker::Message>().unwrap() {
            let worker::Message::Outcome {
                report: worker::Report::Ready { subscription },
            } = packet.message
            else {
                panic!("worker did not accept complete subscription")
            };
            assert!(subscription.context.is_none());
            assert_eq!(subscription.fence.session, 1);
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    let until = Instant::now() + HEARTBEAT_INTERVAL * 2 + Duration::from_millis(250);
    while Instant::now() < until {
        assert!(worker.check().unwrap().is_none());
        thread::sleep(Duration::from_millis(10));
    }
    assert!(worker.heartbeats() >= 2);
    worker.request_stop().unwrap();
    let report = worker.wait().unwrap();
    assert_eq!(report.reason(), Reason::Requested);
    assert!(report.status().success());
}
