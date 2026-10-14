//! Bounded, ordered configuration snapshots on the private worker channel.

use crate::{
    config::{Config, MAX_CONFIG_BYTES},
    ipc::Channel,
    protocol,
    worker::Subscriptions,
};
use serde::{Deserialize, Serialize};
use std::{io, time::Duration};

pub const CHUNK_BYTES: usize = 8192;
pub const INSTALL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Subscriptions {
        update: Subscriptions,
    },
    Transport {
        fence: crate::protocol::Fence,
        source: crate::input::Source,
        evidence: crate::transport::ConsumerEvidence,
    },
    Begin {
        generation: u64,
        bytes: usize,
    },
    Chunk {
        generation: u64,
        offset: usize,
        data: Vec<u8>,
    },
    Commit {
        generation: u64,
    },
}
pub struct Transfer {
    generation: u64,
    data: Vec<u8>,
    offset: usize,
    begun: bool,
    finished: bool,
}
impl Transfer {
    pub fn new(text: String, generation: u64) -> io::Result<Self> {
        if generation == 0 || text.is_empty() || text.len() > MAX_CONFIG_BYTES {
            return Err(protocol::invalid(
                "invalid configuration snapshot size or generation",
            ));
        }
        Ok(Self {
            generation,
            data: text.into_bytes(),
            offset: 0,
            begun: false,
            finished: false,
        })
    }
    /// A bounded burst leaves health and client processing runnable even for
    /// a maximum-size file. A full socket never advances the transfer offset.
    pub fn send(&mut self, channel: &mut Channel, now: Duration) -> io::Result<bool> {
        for _ in 0..4 {
            if self.finished {
                break;
            }
            let command = if !self.begun {
                Command::Begin {
                    generation: self.generation,
                    bytes: self.data.len(),
                }
            } else if self.offset < self.data.len() {
                Command::Chunk {
                    generation: self.generation,
                    offset: self.offset,
                    data: self.data[self.offset..(self.offset + CHUNK_BYTES).min(self.data.len())]
                        .to_vec(),
                }
            } else {
                Command::Commit {
                    generation: self.generation,
                }
            };
            if !channel.send(&command, now)? {
                break;
            }
            match command {
                Command::Begin { .. } => self.begun = true,
                Command::Chunk { data, .. } => self.offset += data.len(),
                Command::Commit { .. } => self.finished = true,
                _ => unreachable!(),
            }
        }
        Ok(self.finished)
    }
}
struct Pending {
    generation: u64,
    bytes: usize,
    data: Vec<u8>,
    expires: Duration,
}
#[derive(Default)]
pub struct Assembly {
    pending: Option<Pending>,
}
pub struct Installation {
    pub generation: u64,
    pub config: io::Result<Config>,
}
impl Assembly {
    pub fn deadline(&self) -> Option<Duration> {
        self.pending.as_ref().map(|pending| pending.expires)
    }
    pub fn accept(&mut self, command: Command, now: Duration) -> io::Result<Option<Installation>> {
        if self.deadline().is_some_and(|deadline| now >= deadline) {
            return Err(protocol::invalid("configuration snapshot transfer expired"));
        }
        match command {
            Command::Begin { generation, bytes } if self.pending.is_none() => {
                if generation == 0 || bytes == 0 || bytes > MAX_CONFIG_BYTES {
                    return Err(protocol::invalid("invalid configuration snapshot header"));
                }
                self.pending = Some(Pending {
                    generation,
                    bytes,
                    data: Vec::with_capacity(bytes),
                    expires: now + INSTALL_TIMEOUT,
                });
            }
            Command::Chunk {
                generation,
                offset,
                data,
            } => {
                let pending = self
                    .pending
                    .as_mut()
                    .ok_or_else(|| protocol::invalid("configuration chunk lacks a header"))?;
                if generation != pending.generation
                    || offset != pending.data.len()
                    || data.is_empty()
                    || data.len() > CHUNK_BYTES
                    || pending.data.len() + data.len() > pending.bytes
                {
                    return Err(protocol::invalid(
                        "configuration chunk is reordered, oversized or belongs to another generation",
                    ));
                }
                pending.data.extend_from_slice(&data);
            }
            Command::Commit { generation } => {
                let pending = self
                    .pending
                    .take()
                    .ok_or_else(|| protocol::invalid("configuration commit lacks a header"))?;
                if generation != pending.generation || pending.data.len() != pending.bytes {
                    return Err(protocol::invalid(
                        "configuration commit is incomplete or belongs to another generation",
                    ));
                }
                let config = String::from_utf8(pending.data)
                    .map_err(io::Error::other)
                    .and_then(|text| Config::parse(&text).map_err(io::Error::other));
                return Ok(Some(Installation { generation, config }));
            }
            _ => {
                return Err(protocol::invalid(
                    "invalid configuration transfer transition",
                ));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::monotonic;
    #[test]
    fn maximum_snapshot_crosses_packet_boundaries_and_preserves_utf8_bytes() {
        let mut text = "version = 1\n# ".to_owned();
        text.push_str(&"é".repeat((MAX_CONFIG_BYTES - text.len()) / 2));
        text.push_str(&" ".repeat(MAX_CONFIG_BYTES - text.len()));
        let expected = Config::parse(&text).unwrap();
        let mut transfer = Transfer::new(text, 4).unwrap();
        let (mut sender, mut receiver) = Channel::pair().unwrap();
        let mut assembly = Assembly::default();
        let mut installed = None;
        for _ in 0..512 {
            transfer.send(&mut sender, monotonic().unwrap()).unwrap();
            while let Some(packet) = receiver.receive::<Command>().unwrap() {
                if let Some(result) = assembly
                    .accept(packet.message, monotonic().unwrap())
                    .unwrap()
                {
                    assert_eq!(result.generation, 4);
                    installed = Some(result.config.unwrap());
                }
            }
            if installed.is_some() {
                break;
            }
        }
        assert_eq!(installed, Some(expected));
        assert!(assembly.deadline().is_none());
    }
    #[test]
    fn partial_reordered_duplicate_oversized_and_expired_snapshots_are_refused() {
        let start = Duration::from_secs(1);
        let header = || Command::Begin {
            generation: 2,
            bytes: 16,
        };
        for command in [
            Command::Chunk {
                generation: 2,
                offset: 1,
                data: vec![0],
            },
            Command::Chunk {
                generation: 3,
                offset: 0,
                data: vec![0],
            },
            Command::Chunk {
                generation: 2,
                offset: 0,
                data: vec![0; CHUNK_BYTES + 1],
            },
            Command::Commit { generation: 2 },
            header(),
        ] {
            let mut assembly = Assembly::default();
            assembly.accept(header(), start).unwrap();
            assert!(assembly.accept(command, start).is_err());
        }
        let mut assembly = Assembly::default();
        assert!(
            assembly
                .accept(
                    Command::Begin {
                        generation: 1,
                        bytes: MAX_CONFIG_BYTES + 1
                    },
                    start
                )
                .is_err()
        );
        assert!(
            assembly
                .accept(
                    Command::Chunk {
                        generation: 1,
                        offset: 0,
                        data: vec![0]
                    },
                    start
                )
                .is_err()
        );
        assembly.accept(header(), start).unwrap();
        assert!(
            assembly
                .accept(
                    Command::Chunk {
                        generation: 2,
                        offset: 0,
                        data: vec![0]
                    },
                    start + INSTALL_TIMEOUT
                )
                .is_err()
        );
    }
    #[test]
    fn invalid_completed_file_can_be_replaced_by_a_valid_snapshot() {
        let mut assembly = Assembly::default();
        for (text, valid) in [("not = toml = valid", false), ("version = 1", true)] {
            assembly
                .accept(
                    Command::Begin {
                        generation: 2,
                        bytes: text.len(),
                    },
                    Duration::ZERO,
                )
                .unwrap();
            assembly
                .accept(
                    Command::Chunk {
                        generation: 2,
                        offset: 0,
                        data: text.as_bytes().to_vec(),
                    },
                    Duration::ZERO,
                )
                .unwrap();
            let installed = assembly
                .accept(Command::Commit { generation: 2 }, Duration::ZERO)
                .unwrap()
                .unwrap();
            assert_eq!(installed.config.is_ok(), valid);
        }
    }
}
