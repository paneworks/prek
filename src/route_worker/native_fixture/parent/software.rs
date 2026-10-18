use super::*;
use crate::keys::producer::{Operation as Op, Request as SoftwareRequest};

#[derive(Default)]
pub(super) struct Control {
    pending: Option<(SoftwareRequest, bool, Duration)>,
    completed: usize,
    rejected: usize,
}
#[derive(Serialize)]
pub(super) struct Receipt {
    version: u8,
    request: SoftwareRequest,
    accepted: bool,
    sent_at_ns: u128,
    received_at_ns: u128,
}
impl Control {
    fn begin(&mut self, request: SoftwareRequest, accepted: bool, at: Duration) -> io::Result<()> {
        if self.pending.is_some() || self.completed >= 21 {
            return Err(invalid("managed software request is pending or exhausted"));
        }
        self.pending = Some((request, accepted, at));
        Ok(())
    }
    pub(super) fn complete(
        &mut self,
        fence: Fence,
        producer: Epoch,
        sequence: u64,
        accepted: bool,
        at: Duration,
    ) -> io::Result<Receipt> {
        let Some((request, wanted, sent)) = &self.pending else {
            return Err(invalid("unsolicited managed software admission"));
        };
        if request.fence != fence
            || request.producer != producer
            || request.sequence != sequence
            || *wanted != accepted
            || at < *sent
            || at - *sent >= Duration::from_millis(250)
        {
            return Err(invalid("managed software admission correlation differs"));
        }
        let (request, _, sent) = self.pending.take().unwrap();
        self.completed += 1;
        self.rejected += usize::from(!accepted);
        Ok(Receipt {
            version: 1,
            request,
            accepted,
            sent_at_ns: sent.as_nanos(),
            received_at_ns: at.as_nanos(),
        })
    }
    pub(super) fn finish(&self) -> io::Result<()> {
        if self.pending.is_some() || self.completed != 21 || self.rejected != 5 {
            return Err(invalid("managed software admissions are incomplete"));
        }
        Ok(())
    }
}
impl Parent {
    fn software_request(
        &self,
        generated: &Source,
        producer: u64,
        sequence: u64,
        operation: Op,
    ) -> SoftwareRequest {
        SoftwareRequest {
            fence: self.fence,
            source: generated.clone(),
            marker: self.endpoints[generated].descriptor.marker.clone(),
            consumer: self.consumer.clone(),
            producer: Epoch([producer, 7]),
            sequence,
            operation,
        }
    }
    fn software_admit(&mut self, request: SoftwareRequest, accepted: bool) -> io::Result<()> {
        let at = monotonic()?;
        self.software.begin(request.clone(), accepted, at)?;
        control::send(
            &mut self.channel,
            &Command::Software {
                request: Box::new(request),
            },
        )?;
        while self.software.pending.is_some() {
            self.tick()?;
            if monotonic()? - at >= Duration::from_millis(250) {
                return Err(invalid("managed software admission timed out"));
            }
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }
    fn software_send(
        &mut self,
        generated: &Source,
        producer: u64,
        sequence: u64,
        operation: Op,
        accepted: bool,
    ) -> io::Result<()> {
        self.software_admit(
            self.software_request(generated, producer, sequence, operation),
            accepted,
        )
    }
    fn software_collect(
        &mut self,
        name: &str,
        sources: &[Source],
        generated: &Source,
        keys: Vec<(u16, i32)>,
        held: &[u16],
    ) -> io::Result<Vec<Frame>> {
        self.step(0)?;
        let expected = BTreeMap::from([
            (sources[0].clone(), vec![]),
            (sources[1].clone(), vec![]),
            (generated.clone(), keys),
        ]);
        let frames = self.collect(&expected)?;
        if self.receivers[generated].keys() != held {
            return Err(invalid("managed software kernel held state differs"));
        }
        log_phase(name, &frames, sources, generated);
        Ok(frames[generated].clone())
    }
    pub(super) fn software_phases(
        &mut self,
        sources: &[Source],
        generated: &Source,
    ) -> io::Result<()> {
        self.renew()?;
        let open = Op::Open {
            expires: monotonic()? + Duration::from_millis(200),
        };
        let mut forged = self.software_request(generated, 1, 1, open.clone());
        let Consumer::IsolatedReceiver { pid, start } = self.consumer else {
            unreachable!()
        };
        forged.consumer = Consumer::IsolatedReceiver {
            pid: pid
                .checked_add(1)
                .ok_or_else(|| invalid("fixture consumer PID exhausted"))?,
            start,
        };
        self.software_admit(forged, false)?;
        self.software_send(generated, 1, 1, open, true)?;
        let mut alias = self.software_request(generated, 1, 2, Op::Press { id: 1, key: 42 });
        alias.source = sources[0].clone();
        self.software_admit(alias, false)?;
        self.software_send(generated, 1, 2, Op::Press { id: 1, key: 42 }, true)?;
        self.software_send(generated, 1, 3, Op::Repeat { id: 1 }, true)?;
        self.software_send(generated, 1, 4, Op::Release { id: 1 }, true)?;
        self.software_collect(
            "software_fast",
            sources,
            generated,
            vec![(42, 1), (42, 2), (42, 0)],
            &[],
        )?;
        self.software_send(generated, 1, 5, Op::Close {}, true)?;
        self.software_send(
            generated,
            1,
            1,
            Op::Open {
                expires: monotonic()? + Duration::from_millis(200),
            },
            false,
        )?;
        self.renew()?;
        for producer in [2, 3] {
            self.software_send(
                generated,
                producer,
                1,
                Op::Open {
                    expires: monotonic()? + Duration::from_millis(200),
                },
                true,
            )?;
            self.software_send(generated, producer, 2, Op::Press { id: 1, key: 42 }, true)?;
        }
        self.software_collect("software_overlap", sources, generated, vec![(42, 1)], &[42])?;
        self.software_send(generated, 2, 3, Op::Close {}, true)?;
        self.software_send(generated, 3, 3, Op::Repeat { id: 1 }, true)?;
        let expires = monotonic()? + Duration::from_millis(100);
        self.software_send(generated, 3, 4, Op::Renew { expires }, true)?;
        self.software_collect("software_sibling", sources, generated, vec![(42, 2)], &[42])?;
        let expected = BTreeMap::from([
            (sources[0].clone(), vec![]),
            (sources[1].clone(), vec![]),
            (generated.clone(), vec![(42, 0)]),
        ]);
        let expired = self.collect(&expected)?;
        if expired[generated][0]
            .at
            .saturating_add(Duration::from_nanos(999))
            < expires
            || !self.receivers[generated].keys().is_empty()
        {
            return Err(invalid(
                "managed software expired before its lease or stayed held",
            ));
        }
        self.step(0)?;
        log_phase("software_expiry", &expired, sources, generated);
        self.software_send(
            generated,
            3,
            5,
            Op::Renew {
                expires: monotonic()? + Duration::from_millis(200),
            },
            false,
        )?;
        self.software_send(
            generated,
            3,
            1,
            Op::Open {
                expires: monotonic()? + Duration::from_millis(200),
            },
            false,
        )?;
        Ok(())
    }
    pub(super) fn software_physical(
        &mut self,
        sources: &[Source],
        generated: &Source,
    ) -> io::Result<()> {
        self.renew()?;
        self.software_send(
            generated,
            4,
            1,
            Op::Open {
                expires: monotonic()? + Duration::from_millis(200),
            },
            true,
        )?;
        self.software_send(generated, 4, 2, Op::Press { id: 1, key: 42 }, true)?;
        self.software_send(generated, 4, 3, Op::Repeat { id: 1 }, true)?;
        self.step(0)?;
        self.software_send(generated, 4, 4, Op::Close {}, true)?;
        self.software_collect(
            "software_physical",
            sources,
            generated,
            vec![(42, 2)],
            &[42],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> SoftwareRequest {
        SoftwareRequest {
            fence: Fence {
                broker: Epoch([1, 2]),
                configuration: 1,
                session: 1,
            },
            source: Source {
                seat: "seat0".into(),
                device: 100,
                generation: 1,
            },
            marker: "model".into(),
            consumer: Consumer::IsolatedReceiver { pid: 1, start: 1 },
            producer: Epoch([1, 7]),
            sequence: 1,
            operation: Op::Open {
                expires: Duration::from_millis(200),
            },
        }
    }
    #[test]
    fn software_admission_ledger_rejects_unsolicited_duplicate_and_mismatched_receipts() {
        let request = request();
        let mut ledger = Control::default();
        let at = Duration::from_millis(1);
        assert!(
            ledger
                .complete(request.fence, request.producer, 1, true, at)
                .is_err()
        );
        ledger.begin(request.clone(), true, at).unwrap();
        assert!(ledger.begin(request.clone(), true, at).is_err());
        for (fence, producer, sequence, accepted, at) in [
            (
                Fence {
                    session: 2,
                    ..request.fence
                },
                request.producer,
                1,
                true,
                at,
            ),
            (request.fence, Epoch([2, 7]), 1, true, at),
            (request.fence, request.producer, 2, true, at),
            (request.fence, request.producer, 1, false, at),
            (request.fence, request.producer, 1, true, Duration::ZERO),
            (
                request.fence,
                request.producer,
                1,
                true,
                Duration::from_millis(251),
            ),
        ] {
            assert!(
                ledger
                    .complete(fence, producer, sequence, accepted, at)
                    .is_err()
            );
            assert_eq!(ledger.completed, 0);
        }
        let receipt = ledger
            .complete(request.fence, request.producer, 1, true, at)
            .unwrap();
        assert_eq!(receipt.request, request);
        assert_eq!(ledger.completed, 1);
        assert!(
            ledger
                .complete(request.fence, request.producer, 1, true, at)
                .is_err()
        );
        assert!(ledger.finish().is_err());
    }
    #[test]
    fn software_ledger_requires_all_twenty_one_receipts_and_five_rejections() {
        let request = request();
        let mut ledger = Control::default();
        for index in 0..21 {
            assert!(ledger.finish().is_err());
            let accepted = index >= 5;
            ledger
                .begin(request.clone(), accepted, Duration::ZERO)
                .unwrap();
            ledger
                .complete(
                    request.fence,
                    request.producer,
                    1,
                    accepted,
                    Duration::from_millis(1),
                )
                .unwrap();
        }
        ledger.finish().unwrap();
        assert!(ledger.begin(request, true, Duration::ZERO).is_err());
        ledger.rejected = 4;
        assert!(ledger.finish().is_err());
    }
}
