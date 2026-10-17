use crate::{
    configuration::Command,
    morf::{admission::Admission, ownership::Ledger},
    protocol::Fence,
    transport::{ConsumerEvidence, Descriptor},
};
use std::io;

pub(super) fn reply(
    descriptor: &Descriptor,
    fence: Fence,
    evidence: ConsumerEvidence,
    admission: Option<Admission>,
    ledger: Option<&mut Ledger>,
    physical_started: &mut bool,
) -> io::Result<Command> {
    descriptor.validate().map_err(crate::protocol::invalid)?;
    fence.validate()?;
    let admission = if descriptor.is_generated_keyboard() {
        None
    } else {
        if let Some(ledger) = ledger {
            ledger.prepared(fence, descriptor.source.clone())?;
        }
        *physical_started = true;
        admission
    };
    Ok(Command::Transport {
        fence,
        source: descriptor.source.clone(),
        marker: descriptor.marker.clone(),
        evidence,
        admission,
    })
}

#[cfg(test)]
mod tests;
