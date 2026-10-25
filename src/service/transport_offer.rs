use crate::{
    configuration::Command,
    protocol::Fence,
    transport::{ConsumerEvidence, Descriptor},
};
use std::io;

pub(super) fn reply(
    descriptor: &Descriptor,
    fence: Fence,
    evidence: ConsumerEvidence,
    physical_started: &mut bool,
) -> io::Result<Command> {
    descriptor.validate().map_err(crate::protocol::invalid)?;
    fence.validate()?;
    if !descriptor.is_generated_keyboard() {
        *physical_started = true;
    }
    Ok(Command::Transport {
        fence,
        source: descriptor.source.clone(),
        marker: descriptor.marker.clone(),
        evidence,
    })
}

#[cfg(test)]
mod tests;
