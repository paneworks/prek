//! Real supervised Lua callbacks with explicitly modeled native routing readiness.
use super::*;
use prek::{
    config::Delivery,
    input::{
        ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, Axis, Device,
        EV_ABS, EV_KEY, EV_REL, EV_SYN, Frame, RawEvent, SYN_REPORT, Source,
    },
    input_events::{DeviceInfo, ResetReason},
    protocol::Capability,
    recognition::{Metadata, Phase, Proposal, Retirement, Ticket},
    replay::Record,
    routing::Routing,
    rules::{Event, Evidence, Stage},
    transport::{Consumer, ConsumerEvidence, Descriptor, KernelIdentity, Readiness},
};
use std::collections::{BTreeMap, BTreeSet};

struct Harness {
    session: Session,
    routing: Routing,
    device: Device,
    sequence: u64,
}

#[derive(Clone, Copy)]
enum SequenceOrder {
    NativeFirst,
    CustomFirst,
}

struct CapturedSink {
    source: Source,
    pending: Option<Frame>,
    frames: std::rc::Rc<std::cell::RefCell<Vec<Frame>>>,
}
impl prek::keys::output::Sink for CapturedSink {
    fn source(&self) -> &Source {
        &self.source
    }
    fn supports_key(&self, code: u16) -> bool {
        code == 42
    }
    fn enqueue(&mut self, frame: &Frame) -> std::io::Result<()> {
        assert!(self.pending.is_none());
        self.pending = Some(frame.clone());
        Ok(())
    }
    fn flush(&mut self) -> std::io::Result<prek::keys::output::Progress> {
        self.frames.borrow_mut().push(self.pending.take().unwrap());
        Ok(prek::keys::output::Progress::Complete)
    }
    fn abort(&mut self) -> std::io::Result<Vec<RawEvent>> {
        self.pending.take();
        Ok(vec![])
    }
}

