#![cfg(target_os = "linux")]
//! Virtual-time scoped routing workload; `oslo make soak-routing` runs it.
use prek::{
    config::Screen,
    context::{Binding, OutputState, SessionKind, Snapshot, State},
    gestures::{Mapping, Output, Point, Transform},
    input::{
        ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, Axis, Device,
        EV_ABS, EV_KEY, EV_SYN, Frame, RawEvent, SYN_REPORT, Source,
    },
    lua::Definition,
    protocol::{Capability, Epoch, Fence},
    replay::Record,
    routing::Routing,
    transport::{Consumer, ConsumerEvidence, Descriptor, KernelIdentity, Readiness},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

const CONFIG: &str = r#"
local p=require('prek')
return p.config.new {
    mode='route', policy={allowed_devices={'touch'}},
    devices={touch={name='Prek soak touch'}},
    rules={
        p.rules.define {id='swipe', devices={'touch'}, contexts={'desktop'}, scope='everywhere',
            delivery='buffer', trigger=p.triggers.swipe {fingers=2, direction='down',
            travel='12mm', fallback_travel='0.1norm'}, action=p.actions.event {name='swipe'}},
    },
}
"#;

fn device() -> Device {
    let axis = Axis {
        minimum: 0,
        maximum: 1000,
        resolution: 10,
        fuzz: 0,
        flat: 0,
    };
    Device {
        identity: "test:soak-touch".into(),
        source: Source {
            device: 1,
            generation: 1,
            seat: "seat0".into(),
        },
        name: "Prek soak touch".into(),
        path: String::new(),
        physical: "fixture/soak-touch".into(),
        unique: String::new(),
        vendor: 0x1209,
        product: 0xbeef,
        properties: vec![1],
        virtual_device: false,
        capabilities: BTreeMap::from([
            (EV_SYN, vec![0]),
            (EV_KEY, vec![0x14a]),
            (
                EV_ABS,
                vec![
                    ABS_MT_SLOT,
                    ABS_MT_POSITION_X,
                    ABS_MT_POSITION_Y,
                    ABS_MT_TRACKING_ID,
                ],
            ),
        ]),
        axes: BTreeMap::from([
            (
                ABS_MT_SLOT,
                Axis {
                    maximum: 9,
                    resolution: 0,
                    ..axis
                },
            ),
            (ABS_MT_POSITION_X, axis),
            (ABS_MT_POSITION_Y, axis),
            (
                ABS_MT_TRACKING_ID,
                Axis {
                    maximum: 65535,
                    resolution: 0,
                    ..axis
                },
            ),
        ]),
    }
}

fn raw(kind: u16, code: u16, value: i32) -> RawEvent {
    RawEvent { kind, code, value }
}

fn contacts(fingers: i32, y: i32, down: bool) -> Vec<RawEvent> {
    let mut events = Vec::new();
    if down {
        events.push(raw(EV_KEY, 0x14a, 1));
    }
    for slot in 0..fingers {
        events.push(raw(EV_ABS, ABS_MT_SLOT, slot));
        if down {
            events.push(raw(EV_ABS, ABS_MT_TRACKING_ID, slot + 7));
            events.push(raw(EV_ABS, ABS_MT_POSITION_X, 200 + slot * 250));
        }
        events.push(raw(EV_ABS, ABS_MT_POSITION_Y, y));
    }
    events
}

fn release(fingers: i32) -> Vec<RawEvent> {
    let mut events = vec![raw(EV_KEY, 0x14a, 0)];
    for slot in 0..fingers {
        events.push(raw(EV_ABS, ABS_MT_SLOT, slot));
        events.push(raw(EV_ABS, ABS_MT_TRACKING_ID, -1));
    }
    events
}

/// One 480 ms cycle of 8 ms slots: a consumed swipe, an ordinary tap and a
/// motionless pair that falls back after the decision window.
fn script(slot: u64) -> Vec<RawEvent> {
    let position = (slot % 60) as i32;
    match position {
        0 => contacts(2, 200, true),
        1..=8 => contacts(2, 200 + position * 60, false),
        9 => release(2),
        12 => contacts(1, 500, true),
        14 => release(1),
        18 => contacts(2, 500, true),
        58 => release(2),
        _ => Vec::new(),
    }
}

fn resident_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: u64 = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * 4096
}

