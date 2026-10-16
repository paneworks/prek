//! Agent-side custom callback transport. Admission belongs to the broker.
use super::Session;
use crate::{
    ipc::{Channel, monotonic},
    protocol::{Fence, Request},
    recognition::{Cancellation, MAX_PENDING, Ticket},
};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
};

#[derive(Default)]
pub struct RecognizerBridge {
    fence: Option<Fence>,
    sent: Option<bool>,
    acknowledged: bool,
    requests: VecDeque<Request>,
    cleanup: BTreeMap<u64, Cancellation>,
    queued: BTreeMap<u64, u64>,
}
impl RecognizerBridge {
    fn scope(&mut self, fence: Fence) {
        if self.fence != Some(fence) {
            *self = Self {
                fence: Some(fence),
                ..Self::default()
            };
        }
    }
    pub fn acknowledge_ready(&mut self, fence: Fence, ready: bool) {
        self.scope(fence);
        self.acknowledged = ready;
    }
    /// An inner helper failure retires that VM; malformed broker identity fails
    /// the outer result. Neither creates a claim or an execution grant.
    pub fn recognize(
        &mut self,
        fence: Fence,
        ticket: Ticket,
        seat: &str,
        runtime: &mut Option<Session>,
    ) -> io::Result<Option<String>> {
        self.scope(fence);
        if !self.acknowledged
            || ticket.fence != fence
            || ticket.input.source().seat != seat
            || self.queued.len() >= MAX_PENDING
        {
            return Err(io::Error::other(
                "recognizer ticket preceded readiness or belongs to another seat",
            ));
        }
        let identity = (ticket.id, ticket.candidate);
        let result = runtime
            .as_mut()
            .ok_or_else(|| io::Error::other("recognizer helper is absent"))
            .and_then(|runtime| runtime.request_recognition(ticket));
        if result.is_ok() {
            self.queued.insert(identity.0, identity.1);
        }
        Ok(result.err().map(|error| {
            *runtime = None;
            error.to_string()
        }))
    }
    pub fn cancel(
        &mut self,
        fence: Fence,
        cancellation: Cancellation,
        runtime: &mut Option<Session>,
    ) -> io::Result<Option<String>> {
        self.scope(fence);
        cancellation.validate()?;
        if cancellation.fence != fence
            || self.cleanup.len() >= MAX_PENDING
            || self
                .cleanup
                .insert(cancellation.candidate, cancellation.clone())
                .is_some()
        {
            return Err(io::Error::other(
                "foreign, repeated or excessive recognizer cleanup",
            ));
        }
        self.requests.retain(|request| !matches!(request, Request::Recognize { proposal, .. } if self.queued.get(&proposal.ticket) == Some(&cancellation.candidate)));
        self.queued
            .retain(|_, candidate| *candidate != cancellation.candidate);
        let result = runtime
            .as_mut()
            .map(|runtime| runtime.cancel_recognition(cancellation))
            .transpose();
        Ok(result.err().map(|error| {
            *runtime = None;
            error.to_string()
        }))
    }
    /// Return false when a missing proposal requires retiring the helper.
    pub fn collect(&mut self, runtime: &mut Session) -> io::Result<bool> {
        let mut valid = true;
        for (id, proposal) in runtime.take_recognitions() {
            if let Some(proposal) = proposal {
                if !self.queued.contains_key(&id) || proposal.ticket != id {
                    return Err(io::Error::other("unissued recognizer helper proposal"));
                }
                self.requests.push_back(Request::Recognize {
                    fence: proposal.fence,
                    proposal,
                });
            } else {
                self.queued.remove(&id);
                valid = false;
            }
        }
        for cancellation in runtime.take_recognition_cancellations() {
            if self.cleanup.remove(&cancellation.candidate).as_ref() != Some(&cancellation) {
                return Err(io::Error::other(
                    "recognizer helper cleanup acknowledgement differs",
                ));
            }
            self.requests.push_back(Request::RecognitionCancelled {
                fence: cancellation.fence,
                cancellation,
            });
        }
        if self.requests.len() > MAX_PENDING {
            return Err(io::Error::other("recognizer control queue exceeded limit"));
        }
        Ok(valid)
    }
    fn ready(subscribed: bool, runtime: Option<&Session>) -> bool {
        subscribed && runtime.is_some_and(|runtime| runtime.action_ready())
    }
    pub fn flush(
        &mut self,
        fence: Fence,
        subscribed: bool,
        runtime: Option<&Session>,
        channel: &mut Channel,
    ) -> io::Result<()> {
        self.scope(fence);
        self.requests
            .retain(|request| request.fence() == Some(fence));
        if runtime.is_none() {
            self.queued.clear();
            for (_, cancellation) in std::mem::take(&mut self.cleanup) {
                self.requests.push_back(Request::RecognitionCancelled {
                    fence,
                    cancellation,
                });
            }
        }
        if self.requests.len() > MAX_PENDING {
            return Err(io::Error::other("recognizer control queue exceeded limit"));
        }
        let ready = Self::ready(subscribed, runtime);
        if !ready {
            self.requests
                .retain(|request| !matches!(request, Request::Recognize { .. }));
        }
        if self.sent != Some(ready)
            && channel.send(&Request::RecognizerReady { fence, ready }, monotonic()?)?
        {
            self.sent = Some(ready);
        }
        for _ in 0..8 {
            let Some(request) = self.requests.front() else {
                break;
            };
            if !channel.send(request, monotonic()?)? {
                break;
            }
            if let Request::Recognize { proposal, .. } = request {
                self.queued.remove(&proposal.ticket);
            }
            self.requests.pop_front();
        }
        Ok(())
    }
    pub fn wants_write(&self, fence: Fence, subscribed: bool, runtime: Option<&Session>) -> bool {
        self.fence != Some(fence)
            || self.sent != Some(Self::ready(subscribed, runtime))
            || !self.requests.is_empty()
            || runtime.is_none() && !self.cleanup.is_empty()
    }
}
