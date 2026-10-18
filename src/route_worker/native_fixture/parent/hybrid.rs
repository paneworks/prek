use super::*;

fn events(values: &[(u16, u16, i32)]) -> Vec<RawEvent> {
    values
        .iter()
        .map(|&(kind, code, value)| RawEvent { kind, code, value })
        .chain([RawEvent {
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        }])
        .collect()
}

impl Parent {
    pub(super) fn hybrid(
        &mut self,
        producer: &mut Virtual,
        sources: &[Source],
        generated: &Source,
        mixed_input: &[Frame],
        mixed: &BTreeMap<Source, Vec<Frame>>,
    ) -> io::Result<()> {
        let source = &sources[0];
        let expected_input = events(&[
            (EV_REL, 0, 7),
            (EV_KEY, 30, 1),
            (EV_KEY, 30, 2),
            (EV_KEY, 31, 1),
            (EV_REL, 0, 9),
            (EV_KEY, 30, 0),
            (EV_KEY, 31, 0),
        ]);
        let raw = &mixed[source];
        if mixed_input.len() != 1
            || mixed_input[0].source != *source
            || mixed_input[0].events != expected_input
            || raw.len() != 3
            || raw[0].events != events(&[(EV_KEY, 31, 1)])
            || raw[1].events != events(&[(EV_KEY, 31, 0)])
            || raw[2].events != events(&[(EV_REL, 0, 7), (EV_REL, 0, 9)])
            || raw[1].at > raw[2].at
        {
            return Err(invalid(
                "managed hybrid consumption split or changed its coherent tail",
            ));
        }
        kernel::send(producer, 2, &[(EV_REL, 0, 7), (EV_KEY, 32, 1)]);
        self.step(1)?;
        let mut inputs = self.last_inputs.clone();
        let until = Instant::now() + Duration::from_millis(80);
        while Instant::now() < until {
            self.tick()?;
            if self.captured.values().any(|frames| !frames.is_empty()) {
                return Err(invalid("managed hybrid tap escaped before fallback"));
            }
            thread::sleep(Duration::from_millis(1));
        }
        kernel::send(producer, 3, &[(EV_REL, 0, 9), (EV_KEY, 32, 0)]);
        self.step(1)?;
        inputs.extend(self.last_inputs.iter().cloned());
        let id = self.step + 1;
        control::send(&mut self.fixture, &Request::Idle { id })?;
        self.stepped(id, 0)?;
        let expected = BTreeMap::from([
            (
                source.clone(),
                vec![
                    RawEvent {
                        kind: EV_REL,
                        code: 0,
                        value: 7,
                    },
                    RawEvent {
                        kind: EV_KEY,
                        code: 32,
                        value: 1,
                    },
                    RawEvent {
                        kind: EV_REL,
                        code: 0,
                        value: 9,
                    },
                    RawEvent {
                        kind: EV_KEY,
                        code: 32,
                        value: 0,
                    },
                ],
            ),
            (sources[1].clone(), vec![]),
            (generated.clone(), vec![]),
        ]);
        let fallback = self.collect_events(&expected)?;
        let forwarded = &fallback[source];
        if inputs.len() != 2
            || forwarded.len() != 2
            || inputs.iter().any(|input| input.source != *source)
            || inputs[0].events != events(&[(EV_REL, 0, 7), (EV_KEY, 32, 1)])
            || inputs[1].events != events(&[(EV_REL, 0, 9), (EV_KEY, 32, 0)])
            || inputs
                .iter()
                .zip(forwarded)
                .any(|(input, output)| input.events != output.events)
            || inputs[1].at.saturating_sub(inputs[0].at) < Duration::from_millis(80)
            || forwarded[0].at < inputs[1].at
            || forwarded[1]
                .at
                .saturating_sub(forwarded[0].at)
                .abs_diff(inputs[1].at.saturating_sub(inputs[0].at))
                > Duration::from_millis(20)
            || self
                .receivers
                .values()
                .any(|receiver| !receiver.keys().is_empty())
        {
            return Err(invalid(
                "managed hybrid fallback changed reports, cadence or neutral state",
            ));
        }
        log_phase("hybrid_fallback", &fallback, sources, generated);
        let endpoint = &self.endpoints[source];
        let observed: BTreeMap<_, _> = endpoint
            .descriptor
            .capabilities
            .keys()
            .map(|&kind| {
                (
                    kind,
                    if kind == EV_SYN {
                        vec![SYN_REPORT]
                    } else {
                        kernel::capability_bits(&self.receivers[source].raw, kind)
                    },
                )
            })
            .collect();
        println!(
            "{HYBRID}{}",
            serde_json::json!({
                "version":1, "source":source, "generated_source":generated,
                "counterpart_event_nodes":1, "counterpart":endpoint,
                "observed_capabilities":observed, "mixed_input":mixed_input[0],
                "mixed_raw":raw, "mixed_generated":mixed[generated],
                "fallback_inputs":inputs, "fallback_raw":forwarded,
                "fallback_generated":fallback[generated], "scheduler_slack_ns":20_000_000,
                "physical_devices_opened":0, "compositor_delivery_tested":false, "mt_tested":false,
            })
        );
        Ok(())
    }
}
