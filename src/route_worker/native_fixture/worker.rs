use super::*;

struct Owned {
    stream: Stream,
    readers: Vec<kernel::Receiver>,
}
impl Drop for Owned {
    fn drop(&mut self) {
        self.stream.shutdown();
    }
}

fn endpoint(device: &Virtual) -> io::Result<Endpoint> {
    let nodes = device.event_nodes()?;
    if nodes.len() != 1 {
        return Err(invalid("managed output has ambiguous event nodes"));
    }
    let endpoint = Endpoint {
        path: nodes[0].clone(),
        descriptor: device.descriptor().clone(),
    };
    endpoint.validate()?;
    Ok(endpoint)
}

pub(super) fn validate_setup(devices: &[Device], inputs: &[Endpoint]) -> io::Result<()> {
    configuration(devices)?;
    if inputs.len() != 2
        || devices[0].source.device == devices[1].source.device
        || inputs[0].path == inputs[1].path
        || inputs[0].descriptor.source.device == inputs[1].descriptor.source.device
    {
        return Err(invalid("managed fixture source identities differ"));
    }
    for (device, endpoint) in devices.iter().zip(inputs) {
        endpoint.validate()?;
        let mut producer = device.clone();
        producer.source = endpoint.descriptor.source.clone();
        if device.source.seat != "seat0"
            || device.source.generation == 0
            || endpoint.descriptor.source.seat != device.source.seat
            || devices
                .iter()
                .any(|device| endpoint.descriptor.source.device == device.source.device)
            || endpoint.descriptor.fixture_source() != Some(&producer)
            || !endpoint.descriptor.isolated_fixture()
        {
            return Err(invalid(
                "managed fixture source is not the exact owned test descriptor",
            ));
        }
    }
    Ok(())
}

fn pump(stream: &mut Stream, health: &mut Heartbeat, sources: &[Source]) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
        control::live(stream, health)?;
        stream.flush()?;
        let mut pending = false;
        for source in sources {
            let seat = stream
                .seats
                .get_mut(&source.seat)
                .ok_or_else(|| invalid("managed seat disappeared"))?;
            if !seat.routing.has_source(source) {
                return Err(invalid("managed route recovered unexpectedly"));
            }
            pending |= seat
                .routing
                .due_after_input(source, monotonic()?)?
                .is_some();
        }
        let keys = stream.seats["seat0"]
            .keys
            .as_ref()
            .is_some_and(keys::Slot::fixture_settled);
        if !pending
            && keys
            && stream
                .outputs
                .values()
                .all(|output| output.in_flight.is_none())
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "managed native writes did not settle",
            ));
        }
        thread::sleep(Duration::from_millis(1));
    }
}

pub(super) fn run() -> io::Result<()> {
    let mut held = None;
    let result = serve(&mut held);
    if let Err(error) = &result {
        eprintln!("managed worker failed: {error}");
    }
    drop(held);
    result
}