#[test]
fn projected_custom_keys_preserve_unseen_typing_through_the_real_lua_helper() {
    let mut harness = Harness::with_key_output(
        "assert(#ticket.input.changes==1 and ticket.input.changes[1].code==30);return {phase='discrete',data={original=ticket.input.sequence}}",
        false,
        "buffer",
        true,
    );
    let first = harness.key(1).unwrap();
    let proposal = harness.proposal(first);
    harness.sequence += 1;
    harness
        .routing
        .push(Record::Frame {
            frame: Frame {
                source: harness.device.source.clone(),
                at: monotonic().unwrap(),
                sequence: harness.sequence,
                events: vec![
                    RawEvent {
                        kind: EV_KEY,
                        code: 31,
                        value: 1,
                    },
                    RawEvent {
                        kind: EV_KEY,
                        code: 31,
                        value: 0,
                    },
                    RawEvent {
                        kind: EV_SYN,
                        code: SYN_REPORT,
                        value: 0,
                    },
                ],
            },
        })
        .unwrap();
    harness.respond(proposal);
    let work = harness
        .routing
        .take_scoped_dispatches(monotonic().unwrap())
        .into_iter()
        .map(|dispatch| dispatch.into_key_work().ok().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(work.len(), 1);
    let frames = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let output = prek::keys::output::Output::new(CapturedSink {
        source: Source {
            device: 100,
            generation: 1,
            seat: "seat0".into(),
        },
        pending: None,
        frames: frames.clone(),
    })
    .unwrap();
    let mut seat = prek::keys::worker::Seat::new(fence(1), output).unwrap();
    seat.admit(&harness.routing, work, None, monotonic().unwrap())
        .unwrap();
    let mut ordinary = Vec::new();
    let end = monotonic().unwrap() + Duration::from_millis(200);
    loop {
        let at = monotonic().unwrap();
        seat.poll(&harness.routing, at).unwrap();
        while let Some(scheduled) = harness
            .routing
            .due(&harness.device.source, at)
            .unwrap()
            .cloned()
        {
            let span = harness
                .routing
                .input_span(&harness.device.source, scheduled.ticket)
                .unwrap();
            if seat.blocks_ordered_span(&harness.routing, &harness.device.source, span) {
                break;
            }
            ordinary.extend(
                scheduled
                    .frame
                    .events
                    .iter()
                    .filter(|event| event.kind == EV_KEY)
                    .map(|event| (event.code, event.value)),
            );
            harness
                .routing
                .delivered(&harness.device.source, scheduled.ticket, at)
                .unwrap();
        }
        if seat.settled() {
            break;
        }
        assert!(at < end, "projected custom key work did not settle");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(ordinary, [(31, 1), (31, 0)]);
    assert_eq!(
        frames
            .borrow()
            .iter()
            .flat_map(|frame| frame.events.iter())
            .filter(|event| event.kind == EV_KEY)
            .map(|event| (event.code, event.value))
            .collect::<Vec<_>>(),
        [(42, 1), (42, 0)]
    );
    harness.cleanup(Retirement::Completed);
    harness.stop();
}
#[test]
fn projected_custom_keys_admit_only_original_edges_after_activation() {
    let mut harness = Harness::with_conditions(
        "assert(#ticket.input.changes==3);assert(ticket.input.changes[1].code==42 and ticket.input.changes[1].state=='press');assert(ticket.input.changes[2].code==30);assert(ticket.input.changes[3].code==42 and ticket.input.changes[3].state=='release');return {phase='discrete',data={original=ticket.input.sequence}}",
        "require_keys={'KEY_LEFTSHIFT'},",
    );
    let ticket = harness
        .key_report(&[(31, 1), (31, 0), (42, 1), (30, 1), (42, 0)])
        .unwrap();
    let proposal = harness.proposal(ticket);
    assert_eq!(proposal.data, serde_json::json!({"original":"1"}));
    harness.respond(proposal);
    assert_eq!(
        harness
            .routing
            .take_scoped_dispatches(monotonic().unwrap())
            .len(),
        1
    );
    harness.cleanup(Retirement::Completed);
    harness.stop();
}

#[test]
fn projected_custom_keys_revoke_a_real_reply_after_transient_condition_loss() {
    let mut harness = Harness::with_conditions(
        "return {phase='discrete',data={original=ticket.input.sequence}}",
        "require_keys={'KEY_LEFTSHIFT'},keys_throughout=true,",
    );
    let ticket = harness.key_report(&[(42, 1), (30, 1)]).unwrap();
    let proposal = harness.proposal(ticket);
    assert!(harness.key_report(&[(42, 0), (42, 1)]).is_none());
    assert!(
        harness
            .routing
            .recognition_response(proposal, monotonic().unwrap())
            .is_err()
    );
    assert!(
        harness
            .routing
            .take_scoped_dispatches(monotonic().unwrap())
            .is_empty()
    );
    harness.cleanup(Retirement::Cancelled);
    harness.stop();
}

#[test]
fn projected_custom_keys_native_prefix_delivers_only_its_original_suffix_to_lua() {
    let mut harness = Harness::with_sequence(SequenceOrder::NativeFirst);
    let ticket = harness.key_report(&[(31, 1), (31, 0), (30, 1)]).unwrap();
    let proposal = harness.proposal(ticket);
    harness.respond(proposal);
    assert_eq!(
        harness
            .routing
            .take_scoped_dispatches(monotonic().unwrap())
            .len(),
        1
    );
    harness.cleanup(Retirement::Completed);
    harness.stop();
}

#[test]
fn projected_custom_keys_native_suffix_waits_for_a_fresh_post_reply_press() {
    let mut harness = Harness::with_sequence(SequenceOrder::CustomFirst);
    let ticket = harness.key(1).unwrap();
    let proposal = harness.proposal(ticket);
    assert!(harness.key_report(&[(31, 1), (31, 0)]).is_some());
    harness.respond(proposal);
    assert!(
        harness
            .routing
            .take_scoped_dispatches(monotonic().unwrap())
            .is_empty()
    );
    assert!(harness.key_report(&[(31, 1)]).is_none());
    assert_eq!(
        harness
            .routing
            .take_scoped_dispatches(monotonic().unwrap())
            .len(),
        1
    );
    harness.cleanup(Retirement::Completed);
    harness.stop();
}

#[test]
fn input_domains_real_helper_cannot_extend_an_exclusive_group_with_undeclared_held_input() {
    for touch in [false, true] {
        let (inputs, body) = if touch {
            (
                "'touch'",
                "assert(#ticket.input.contacts==1 and #ticket.input.changes==0);return {phase='begin'}",
            )
        } else {
            (
                "'key','relative'",
                "assert(#ticket.input.contacts==0 and #ticket.input.changes==2);return {phase='begin'}",
            )
        };
        let mut harness = Harness::configured(body, true, "buffer", false, "", None, inputs);
        let ticket = harness
            .raw_report(&[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 7),
                (EV_ABS, ABS_MT_POSITION_X, 100),
                (EV_ABS, ABS_MT_POSITION_Y, 200),
                (EV_KEY, 0x14a, 1),
                (EV_KEY, 30, 1),
                (EV_REL, 0, 4),
            ])
            .unwrap();
        let proposal = harness.proposal(ticket);
        if touch {
            harness.raw_report(&[(EV_ABS, ABS_MT_TRACKING_ID, -1), (EV_KEY, 0x14a, 0)]);
        } else {
            harness.raw_report(&[(EV_KEY, 30, 0)]);
        }
        let error = harness
            .routing
            .recognition_response(proposal, monotonic().unwrap())
            .unwrap_err();
        assert!(error.to_string().contains("physical completion"), "{error}");
        assert!(
            harness
                .routing
                .take_dispatches(monotonic().unwrap())
                .is_empty()
        );
        harness.cleanup(Retirement::Cancelled);
        harness.stop();
    }
}

impl Harness {
    fn new(body: &str, continuous: bool, delivery: &str) -> Self {
        Self::with_key_output(body, continuous, delivery, false)
    }
    fn with_key_output(body: &str, continuous: bool, delivery: &str, projected: bool) -> Self {
        Self::configured(body, continuous, delivery, projected, "", None, "'key'")
    }
    fn with_conditions(body: &str, activation: &str) -> Self {
        Self::configured(body, false, "buffer", true, activation, None, "'key'")
    }
    fn with_sequence(order: SequenceOrder) -> Self {
        Self::configured(
            "assert(#ticket.input.changes==1 and ticket.input.changes[1].code==30);return {phase='discrete'}",
            false,
            "buffer",
            true,
            "",
            Some(order),
            "'key'",
        )
    }
    fn configured(
        body: &str,
        continuous: bool,
        delivery: &str,
        projected: bool,
        activation: &str,
        sequence: Option<SequenceOrder>,
        inputs: &str,
    ) -> Self {
        let trigger = match sequence {
            None => "trigger",
            Some(SequenceOrder::NativeFirst) => {
                "p.triggers.sequence{steps={p.triggers.key{key='KEY_A'},trigger},timeout='200ms'}"
            }
            Some(SequenceOrder::CustomFirst) => {
                "p.triggers.sequence{steps={trigger,p.triggers.key{key='KEY_S'}},timeout='200ms'}"
            }
        };
        let action = if projected {
            "p.actions.keys{keys={'KEY_LEFTSHIFT'}}"
        } else {
            "p.actions.event{name='native-route'}"
        };
        let definition = Definition::compile(
            &format!(
                r#"
local p=require('prek');local calls=0
p.events.on('input',function()error('ordinary input reached custom-only helper')end)
local trigger=p.recognizers.define('native_route',{{
 selectors={{'keyboard'}},inputs={{{inputs}}},reply_within='250ms',continuous={continuous},
}},function(ticket)
 if ticket.cause=='cancel' then
  assert(ticket.input==nil);p.log.info('cleanup',{{reason=ticket.reason}});return nil
 end
 calls=calls+1
 assert(ticket.input.source.device=='18446744073709551615')
 assert(ticket.input.source.generation=='18446744073709551614')
 local info=p.devices.get(ticket.input.source)
 assert(info and info.device.name=='Prek test native routing' and #p.devices.list()==1)
 {body}
end)
return p.config.new{{mode='route',
 policy={{allow_input_events=true,allowed_devices={{'keyboard'}}}},
 timing={{join_window='1ms',decision_window='200ms'}},
 devices={{keyboard={{keys={{'KEY_A'}}}}}},
 rules={{p.rules.define{{id='native-route',devices={{'keyboard'}},contexts={{'desktop'}},
  scope='everywhere',delivery='{delivery}',trigger={trigger},{activation}
  action={action}}}}},
}}
"#
            ),
            "native-routing-helper.lua",
        )
        .unwrap();
        let config = definition.validate().unwrap();
        assert_eq!(
            config.rules[0].delivery,
            if delivery == "reserve" {
                Delivery::Reserve
            } else {
                Delivery::Buffer
            }
        );
        let mut session = Session::spawn(
            Path::new(env!("CARGO_BIN_EXE_prek")),
            fence(1),
            "seat0",
            definition,
        )
        .unwrap();
        drain(&mut session).unwrap();
        authorize_recognizer(&mut session);
        let at = monotonic().unwrap();
        let provided = context_view(at, 0).context.unwrap();
        let mapped_output = (inputs != "'key'").then(|| provided.snapshot.outputs[0].clone());
        let mut routing = if projected {
            Routing::with_key_projection(config, fence(1))
        } else {
            Routing::with_recognition(config, fence(1))
        }
        .unwrap();
        routing
            .authority_mut()
            .bind(provided.binding, true, false)
            .unwrap();
        routing
            .authority_mut()
            .update(provided.snapshot, at)
            .unwrap();
        routing.actions_available(
            1,
            if projected {
                BTreeSet::from([Capability::SyntheticInput])
            } else {
                BTreeSet::from([Capability::LuaEvents, Capability::Observations])
            },
            at,
        );
        let mut device = Device {
            source: Source {
                device: u64::MAX,
                generation: u64::MAX - 1,
                seat: "seat0".into(),
            },
            identity: "native-routing-model".into(),
            name: "Prek test native routing".into(),
            path: String::new(),
            physical: "fixture/native-routing".into(),
            unique: String::new(),
            vendor: 1,
            product: 1,
            properties: vec![],
            axes: BTreeMap::new(),
            capabilities: BTreeMap::from([
                (EV_SYN, vec![SYN_REPORT]),
                (
                    EV_KEY,
                    if projected {
                        vec![30, 31, 42]
                    } else {
                        vec![30]
                    },
                ),
            ]),
            virtual_device: false,
        };
        if inputs != "'key'" {
            device.properties = vec![1];
            device.capabilities.get_mut(&EV_KEY).unwrap().push(0x14a);
            device.capabilities.insert(EV_REL, vec![0]);
            device.axes = [
                (ABS_MT_SLOT, 1),
                (ABS_MT_TRACKING_ID, 65535),
                (ABS_MT_POSITION_X, 1000),
                (ABS_MT_POSITION_Y, 1000),
            ]
            .into_iter()
            .map(|(code, maximum)| {
                (
                    code,
                    Axis {
                        minimum: 0,
                        maximum,
                        resolution: 0,
                        fuzz: 0,
                        flat: 0,
                    },
                )
            })
            .collect();
            device
                .capabilities
                .insert(EV_ABS, device.axes.keys().copied().collect());
        }
        let mapping = mapped_output.map(|output| {
            let mut mapping = prek::gestures::Mapping::from_device(&device).unwrap();
            mapping.output = Some(output.geometry);
            mapping.generation = output.geometry_generation;
            mapping
        });
        let descriptor = Descriptor::mirror(
            &device,
            KernelIdentity {
                bus: 3,
                vendor: 1,
                product: 1,
                version: 1,
            },
            None,
            &BTreeMap::new(),
        )
        .unwrap();
        let consumer = Consumer::IsolatedReceiver { pid: 1, start: 1 };
        let mut readiness = Readiness::new(descriptor.clone(), consumer.clone(), at).unwrap();
        readiness.metadata(&descriptor, at).unwrap();
        readiness
            .consumer(
                &ConsumerEvidence {
                    consumer,
                    marker: descriptor.marker,
                    name: descriptor.name,
                    class: descriptor.class,
                    seat: "seat0".into(),
                    at,
                },
                at,
            )
            .unwrap();
        readiness.neutral(true, at).unwrap();
        routing
            .attach(
                device.clone(),
                &readiness.finish(at).unwrap(),
                mapping.clone(),
                at,
            )
            .unwrap();
        session
            .recognizer_metadata(Metadata {
                source: device.source.clone(),
                at,
                selectors: vec!["keyboard".into()],
                reason: ResetReason::Attached,
                metadata: Some(Box::new(DeviceInfo {
                    device: device.clone(),
                    selectors: vec!["keyboard".into()],
                    mapping,
                })),
            })
            .unwrap();
        drain(&mut session).unwrap();
        Self {
            session,
            routing,
            device,
            sequence: 0,
        }
    }
    fn key(&mut self, value: i32) -> Option<Ticket> {
        self.key_report(&[(30, value)])
    }
    fn key_report(&mut self, changes: &[(u16, i32)]) -> Option<Ticket> {
        self.raw_report(
            &changes
                .iter()
                .map(|&(code, value)| (EV_KEY, code, value))
                .collect::<Vec<_>>(),
        )
    }
    fn raw_report(&mut self, changes: &[(u16, u16, i32)]) -> Option<Ticket> {
        self.sequence += 1;
        self.routing
            .push(Record::Frame {
                frame: Frame {
                    source: self.device.source.clone(),
                    at: monotonic().unwrap(),
                    sequence: self.sequence,
                    events: changes
                        .iter()
                        .map(|&(kind, code, value)| RawEvent { kind, code, value })
                        .chain([RawEvent {
                            kind: EV_SYN,
                            code: SYN_REPORT,
                            value: 0,
                        }])
                        .collect(),
                },
            })
            .unwrap();
        let mut tickets = self
            .routing
            .recognition_requests(monotonic().unwrap())
            .unwrap();
        assert!(tickets.len() <= 1);
        tickets.pop()
    }
    fn proposal(&mut self, ticket: Ticket) -> Proposal {
        self.session.request_recognition(ticket.clone()).unwrap();
        assert!(drain(&mut self.session).unwrap().is_empty());
        let replies = self.session.take_recognitions();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].0, ticket.id);
        replies
            .into_iter()
            .next()
            .unwrap()
            .1
            .expect("successful actual callback")
    }
    fn respond(&mut self, proposal: Proposal) {
        self.routing
            .recognition_response(proposal, monotonic().unwrap())
            .unwrap();
    }
    fn cleanup(&mut self, expected: Retirement) {
        let cancellations = self.routing.take_recognition_cancellations();
        assert_eq!(cancellations.len(), 1);
        assert_eq!(cancellations[0].reason, expected);
        self.session
            .cancel_recognition(cancellations[0].clone())
            .unwrap();
        let logs = drain(&mut self.session).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].message, "cleanup");
        assert_eq!(
            logs[0].fields["reason"],
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(self.session.take_recognition_cancellations(), cancellations);
        assert!(self.session.take_recognitions().is_empty());
    }
    fn stop(&mut self) {
        self.session.stop().unwrap();
        drain(&mut self.session).unwrap();
    }
}

