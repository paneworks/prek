use super::*;
use prek::{
    input::{EV_REL, InputPosition, TailDomain},
    router::{DomainDecision, Limits, PendingDomain, Router},
};

fn write_due(router: &mut Router, sink: &mut Keyboard, written: &mut Vec<Value>) {
    loop {
        let Some(scheduled) = router.due(monotonic().unwrap()).unwrap().cloned() else {
            break;
        };
        let sent = monotonic().unwrap();
        sink.enqueue(&Frame {
            source: sink.source.clone(),
            ..scheduled.frame.clone()
        })
        .unwrap();
        while sink.flush().unwrap() == Progress::Pending {}
        router
            .delivered(scheduled.ticket, monotonic().unwrap())
            .unwrap();
        written.push(json!({"frame":scheduled.frame, "due":scheduled.due, "sent":sent}));
    }
}

pub(super) fn exercise(output: &Path, sink: &mut Keyboard) {
    let at = monotonic().unwrap();
    let device = Device {
        source: source(601),
        identity: "domain-protocol-model".into(),
        name: "Independent-domain source model".into(),
        path: String::new(),
        physical: String::new(),
        unique: String::new(),
        vendor: 1,
        product: 1,
        properties: vec![],
        capabilities: BTreeMap::from([(EV_KEY, vec![105]), (EV_REL, vec![0, 1])]),
        axes: BTreeMap::new(),
        virtual_device: false,
    };
    let mut router = Router::new(
        device.clone(),
        &ready(&device, at),
        1,
        Limits::default(),
        at,
    )
    .unwrap();
    let make = |sequence, events: &[(u16, u16, i32)]| Frame {
        source: device.source.clone(),
        sequence,
        at: monotonic().unwrap(),
        events: events
            .iter()
            .map(|&(kind, code, value)| RawEvent { kind, code, value })
            .chain([RawEvent {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            }])
            .collect(),
    };
    let first = make(
        1,
        &[
            (EV_REL, 0, 47),
            (EV_KEY, 105, 1),
            (EV_REL, 1, 31),
            (EV_KEY, 105, 0),
        ],
    );
    let roots = router
        .stage_domain_frame(
            first.clone(),
            1,
            &[],
            &[PendingDomain {
                authorization: 1,
                domain: TailDomain::Relative,
                deadline: first.at + Duration::from_millis(200),
                continuation: None,
            }],
            &BTreeSet::new(),
            first.at,
        )
        .unwrap();
    assert_eq!(roots.len(), 1);
    let root = roots[0];
    assert_eq!(root.domain, TailDomain::Relative);
    assert_eq!(root.position, InputPosition { frame: 1, event: 4 });
    let mut written = Vec::new();
    write_due(&mut router, sink, &mut written);
    assert_eq!(written.len(), 1);
    assert!(router.position_prefix_delivered(root.position, false));
    assert!(!router.position_prefix_delivered(root.position, true));
    router
        .decide_domain_scopes(
            &[],
            &[DomainDecision {
                authorization: 1,
                scope: root,
                owner: Some(9),
            }],
            monotonic().unwrap(),
        )
        .unwrap();
    write_due(&mut router, sink, &mut written);
    assert_eq!(written.len(), 1);
    assert!(router.position_prefix_delivered(root.position, true));
    assert!(router.domain_disposition(root).is_none());
    let second = make(2, &[(EV_REL, 0, 5), (EV_REL, 1, 3)]);
    let next = router
        .stage_domain_frame(second.clone(), 2, &[], &[], &BTreeSet::new(), second.at)
        .unwrap();
    assert_eq!(next.len(), 1);
    assert_ne!(next[0], root);
    write_due(&mut router, sink, &mut written);
    assert_eq!(written.len(), 2);
    assert!(router.physical_neutral());
    assert!(router.recover().unwrap().neutral_events.is_empty());
    fs::write(output.join("domain-journal-runtime.json"), serde_json::to_vec(&json!({
        "status":"passed", "scope":"owned_protocol_domain_journal", "source_and_readiness_modeled":true,
        "native_gesture_arbitration_tested":false, "physical_takeover_tested":false,
        "kernel_input_opened":false, "mt_rendering_tested":false, "switch_ownership_tested":false,
        "inputs":[first,second], "delivered":written,
    })).unwrap()).unwrap();
}
