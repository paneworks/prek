//! Real supervised Lua callbacks with explicitly modeled native routing readiness.
use super::*;
use prek::{
    config::Delivery,
    input::{Device, EV_KEY, EV_SYN, Frame, RawEvent, SYN_REPORT, Source},
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
impl Harness {
    fn new(body: &str, continuous: bool, delivery: &str) -> Self {
        Self::with_key_output(body, continuous, delivery, false)
    }
    fn with_key_output(body: &str, continuous: bool, delivery: &str, projected: bool) -> Self {
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
 selectors={{'keyboard'}},inputs={{'key'}},reply_within='250ms',continuous={continuous},
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
  scope='everywhere',delivery='{delivery}',trigger=trigger,
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
        let device = Device {
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
                (EV_KEY, if projected { vec![30, 31] } else { vec![30] }),
            ]),
            virtual_device: false,
        };
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
            BTreeMap::new(),
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
                    settings: BTreeMap::new(),
                    at,
                },
                at,
            )
            .unwrap();
        readiness.neutral(true, at).unwrap();
        routing
            .attach(device.clone(), &readiness.finish(at).unwrap(), None, at)
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
                    mapping: None,
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
        self.sequence += 1;
        self.routing
            .push(Record::Frame {
                frame: Frame {
                    source: self.device.source.clone(),
                    at: monotonic().unwrap(),
                    sequence: self.sequence,
                    events: vec![
                        RawEvent {
                            kind: EV_KEY,
                            code: 30,
                            value,
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
