use super::*;
use prek::{
    input::InputPosition,
    router::{KeyDecision, Limits, PendingKey, Router},
};

pub(super) fn exercise(output: &Path, socket: &Path, first_phase: usize, text: &str) {
    let mut sink = Keyboard::open(output, socket, 902);
    let at = monotonic().unwrap();
    let device = Device {
        source: source(902),
        identity: "pending-protocol-source-model".into(),
        name: "Pending protocol fixture source".into(),
        path: String::new(),
        physical: String::new(),
        unique: String::new(),
        vendor: 1,
        product: 1,
        properties: vec![],
        capabilities: BTreeMap::from([(EV_KEY, vec![30])]),
        axes: BTreeMap::new(),
        virtual_device: false,
    };
    let limits = Limits {
        offset: Duration::from_secs(5),
        ..Limits::default()
    };
    let mut router = Router::new(device.clone(), &ready(&device, at), 1, limits, at).unwrap();
    let make = |sequence, at, values: &[i32]| Frame {
        source: device.source.clone(),
        sequence,
        at,
        events: values
            .iter()
            .map(|&value| RawEvent {
                kind: EV_KEY,
                code: 30,
                value,
            })
            .chain([RawEvent {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            }])
            .collect(),
    };
    router
        .stage_key_frame(
            make(1, at, &[1, 0]),
            1,
            &[PendingKey {
                authorization: 1,
                event: 0,
                deadline: at + Duration::from_secs(5),
            }],
            &BTreeSet::new(),
            at,
        )
        .unwrap();
    router.advance(at).unwrap();
    assert!(router.due(at).unwrap().is_none());
    phase(output, first_phase, text, &[]);
    let at = monotonic().unwrap();
    router
        .decide_key_presses(
            &[KeyDecision {
                authorization: 1,
                code: 30,
                position: InputPosition { frame: 1, event: 0 },
                owner: Some(1),
            }],
            at,
        )
        .unwrap();
    assert!(router.due(at).unwrap().is_none());
    phase(output, first_phase + 1, text, &[]);

    let down_at = monotonic().unwrap();
    router
        .stage_key_frame(
            make(2, down_at, &[1]),
            2,
            &[PendingKey {
                authorization: 1,
                event: 0,
                deadline: down_at + Duration::from_millis(200),
            }],
            &BTreeSet::new(),
            down_at,
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(80));
    let up_at = monotonic().unwrap();
    router
        .stage_key_frame(make(3, up_at, &[0]), 3, &[], &BTreeSet::new(), up_at)
        .unwrap();
    let mut written = Vec::new();
    let until = Instant::now() + Duration::from_secs(2);
    while written.len() < 2 {
        let now = monotonic().unwrap();
        if let Some(scheduled) = router.due(now).unwrap().cloned() {
            let edges = scheduled
                .frame
                .events
                .iter()
                .filter(|event| event.kind == EV_KEY)
                .map(|event| (event.code, event.value))
                .collect::<Vec<_>>();
            let sent = monotonic().unwrap();
            sink.send(scheduled.frame.sequence, &edges);
            router
                .delivered(scheduled.ticket, monotonic().unwrap())
                .unwrap();
            written.push((scheduled.due, sent, edges));
        }
        assert!(
            Instant::now() < until,
            "pending raw fallback did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let input_gap = up_at - down_at;
    let scheduled_gap = written[1].0 - written[0].0;
    let delivery_gap = written[1].1 - written[0].1;
    assert_eq!(written[0].2, [(30, 1)]);
    assert_eq!(written[1].2, [(30, 0)]);
    assert_eq!(scheduled_gap, input_gap);
    assert!(delivery_gap >= input_gap.saturating_sub(limits.scheduler_slack));
    assert!(router.physical_neutral());
    assert!(router.recover().unwrap().neutral_events.is_empty());
    phase(output, first_phase + 2, &format!("{text}a"), &[]);
    fs::write(
        output.join("pending-journal-receipt.json"),
        serde_json::to_vec(&json!({
            "status":"passed", "consumed_reports_written":0, "fallback_reports_written":2,
            "input_gap_ns":input_gap.as_nanos(), "scheduled_gap_ns":scheduled_gap.as_nanos(),
            "delivery_gap_ns":delivery_gap.as_nanos(), "slack_ns":limits.scheduler_slack.as_nanos(),
            "physical_neutral":true, "delivered_neutral":true,
            "authority_modeled":true, "delayed_gesture_arbitration_tested":false
        }))
        .unwrap(),
    )
    .unwrap();
}