fn ready(device: &Device) -> prek::transport::Ready {
    let descriptor = Descriptor::mirror(
        device,
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
    .unwrap();
    let consumer = Consumer::IsolatedReceiver { pid: 2, start: 1 };
    let mut gate = Readiness::new(descriptor.clone(), consumer.clone(), Duration::ZERO).unwrap();
    gate.metadata(&descriptor, Duration::ZERO).unwrap();
    gate.consumer(
        &ConsumerEvidence {
            consumer,
            marker: descriptor.marker,
            name: descriptor.name,
            class: descriptor.class,
            seat: "seat0".into(),
            settings: BTreeMap::new(),
            at: Duration::ZERO,
        },
        Duration::ZERO,
    )
    .unwrap();
    gate.neutral(true, Duration::ZERO).unwrap();
    gate.finish(Duration::ZERO).unwrap()
}

fn refresh(routing: &mut Routing, revision: u64, now: Duration) {
    let geometry = Output {
        name: "SOAK-1".into(),
        pixel_width: 1000.0,
        pixel_height: 1000.0,
        scale: 1.0,
        origin: Point { x: 0.0, y: 0.0 },
        transform: Transform::Normal,
    };
    routing
        .authority_mut()
        .update(
            Snapshot {
                generation: 1,
                revision,
                captured_at: now,
                state: State::Desktop,
                secure_overlay: false,
                outputs: vec![OutputState {
                    geometry,
                    geometry_generation: 2,
                    active_workspace: Some(1),
                    screen: Screen::On,
                    keyboard_known: true,
                    keyboard: None,
                }],
                windows: vec![],
                regions: vec![],
            },
            now,
        )
        .unwrap();
    routing.actions_available(1, BTreeSet::from([Capability::Observations]), now);
}

#[test]
#[ignore = "long soak; run with oslo make soak-routing"]
fn scoped_routing_keeps_bounded_memory_and_frame_overhead_for_an_hour() {
    let seconds: u64 = std::env::var("PREK_SOAK_SECONDS")
        .ok()
        .map_or(3600, |value| value.parse().unwrap());
    let config = Definition::compile(CONFIG, "routing-soak.lua")
        .unwrap()
        .validate()
        .unwrap();
    let fence = Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    };
    let mut routing = Routing::with_input_projection(config, fence).unwrap();
    routing
        .authority_mut()
        .bind(
            Binding {
                seat: "seat0".into(),
                session: "routing-soak".into(),
                uid: 1000,
                compositor_pid: 42,
                compositor_start: 1,
                kind: SessionKind::User,
            },
            true,
            false,
        )
        .unwrap();
    refresh(&mut routing, 1, Duration::ZERO);
    let device = device();
    let mut mapping = Mapping::from_device(&device).unwrap();
    mapping.generation = 2;
    mapping.output = Some(Output {
        name: "SOAK-1".into(),
        pixel_width: 1000.0,
        pixel_height: 1000.0,
        scale: 1.0,
        origin: Point { x: 0.0, y: 0.0 },
        transform: Transform::Normal,
    });
    routing
        .attach(
            device.clone(),
            &ready(&device),
            Some(mapping),
            Duration::ZERO,
        )
        .unwrap();
    let step = Duration::from_millis(8);
    let end = Duration::from_secs(seconds);
    let mut now = Duration::ZERO;
    let mut histogram = vec![0u64; 100_001];
    let mut slowest = Duration::ZERO;
    let (mut slot, mut frames, mut delivered, mut dispatches) = (0u64, 0u64, 0u64, 0u64);
    let mut samples = Vec::new();
    let started = Instant::now();
    while now < end {
        now += step;
        if slot.is_multiple_of(12) {
            refresh(&mut routing, slot / 12 + 2, now);
        }
        let mut events = script(slot);
        slot += 1;
        let tick = Instant::now();
        if events.is_empty() {
            routing.push(Record::Tick { at: now }).unwrap();
        } else {
            frames += 1;
            events.push(raw(EV_SYN, SYN_REPORT, 0));
            routing
                .push(Record::Frame {
                    frame: Frame {
                        source: device.source.clone(),
                        at: now,
                        sequence: frames,
                        events,
                    },
                })
                .unwrap();
        }
        while let Some(scheduled) = routing.due(&device.source, now).unwrap().cloned() {
            routing
                .delivered(&device.source, scheduled.ticket, now)
                .unwrap();
            delivered += 1;
        }
        dispatches += routing.take_dispatches(now).len() as u64;
        routing.take_events();
        assert!(routing.take_recoveries().is_empty());
        let elapsed = tick.elapsed();
        histogram[(elapsed.as_micros() as usize).min(100_000)] += 1;
        slowest = slowest.max(elapsed);
        if slot.is_multiple_of(7_500) {
            samples.push(resident_bytes());
        }
    }
    let total: u64 = histogram.iter().sum();
    let percentile = |p: u64| {
        let wanted = total.saturating_sub(1) * p / 100;
        let mut seen = 0;
        let micros = histogram
            .iter()
            .position(|count| {
                seen += count;
                seen > wanted
            })
            .unwrap();
        Duration::from_micros(micros as u64)
    };
    let warm = samples[samples.len().min(5) - 1];
    let peak = *samples.iter().max().unwrap();
    let receipt = serde_json::json!({
        "virtual_seconds": seconds,
        "wall_seconds": started.elapsed().as_secs_f64(),
        "slots": total,
        "frames": frames,
        "delivered_reports": delivered,
        "dispatches": dispatches,
        "cycle_p50_us": percentile(50).as_micros(),
        "cycle_p99_us": percentile(99).as_micros(),
        "cycle_max_us": slowest.as_micros(),
        "rss_warm_bytes": warm,
        "rss_peak_bytes": peak,
        "rss_final_bytes": *samples.last().unwrap(),
    });
    let directory = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("routing-soak");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("receipt.json"),
        serde_json::to_vec_pretty(&receipt).unwrap(),
    )
    .unwrap();
    eprintln!("{receipt}");
    let cycles = slot / 60;
    assert!(
        dispatches + 1 >= cycles && dispatches <= cycles + 1,
        "{receipt}"
    );
    assert!(
        delivered + 4 >= 4 * cycles && delivered <= 4 * (cycles + 1),
        "consumed swipes must not leak: {receipt}"
    );
    assert!(peak - warm < 4 * 1024 * 1024, "{receipt}");
    assert!(percentile(99) < Duration::from_millis(5), "{receipt}");
}