#[test]
fn actual_helper_continuous_results_preserve_native_claim_and_phase_order() {
    let mut harness = Harness::new(
        r#"
 local phases={'begin','update','end'}
 return {phase=phases[calls],data={calls=calls,sequence=ticket.input.sequence}}
"#,
        true,
        "reserve",
    );
    let mut candidate = None;
    for (index, (value, phase, stage)) in [
        (1, Phase::Begin, Stage::Begin),
        (2, Phase::Update, Stage::Update),
        (0, Phase::End, Stage::End),
    ]
    .into_iter()
    .enumerate()
    {
        let ticket = harness.key(value).expect("native frame ticket");
        assert_eq!(*candidate.get_or_insert(ticket.candidate), ticket.candidate);
        let proposal = harness.proposal(ticket);
        assert_eq!(proposal.phase, phase);
        harness.respond(proposal);
        std::thread::sleep(Duration::from_millis(2));
        let now = monotonic().unwrap();
        harness.routing.push(Record::Tick { at: now }).unwrap();
        let dispatches = harness.routing.take_dispatches(now);
        assert!(matches!(&dispatches[..], [Event::Matched {
            stage: actual, ordinal, evidence: Evidence::Custom { data, .. }, ..
        }] if *actual == stage && *ordinal == index as u64 + 1 && data["calls"] == index + 1));
        assert!(
            harness
                .routing
                .due(&harness.device.source, monotonic().unwrap())
                .unwrap()
                .is_none()
        );
    }
    harness.cleanup(Retirement::Completed);
    assert!(harness.routing.take_recoveries().is_empty());
    harness.stop();
}