fn serve(held: &mut Option<Owned>) -> io::Result<()> {
    let mut health = Heartbeat::connect()?;
    let channel = health
        .take_control()
        .ok_or_else(|| invalid("managed worker has no authenticated control"))?;
    let mut fixture = control::child(&health, &channel)?;
    let deadline = Instant::now() + WAIT;
    let setup = loop {
        health.progress()?;
        if let Some(packet) = fixture.receive::<Request>()? {
            break packet.message;
        }
        if Instant::now() >= deadline {
            return Err(invalid("managed setup timed out"));
        }
        thread::sleep(Duration::from_millis(1));
    };
    let Request::Setup {
        devices,
        inputs,
        consumer,
        fence,
    } = setup
    else {
        return Err(invalid("managed worker expected setup"));
    };
    fence.validate()?;
    validate_setup(&devices, &inputs)?;
    let Consumer::IsolatedReceiver { pid, .. } = consumer else {
        return Err(invalid("managed consumer is not isolated"));
    };
    if pid != fixture.peer().pid {
        return Err(invalid("managed consumer differs from its supervisor"));
    }
    let registry = kernel::authorized();
    let Owned { stream, readers } = held.insert(Owned {
        stream: Stream::with_registry(configuration(&devices)?, channel, registry)?,
        readers: Vec::new(),
    });
    let mut physical = Vec::new();
    for device in &devices {
        stream.push(Record::Device {
            device: device.clone(),
            keys: BTreeSet::new(),
            contacts: vec![],
        })?;
        physical.push(Virtual::create(
            stream
                .registry
                .prepare_isolated_for_worker(kernel::descriptor(device), &health)?,
        )?);
    }
    let source = stream.next_key_source("seat0")?;
    let prepared = crate::keys::native::Prepared::isolated_for_worker(
        &stream.registry,
        &health,
        source,
        consumer.clone(),
    )?;
    let deadline = Instant::now() + WAIT;
    let mut outputs;
    loop {
        health.progress()?;
        let nodes = prepared.fixture_nodes()?;
        if nodes.as_ref().is_some_and(|nodes| nodes.len() == 1)
            && physical
                .iter()
                .map(Virtual::processed_metadata)
                .collect::<io::Result<Vec<_>>>()?
                .iter()
                .all(|ready| *ready)
        {
            outputs = physical
                .iter()
                .map(endpoint)
                .collect::<io::Result<Vec<_>>>()?;
            outputs.push(Endpoint {
                path: nodes.unwrap()[0].clone(),
                descriptor: prepared.descriptor().clone(),
            });
            break;
        }
        if Instant::now() >= deadline {
            return Err(invalid("managed owned output metadata timed out"));
        }
        thread::sleep(Duration::from_millis(1));
    }
    control::send(
        &mut fixture,
        &Notice::Prepared {
            worker: health.worker_identity(),
            outputs,
        },
    )?;
    for (device, output) in devices.iter().zip(physical) {
        let now = monotonic()?;
        stream
            .releases
            .insert(device.source.clone(), (fence, health.worker_identity()));
        stream.outputs.insert(
            device.source.clone(),
            Output {
                fence,
                admission: crate::morf::admission::Gate::new(
                    health.worker_identity(),
                    fence,
                    device.source.clone(),
                ),
                virtual_device: output,
                state_probe: None,
                feedback: None,
                readiness: None,
                consumer: None,
                offered: Duration::ZERO,
                started: now,
                in_flight: None,
                sequence: 0,
            },
        );
    }
    let deadline = Instant::now() + WAIT;
    while !stream.seats.contains_key("seat0") {
        control::live(stream, &mut health)?;
        if Instant::now() >= deadline {
            return Err(invalid("managed subscription timed out"));
        }
        thread::sleep(Duration::from_millis(1));
    }
    stream.seats.get_mut("seat0").unwrap().keys =
        Some(keys::Slot::new(fence, consumer.clone(), prepared)?);
    if !matches!(
        control::request(&mut fixture, stream, &mut health)?,
        Request::Arm {}
    ) {
        return Err(invalid("managed worker expected arm"));
    }
    for (device, endpoint) in devices.iter().zip(&inputs) {
        let reader = endpoint.receiver_as(&device.source)?;
        if !reader.keys().is_empty() {
            return Err(invalid("managed source is not neutral before enrollment"));
        }
        readers.push(reader);
        let output = stream.outputs.get(&device.source).unwrap();
        let now = monotonic()?;
        let evidence = output
            .consumer
            .as_ref()
            .ok_or_else(|| invalid("managed physical receiver has no evidence"))?;
        if !output.admission.allowed(now) {
            return Err(invalid("managed physical fixture admission is not ready"));
        }
        let mut gate = Readiness::new(
            output.virtual_device.descriptor().clone(),
            consumer.clone(),
            now,
        )
        .map_err(invalid)?;
        gate.metadata(output.virtual_device.descriptor(), now)
            .map_err(invalid)?;
        gate.consumer(evidence, now).map_err(invalid)?;
        gate.neutral(true, now).map_err(invalid)?;
        stream.seats.get_mut("seat0").unwrap().routing.attach(
            device.clone(),
            &gate.finish(now).map_err(invalid)?,
            None,
            now,
        )?;
        stream.grabbed.insert(device.source.clone());
    }
    let sources: Vec<_> = devices.iter().map(|device| device.source.clone()).collect();
    pump(stream, &mut health, &sources)?;
    control::send(&mut fixture, &Notice::Armed {})?;
    let mut step = 0;
    loop {
        match control::request(&mut fixture, stream, &mut health)? {
            Request::Step { id, frames } if id == step + 1 && frames <= 4 => {
                let deadline = Instant::now() + Duration::from_millis(200);
                let mut batch = Vec::new();
                while batch.len() < frames {
                    control::live(stream, &mut health)?;
                    for reader in readers.iter_mut() {
                        if let Some(frame) = reader.next(Duration::from_millis(1)) {
                            batch.push(frame);
                        }
                    }
                    if batch.len() > frames {
                        return Err(invalid("managed source produced unexpected frames"));
                    }
                    if Instant::now() >= deadline {
                        return Err(invalid("managed source read timed out"));
                    }
                }
                batch.sort_by(|a, b| {
                    (a.at, &a.source, a.sequence).cmp(&(b.at, &b.source, b.sequence))
                });
                for frame in &batch {
                    stream.push(Record::Frame {
                        frame: frame.clone(),
                    })?;
                }
                pump(stream, &mut health, &sources)?;
                step = id;
                control::send(&mut fixture, &Notice::Stepped { id, inputs: batch })?;
            }
            Request::Idle { id } if id == step + 1 => {
                let until = Instant::now() + Duration::from_millis(120);
                while Instant::now() < until {
                    for reader in readers.iter_mut() {
                        if !reader.drain_complete()?.is_empty() {
                            return Err(invalid("managed idle advanced before input was drained"));
                        }
                    }
                    stream.push(Record::Tick { at: monotonic()? })?;
                    pump(stream, &mut health, &sources)?;
                    thread::sleep(Duration::from_millis(1));
                }
                step = id;
                control::send(&mut fixture, &Notice::Stepped { id, inputs: vec![] })?;
            }
            Request::Finish {} if step == 12 => {
                stream.shutdown();
                for (reader, source) in readers.drain(..).zip(&sources) {
                    drop(reader);
                    stream.reader_released(source)?;
                }
                control::send(&mut fixture, &Notice::Closed {})?;
                return Ok(());
            }
            _ => return Err(invalid("managed fixture command is out of order")),
        }
    }
}
