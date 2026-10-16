//! Native retained-group tokens and historical ownership receipts.
use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
/// Router-issued identity for one retained physical segment.
/// It contains no recognizer-step, context or execution permission.
pub struct ChainToken {
    source: Source,
    authorization: u64,
    candidate: u64,
    segment: u64,
}
impl ChainToken {
    pub fn source(&self) -> &Source {
        &self.source
    }
    pub fn candidate(&self) -> u64 {
        self.candidate
    }
    pub fn segment(&self) -> u64 {
        self.segment
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChainAdmission {
    Held(ChainToken),
    Forwarded,
}

/// A native claim receipt bound to one source incarnation and authorization.
/// It carries no action, source registration or execution permission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimedChain {
    source: Source,
    authorization: u64,
    candidate: u64,
    first: u64,
    through: u64,
}
impl ClaimedChain {
    pub fn source(&self) -> &Source {
        &self.source
    }
    pub fn candidate(&self) -> u64 {
        self.candidate
    }
    pub fn segments(&self) -> u64 {
        self.through - self.first + 1
    }
}

#[derive(Clone, Copy)]
pub(super) enum Retention<'a> {
    Single(bool),
    Start,
    Continue(&'a ChainToken),
}
impl Retention<'_> {
    pub(super) fn retains(self) -> bool {
        !matches!(self, Self::Single(false))
    }
}

impl Router {
    /// Retain physical groups under one original buffer decision or reserve.
    /// The trusted caller supplies native policy/context admission.
    pub fn start_chain(
        &mut self,
        frame: Frame,
        candidate: Candidate,
        at: Duration,
    ) -> Result<ChainAdmission, Fault> {
        self.push_inner(frame, Some(candidate), at, Retention::Start)?;
        Ok(self.chain_admission())
    }
    /// Append a fresh neutral-to-active group using the latest native token.
    /// An elapsed buffer decision forwards input without renewing the lease.
    pub fn continue_chain(
        &mut self,
        frame: Frame,
        previous: &ChainToken,
        at: Duration,
    ) -> Result<ChainAdmission, Fault> {
        self.push_inner(frame, None, at, Retention::Continue(previous))?;
        Ok(self.chain_admission())
    }
    fn chain_admission(&self) -> ChainAdmission {
        let Some(group) = self.group.as_ref().filter(|group| {
            group.chain
                && group.completion
                && matches!(group.ownership, Ownership::Pending | Ownership::Claimed)
        }) else {
            return ChainAdmission::Forwarded;
        };
        ChainAdmission::Held(ChainToken {
            source: self.device.source.clone(),
            authorization: self.authorization,
            candidate: group.candidate.unwrap().id,
            segment: group.last_segment,
        })
    }
    pub(super) fn continuation(&self, token: &ChainToken) -> Result<(), Fault> {
        if token.source != self.device.source {
            return Err(Fault::StaleSource);
        }
        if token.authorization != self.authorization {
            return Err(Fault::Unauthorized);
        }
        let Some(group) = self.group.as_ref().filter(|group| {
            group.chain
                && group.completion
                && matches!(group.ownership, Ownership::Pending | Ownership::Claimed)
        }) else {
            return Err(Fault::TooLate);
        };
        if group
            .candidate
            .is_none_or(|candidate| candidate.id != token.candidate)
            || group.last_segment != token.segment
        {
            return Err(Fault::WrongTicket);
        }
        Ok(())
    }
    /// Inspect a retained segment without advancing input or delivery clocks.
    pub fn chain_ownership(&self, token: &ChainToken, at: Duration) -> Option<Ownership> {
        if self.stopped
            || at < self.last_at
            || token.source != self.device.source
            || token.authorization != self.authorization
        {
            return None;
        }
        let group = self.group.as_ref()?;
        if !group.chain
            || !group.completion
            || group
                .candidate
                .is_none_or(|candidate| candidate.id != token.candidate)
            || !(group.first_segment..=group.last_segment).contains(&token.segment)
            || !matches!(group.ownership, Ownership::Pending | Ownership::Claimed)
            || group.ownership == Ownership::Pending
                && matches!(group.candidate?.policy,Policy::Buffer{decision,..} if at>=group.first+decision)
        {
            return None;
        }
        Some(group.ownership)
    }
    /// Seal all claimed segments through the latest native token.
    pub fn seal_chain(&self, token: &ChainToken, at: Duration) -> Result<ClaimedChain, Fault> {
        self.continuation(token)?;
        if self.chain_ownership(token, at) != Some(Ownership::Claimed) {
            return Err(Fault::TooLate);
        }
        let group = self.group.as_ref().unwrap();
        Ok(ClaimedChain {
            source: token.source.clone(),
            authorization: token.authorization,
            candidate: token.candidate,
            first: group.first_segment,
            through: group.last_segment,
        })
    }
    /// Check receipt transport lifetime, independently of later ordinary groups.
    pub fn chain_receipt_valid(&self, receipt: &ClaimedChain) -> bool {
        !self.stopped
            && self.device.source == receipt.source
            && self.authorization == receipt.authorization
    }
}