#[test]
fn actual_helper_result_after_native_transport_loss_cannot_claim_or_dispatch() {
    let mut harness = Harness::new("return {phase='begin',data={calls=calls}}", true, "reserve");
    let ticket = harness.key(1).unwrap();
    let proposal = harness.proposal(ticket);
    harness.routing.recover(&harness.device.source);
    harness.cleanup(Retirement::SourceLost);
    assert!(
        harness
            .routing
            .recognition_response(proposal, monotonic().unwrap())
            .is_err()
    );
    assert!(
        harness
            .routing
            .take_dispatches(monotonic().unwrap())
            .is_empty()
    );
    assert_eq!(harness.routing.take_recoveries().len(), 1);
    harness.stop();
}

#[test]
fn actual_helper_rejection_preserves_each_passthrough_frame_once() {
    let mut harness = Harness::new("return {phase='cancel'}", false, "buffer");
    let ticket = harness.key(1).unwrap();
    let proposal = harness.proposal(ticket);
    harness.respond(proposal);
    assert!(harness.key(0).is_none());
    let mut frames = vec![];
    let deadline = monotonic().unwrap() + Duration::from_millis(200);
    loop {
        let now = monotonic().unwrap();
        harness.routing.push(Record::Tick { at: now }).unwrap();
        while let Some(scheduled) = harness
            .routing
            .due(&harness.device.source, monotonic().unwrap())
            .unwrap()
            .cloned()
        {
            frames.push(scheduled.frame);
            harness
                .routing
                .delivered(
                    &harness.device.source,
                    scheduled.ticket,
                    monotonic().unwrap(),
                )
                .unwrap();
        }
        if frames.len() >= 2 {
            break;
        }
        assert!(
            monotonic().unwrap() < deadline,
            "native fallback did not drain"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        frames
            .iter()
            .map(|frame| frame.sequence)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        frames
            .iter()
            .flat_map(|frame| frame.events.iter())
            .filter(|event| event.kind == EV_KEY)
            .map(|event| event.value)
            .collect::<Vec<_>>(),
        [1, 0]
    );
    assert!(
        harness
            .routing
            .take_dispatches(monotonic().unwrap())
            .is_empty()
    );
    harness.cleanup(Retirement::Cancelled);
    assert!(harness.routing.take_recoveries().is_empty());
    harness.stop();
}
