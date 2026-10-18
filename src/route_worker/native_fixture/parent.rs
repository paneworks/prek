use super::*;

mod software;

struct Parent {
    supervisor: Supervisor,
    channel: Channel,
    fixture: Channel,
    consumer: Consumer,
    fence: Fence,
    revision: u64,
    synthetic: bool,
    renewed: Instant,
    endpoints: BTreeMap<Source, Endpoint>,
    receivers: BTreeMap<Source, kernel::Receiver>,
    captured: BTreeMap<Source, Vec<Frame>>,
    software: software::Control,
    step: u64,
    neutral_drains: BTreeMap<Source, crate::keys::drain::Request>,
    released: BTreeSet<Source>,
    finishing: bool,
    reaped: bool,
}
impl Parent {
    fn capture(&mut self) -> io::Result<()> {
        for (source, receiver) in &mut self.receivers {
            if self.neutral_drains.contains_key(source) {
                continue;
            }
            let frames = receiver.drain_complete()?;
            for frame in &frames {
                if frame.events.last()
                    != Some(&RawEvent {
                        kind: EV_SYN,
                        code: SYN_REPORT,
                        value: 0,
                    })
                    || frame.events[..frame.events.len() - 1]
                        .iter()
                        .any(|event| event.kind != EV_KEY)
                {
                    return Err(invalid(
                        "managed kernel output is not a complete key report",
                    ));
                }
            }
            let buffered = self.captured.entry(source.clone()).or_default();
            if buffered.len().saturating_add(frames.len()) > 64 {
                return Err(invalid(
                    "managed captured output exceeded its fixture bound",
                ));
            }
            buffered.extend(frames);
        }
        Ok(())
    }
    fn acknowledge_drain(&mut self, request: crate::keys::drain::Request) -> io::Result<()> {
        let now = monotonic()?;
        let endpoint = self
            .endpoints
            .get(&request.source)
            .ok_or_else(|| invalid("managed drain source is unknown"))?;
        if request.fence != self.fence
            || request.consumer != self.consumer
            || request.marker != endpoint.descriptor.marker
            || !endpoint.descriptor.is_generated_keyboard()
            || request.requested_at > now
            || now >= request.deadline
            || request.nonce.0 == [0, 0]
            || self.neutral_drains.contains_key(&request.source)
        {
            return Err(invalid("managed consumer drain challenge differs"));
        }
        self.capture()?;
        let receiver = self
            .receivers
            .get(&request.source)
            .ok_or_else(|| invalid("managed drain receiver is missing"))?;
        let keys_down: Vec<_> = receiver
            .raw
            .get_key_state()?
            .iter()
            .map(|key| key.0)
            .collect();
        if !keys_down.is_empty() {
            return Err(invalid("managed drain receiver is not actually neutral"));
        }
        let at = monotonic()?;
        if at >= request.deadline {
            return Err(invalid("managed consumer drain deadline expired"));
        }
        let drained = self
            .captured
            .get(&request.source)
            .ok_or_else(|| invalid("managed final release was not read"))?;
        if drained.len() != 1 || edges(drained) != [(42, 0)] {
            return Err(invalid("managed final release was not completely consumed"));
        }
        let last_report_at_ns = drained[0].at.as_nanos();
        control::send(
            &mut self.channel,
            &Command::KeyDrained {
                request: request.clone(),
                at,
            },
        )?;
        println!(
            "{DRAIN}{}",
            serde_json::json!({
                "version":1, "nonce":request.nonce.0,
                "requested_at_ns":request.requested_at.as_nanos(),
                "neutral_at_ns":at.as_nanos(), "deadline_ns":request.deadline.as_nanos(),
                "last_report_at_ns":last_report_at_ns, "keys_down":keys_down,
                "generated":[[42,0]], "reports":drained.len()
            })
        );
        self.neutral_drains.insert(request.source.clone(), request);
        Ok(())
    }
    fn renew(&mut self) -> io::Result<()> {
        self.revision += 1;
        let subscription = subscription(self.fence, &self.consumer, self.revision, self.synthetic)?;
        control::send(
            &mut self.channel,
            &Command::Subscriptions {
                update: Subscriptions {
                    revision: self.revision,
                    seats: vec![subscription],
                },
            },
        )?;
        let endpoints: Vec<_> = self.endpoints.values().cloned().collect();
        for endpoint in endpoints {
            self.transport(&endpoint)?;
        }
        self.renewed = Instant::now();
        Ok(())
    }
    fn transport(&mut self, endpoint: &Endpoint) -> io::Result<()> {
        let descriptor = &endpoint.descriptor;
        control::send(
            &mut self.channel,
            &Command::Transport {
                fence: self.fence,
                source: descriptor.source.clone(),
                marker: descriptor.marker.clone(),
                evidence: ConsumerEvidence {
                    consumer: self.consumer.clone(),
                    marker: descriptor.marker.clone(),
                    name: descriptor.name.clone(),
                    class: descriptor.class,
                    seat: descriptor.source.seat.clone(),
                    settings: descriptor.consumer_settings.clone(),
                    at: monotonic()?,
                },
                admission: None,
            },
        )
    }
    fn tick(&mut self) -> io::Result<()> {
        if self.supervisor.check()?.is_some() && !self.finishing {
            return Err(invalid("managed worker exited before its fixture finished"));
        }
        loop {
            let packet = match self.channel.receive::<Message>() {
                Ok(Some(packet)) => packet,
                Ok(None) => break,
                Err(error) if self.finishing && crate::ipc::is_transport_loss(&error) => break,
                Err(error) => return Err(error),
            };
            match packet.message {
                Message::SoftwareResult {
                    fence,
                    producer,
                    sequence,
                    accepted,
                } => {
                    let receipt = self.software.complete(
                        fence,
                        producer,
                        sequence,
                        accepted,
                        monotonic()?,
                    )?;
                    println!("{SOFTWARE}{}", serde_json::to_string(&receipt)?);
                }
                Message::KeyDrain { request } => self.acknowledge_drain(request)?,
                Message::Configuration {
                    generation: 1,
                    error: None,
                } => {}
                Message::Transport { descriptor, .. } => {
                    if let Some(endpoint) = self.endpoints.get(&descriptor.source).cloned() {
                        if endpoint.descriptor != *descriptor {
                            return Err(invalid(
                                "managed transport offer changed its exact publication",
                            ));
                        }
                        self.transport(&endpoint)?;
                    } else if !self.endpoints.is_empty() {
                        return Err(invalid("managed transport offer has an unknown source"));
                    }
                }
                Message::Released {
                    worker,
                    fence,
                    source,
                } => {
                    if worker != self.supervisor.worker_identity()
                        || fence != self.fence
                        || !self.released.insert(source)
                    {
                        return Err(invalid("managed release is stale or duplicated"));
                    }
                }
                Message::Outcome {
                    report: Report::Dispatch { event, .. },
                } if matches!(
                    *event,
                    crate::rules::Event::Matched {
                        action: Action::Remap { .. },
                        ..
                    }
                ) =>
                {
                    return Err(invalid(
                        "managed key work leaked into external action dispatch",
                    ));
                }
                Message::Outcome { .. } => {}
                _ => return Err(invalid("unexpected managed worker control response")),
            }
        }
        if !self.finishing && self.renewed.elapsed() >= Duration::from_millis(30) {
            self.renew()?;
        }
        if !self.finishing {
            self.capture()?;
        }
        Ok(())
    }
    fn notice(&mut self) -> io::Result<Notice> {
        let deadline = Instant::now() + WAIT;
        loop {
            self.tick()?;
            if let Some(packet) = self.fixture.receive::<Notice>()? {
                return Ok(packet.message);
            }
            if Instant::now() >= deadline {
                return Err(invalid("managed worker notice timed out"));
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn step(&mut self, frames: usize) -> io::Result<()> {
        let id = self.step + 1;
        control::send(&mut self.fixture, &Request::Step { id, frames })?;
        if !matches!(self.notice()?, Notice::Stepped { id: acknowledged } if acknowledged == id) {
            return Err(invalid("managed step acknowledgement differs"));
        }
        self.step = id;
        Ok(())
    }
    fn collect(
        &mut self,
        expected: &BTreeMap<Source, Vec<(u16, i32)>>,
    ) -> io::Result<BTreeMap<Source, Vec<Frame>>> {
        let deadline = Instant::now() + Duration::from_millis(250);
        let mut frames: BTreeMap<Source, Vec<Frame>> = self
            .receivers
            .keys()
            .map(|source| (source.clone(), vec![]))
            .collect();
        let mut quiet = Instant::now();
        loop {
            self.tick()?;
            for (source, buffered) in &mut self.captured {
                if !buffered.is_empty() {
                    frames.get_mut(source).unwrap().append(buffered);
                    quiet = Instant::now();
                }
            }
            let actual: BTreeMap<_, _> = frames
                .iter()
                .map(|(source, frames)| (source.clone(), edges(frames)))
                .collect();
            if actual.iter().any(|(source, events)| {
                !expected
                    .get(source)
                    .is_some_and(|wanted| wanted.starts_with(events))
            }) {
                return Err(invalid(format!(
                    "managed native output differs: {actual:?}"
                )));
            }
            if &actual == expected && quiet.elapsed() >= Duration::from_millis(10) {
                return Ok(frames);
            }
            if Instant::now() >= deadline {
                return Err(invalid(format!(
                    "managed native output timed out: {actual:?}, expected {expected:?}"
                )));
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}
impl Drop for Parent {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.supervisor.request_stop();
            match self
                .supervisor
                .wait()
                .and_then(|report| Registry::system()?.recover_worker(&report))
            {
                Ok(_) => {}
                Err(error) => eprintln!("managed fixture worker cleanup failed: {error}"),
            }
        }
    }
}

fn edges(frames: &[Frame]) -> Vec<(u16, i32)> {
    frames
        .iter()
        .flat_map(|frame| &frame.events)
        .filter(|event| event.kind == EV_KEY)
        .map(|event| (event.code, event.value))
        .collect()
}
fn log_phase(
    name: &str,
    frames: &BTreeMap<Source, Vec<Frame>>,
    sources: &[Source],
    generated: &Source,
) {
    let times = |source: &Source| {
        frames[source]
            .iter()
            .map(|frame| frame.at.as_nanos())
            .collect::<Vec<_>>()
    };
    println!(
        "{PHASE}{}",
        serde_json::json!({"phase":name,"raw0":edges(&frames[&sources[0]]),"raw1":edges(&frames[&sources[1]]),"generated":edges(&frames[generated]),"raw0_times_ns":times(&sources[0]),"raw1_times_ns":times(&sources[1]),"generated_times_ns":times(generated)})
    );
}

fn owned_metadata(endpoint: &Endpoint, worker: WorkerIdentity) -> io::Result<()> {
    let marker = crate::transport::Marker::parse(&endpoint.descriptor.marker).map_err(invalid)?;
    let file =
        Path::new("/run/prek/devices").join(format!("{}.json", marker.base.replace('/', "-")));
    let metadata: serde_json::Value = serde_json::from_slice(&fs::read(file)?)?;
    if serde_json::from_value::<WorkerIdentity>(metadata["owner"].clone())? != worker {
        return Err(invalid(
            "managed output metadata is not bound to its actual worker",
        ));
    }
    Ok(())
}
fn input_endpoint(device: &Virtual) -> io::Result<Endpoint> {
    let deadline = Instant::now() + WAIT;
    loop {
        if device.processed_metadata()? {
            let nodes = device.event_nodes()?;
            if nodes.len() == 1 {
                return Ok(Endpoint {
                    path: nodes[0].clone(),
                    descriptor: device.descriptor().clone(),
                });
            }
        }
        if Instant::now() >= deadline {
            return Err(invalid("managed fixture producer metadata timed out"));
        }
        thread::sleep(Duration::from_millis(1));
    }
}

pub(super) fn run() -> io::Result<()> {
    let registry = kernel::authorized();
    let mut devices = vec![kernel::fixture(false), kernel::fixture(false)];
    for device in &mut devices {
        device.capabilities.insert(EV_KEY, vec![30, 31]);
    }
    let mut producers: Vec<_> = devices
        .iter()
        .map(|device| {
            let mut producer = device.clone();
            producer.source.device = producer
                .source
                .device
                .checked_add(1 << 32)
                .ok_or_else(|| invalid("managed producer identity exhausted"))?;
            Virtual::create(registry.prepare_isolated(kernel::descriptor(&producer))?)
        })
        .collect::<io::Result<_>>()?;
    let inputs = producers
        .iter()
        .map(input_endpoint)
        .collect::<io::Result<Vec<_>>>()?;
    let producer_nodes: Vec<_> = inputs
        .iter()
        .map(|endpoint| endpoint.path.clone())
        .collect();
    worker::validate_setup(&devices, &inputs)?;
    let consumer = kernel::receiver_identity();
    let fence = Fence {
        broker: Epoch::random()?,
        configuration: 1,
        session: 1,
    };
    let (supervisor, channel, fixture) = control::spawn()?;
    let mut parent = Parent {
        supervisor,
        channel,
        fixture,
        consumer: consumer.clone(),
        fence,
        revision: 0,
        synthetic: true,
        renewed: Instant::now(),
        endpoints: BTreeMap::new(),
        receivers: BTreeMap::new(),
        captured: BTreeMap::new(),
        software: software::Control::default(),
        step: 0,
        neutral_drains: BTreeMap::new(),
        released: BTreeSet::new(),
        finishing: false,
        reaped: false,
    };
    control::send(
        &mut parent.fixture,
        &Request::Setup {
            devices: devices.clone(),
            inputs,
            consumer,
            fence,
        },
    )?;
    parent.renew()?;
    let Notice::Prepared { worker, outputs } = parent.notice()? else {
        return Err(invalid("managed worker did not prepare outputs"));
    };
    if worker != parent.supervisor.worker_identity() || outputs.len() != 3 {
        return Err(invalid("managed output count or worker binding differs"));
    }
    let mut generated = None;
    let mut removed_nodes = Vec::new();
    for endpoint in outputs {
        endpoint.validate()?;
        owned_metadata(&endpoint, worker)?;
        if endpoint.descriptor.is_generated_keyboard() {
            if generated
                .replace(endpoint.descriptor.source.clone())
                .is_some()
            {
                return Err(invalid("managed fixture has multiple generated keyboards"));
            }
        } else if !devices
            .iter()
            .any(|device| device.source == endpoint.descriptor.source)
        {
            return Err(invalid("managed raw output is not an enrolled source"));
        }
        let receiver = endpoint.receiver()?;
        let source = endpoint.descriptor.source.clone();
        removed_nodes.push(endpoint.path.clone());
        if parent.receivers.insert(source.clone(), receiver).is_some()
            || parent.endpoints.insert(source, endpoint).is_some()
        {
            return Err(invalid("managed output source is duplicated"));
        }
    }
    let generated = generated.ok_or_else(|| invalid("managed generated keyboard missing"))?;
    parent.renew()?;
    control::send(&mut parent.fixture, &Request::Arm {})?;
    if !matches!(parent.notice()?, Notice::Armed {}) {
        return Err(invalid("managed worker did not arm"));
    }
    let sources: Vec<_> = devices.iter().map(|device| device.source.clone()).collect();
    let expected = |raw0: Vec<(u16, i32)>, raw1: Vec<(u16, i32)>, keys: Vec<(u16, i32)>| {
        BTreeMap::from([
            (sources[0].clone(), raw0),
            (sources[1].clone(), raw1),
            (generated.clone(), keys),
        ])
    };
    parent.software_phases(&sources, &generated)?;
    kernel::send(
        &mut producers[0],
        1,
        &[
            (EV_KEY, 30, 1),
            (EV_KEY, 30, 2),
            (EV_KEY, 31, 1),
            (EV_KEY, 30, 0),
            (EV_KEY, 31, 0),
        ],
    );
    parent.step(1)?;
    let mixed = parent.collect(&expected(
        vec![(31, 1), (31, 0)],
        vec![],
        vec![(42, 1), (42, 2), (42, 0)],
    ))?;
    let keys = &mixed[&generated];
    let raw = &mixed[&sources[0]];
    if keys.len() != 3
        || raw.len() != 2
        || !(keys[0].at <= keys[1].at
            && keys[1].at < raw[0].at
            && raw[0].at < keys[2].at
            && keys[2].at < raw[1].at)
    {
        return Err(invalid("managed native within-report write order differs"));
    }
    log_phase("mixed", &mixed, &sources, &generated);
    kernel::send(&mut producers[0], 2, &[(EV_KEY, 30, 1)]);
    kernel::send(&mut producers[1], 1, &[(EV_KEY, 31, 1), (EV_KEY, 31, 0)]);
    parent.step(2)?;
    let cross = parent.collect(&expected(vec![], vec![(31, 1), (31, 0)], vec![(42, 1)]))?;
    if cross[&generated][0].at >= cross[&sources[1]][0].at
        || parent.receivers[&generated].keys() != [42]
    {
        return Err(invalid(
            "managed native modifier did not precede the second source",
        ));
    }
    log_phase("cross_source", &cross, &sources, &generated);
    parent.software_physical(&sources, &generated)?;
    parent.synthetic = false;
    parent.renew()?;
    kernel::send(
        &mut producers[0],
        3,
        &[
            (EV_KEY, 30, 2),
            (EV_KEY, 30, 0),
            (EV_KEY, 31, 1),
            (EV_KEY, 31, 0),
        ],
    );
    parent.step(1)?;
    let cancelled = parent.collect(&expected(vec![(31, 1), (31, 0)], vec![], vec![(42, 0)]))?;
    if cancelled[&generated][0].at >= cancelled[&sources[0]][0].at {
        return Err(invalid(
            "managed generated cleanup did not precede ordinary reentry",
        ));
    }
    log_phase("revocation", &cancelled, &sources, &generated);
    if parent.neutral_drains.len() != 1
        || !parent.neutral_drains.contains_key(&generated)
        || parent.receivers.iter().any(|(source, receiver)| {
            !parent.neutral_drains.contains_key(source) && !receiver.keys().is_empty()
        })
    {
        return Err(invalid("managed native receivers did not return neutral"));
    }
    parent.software.finish()?;
    parent.finishing = true;
    control::send(&mut parent.fixture, &Request::Finish {})?;
    if !matches!(parent.notice()?, Notice::Closed {}) {
        return Err(invalid("managed worker did not close"));
    }
    parent.tick()?;
    let report = parent.supervisor.wait()?;
    if !report.belongs_to(&parent.supervisor)
        || !report.status().success()
        || report.forced()
        || report.heartbeats() == 0
    {
        return Err(invalid(
            "managed worker did not exit successfully under supervision",
        ));
    }
    let recovered = registry.recover_worker(&report)?;
    parent.reaped = true;
    if recovered != 0 || parent.released != sources.iter().cloned().collect() {
        return Err(invalid(
            "managed worker did not retire every output before source release",
        ));
    }
    let deadline = Instant::now() + WAIT;
    while removed_nodes.iter().any(|path| path.exists()) {
        if Instant::now() >= deadline {
            return Err(invalid("managed owned output nodes remain"));
        }
        thread::sleep(Duration::from_millis(1));
    }
    drop(parent);
    drop(producers);
    let deadline = Instant::now() + WAIT;
    while producer_nodes.iter().any(|path| path.exists()) {
        if Instant::now() >= deadline {
            return Err(invalid("managed owned producer nodes remain"));
        }
        thread::sleep(Duration::from_millis(1));
    }
    println!(
        "{RESULT}{}",
        serde_json::json!({"version":3,"status":"passed","uid":0,"physical_devices_opened":0,"worker_binding_tested":true,"private_control_authenticated":true,"consumer_neutral_drain_tested":true,"consumer_neutral_drain_acknowledgements":1,"raw_outputs":2,"generated_outputs":1,"source_readers_grabbed":2,"native_projection_tested":true,"native_repeat_tested":true,"native_cancellation_tested":true,"kernel_write_order_tested":true,"software_producer_control_tested":true,"software_producer_expiry_tested":true,"software_producer_rejection_tested":true,"software_physical_ownership_tested":true,"software_request_acknowledgements":21,"software_rejections":5,"software_public_authorization_tested":false,"native_partial_write_tested":false,"broker_grant_tested":false,"context_authority_modeled":true,"production_input_loop_tested":false,"production_route_admission_tested":false,"compositor_delivery_tested":false,"keys_neutral":true,"worker_reaped":true,"owned_devices_removed":true})
    );
    Ok(())
}
