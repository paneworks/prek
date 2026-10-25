//! Full broker-approved context snapshots, transferred without truncation.
use crate::{
    context::State,
    protocol::{self, Fence},
    worker::ProvidedContext,
};
use serde::{Deserialize, Serialize};
use std::{io, time::Duration};

pub const MAX_BYTES: usize = 4 * 1024 * 1024;
pub const CHUNK_BYTES: usize = 32 * 1024;
pub const TIMEOUT: Duration = Duration::from_secs(1);
pub const FRESHNESS: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    pub fence: Fence,
    pub sampled_at: Duration,
    pub state: State,
    pub context: Option<Box<ProvidedContext>>,
    pub error: Option<String>,
}
impl View {
    pub fn validate(&self) -> io::Result<()> {
        self.fence.validate()?;
        if self
            .error
            .as_ref()
            .is_some_and(|error| error.len() > 512 || error.chars().any(char::is_control))
        {
            return Err(protocol::invalid("invalid context diagnostic"));
        }
        if let Some(context) = &self.context {
            let binding = &context.binding;
            if context.snapshot.generation != self.fence.session
                || context.snapshot.state != self.state
                || binding.uid == 0
                || binding.compositor_pid == 0
                || binding.compositor_start == 0
                || [&binding.seat, &binding.session].into_iter().any(|name| {
                    name.is_empty() || name.len() > 1024 || name.chars().any(char::is_control)
                })
            {
                return Err(protocol::invalid(
                    "context snapshot differs from its session authority",
                ));
            }
            context.snapshot.validate().map_err(io::Error::other)?;
        }
        Ok(())
    }
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        self.validate()?;
        struct Buffer(Vec<u8>);
        impl io::Write for Buffer {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.0.len().saturating_add(bytes.len()) > MAX_BYTES {
                    return Err(protocol::invalid(
                        "context snapshot exceeds transfer byte limit",
                    ));
                }
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut buffer = Buffer(Vec::new());
        serde_json::to_writer(&mut buffer, self).map_err(io::Error::other)?;
        Ok(buffer.0)
    }
}

pub struct Export {
    pub fence: Fence,
    pub contains_context: bool,
    data: Vec<u8>,
    expires: Duration,
}
impl Export {
    pub fn new(view: &View, now: Duration) -> io::Result<Self> {
        let expires = now + TIMEOUT;
        if expires <= now
            || view.sampled_at > now
            || view
                .context
                .as_ref()
                .is_some_and(|context| context.snapshot.captured_at > now)
        {
            return Err(protocol::invalid(
                "context snapshot is expired or from the future",
            ));
        }
        Ok(Self {
            fence: view.fence,
            contains_context: view.context.is_some(),
            data: view.encode()?,
            expires,
        })
    }
    pub fn chunk(
        &self,
        fence: Fence,
        offset: usize,
        now: Duration,
    ) -> io::Result<(usize, Vec<u8>)> {
        if fence != self.fence
            || now >= self.expires
            || offset >= self.data.len()
            || !offset.is_multiple_of(CHUNK_BYTES)
        {
            return Err(protocol::invalid(
                "stale or invalid context transfer offset",
            ));
        }
        Ok((
            self.data.len(),
            self.data[offset..(offset + CHUNK_BYTES).min(self.data.len())].to_vec(),
        ))
    }
}

pub struct Assembly {
    fence: Fence,
    bytes: Option<usize>,
    data: Vec<u8>,
    expires: Duration,
}
impl Assembly {
    pub fn new(fence: Fence, now: Duration) -> io::Result<Self> {
        fence.validate()?;
        Ok(Self {
            fence,
            bytes: None,
            data: Vec::new(),
            expires: now + TIMEOUT,
        })
    }
    pub fn offset(&self) -> usize {
        self.data.len()
    }
    pub fn deadline(&self) -> Duration {
        self.expires
    }
    pub fn push(
        &mut self,
        fence: Fence,
        offset: usize,
        bytes: usize,
        data: &[u8],
        now: Duration,
    ) -> io::Result<Option<View>> {
        if fence != self.fence
            || now >= self.expires
            || bytes == 0
            || bytes > MAX_BYTES
            || offset != self.data.len()
            || offset >= bytes
            || self.bytes.is_some_and(|expected| expected != bytes)
            || data.len() != CHUNK_BYTES.min(bytes - offset)
        {
            return Err(protocol::invalid(
                "stale, reordered or invalid context chunk",
            ));
        }
        self.bytes = Some(bytes);
        self.data.extend_from_slice(data);
        if self.data.len() < bytes {
            return Ok(None);
        }
        let view: View = serde_json::from_slice(&self.data).map_err(io::Error::other)?;
        view.validate()?;
        if view.fence != fence
            || view.sampled_at > now
            || view
                .context
                .as_ref()
                .is_some_and(|context| context.snapshot.captured_at > now)
        {
            return Err(protocol::invalid(
                "assembled context lost its freshness or fence",
            ));
        }
        Ok(Some(view))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        config::Screen,
        context::{Binding, OutputState, SessionKind, Snapshot},
        gestures::{Output, Point, Transform},
    };
    fn output(name: &str, generation: u64) -> OutputState {
        OutputState {
            geometry: Output {
                name: name.into(),
                pixel_width: 1000.0,
                pixel_height: 800.0,
                scale: 1.0,
                origin: Point { x: 0.0, y: 0.0 },
                transform: Transform::Normal,
            },
            geometry_generation: generation,
            screen: Screen::On,
        }
    }
    pub(crate) fn view(at: Duration) -> View {
        View {
            fence: fence(),
            sampled_at: at,
            state: State::Desktop,
            error: None,
            context: Some(Box::new(ProvidedContext {
                binding: Binding {
                    seat: "seat0".into(),
                    session: "fixture".into(),
                    uid: 1000,
                    compositor_pid: 123,
                    compositor_start: 456,
                    kind: SessionKind::User,
                },
                snapshot: Snapshot {
                    generation: 1,
                    revision: 7,
                    captured_at: at,
                    state: State::Desktop,
                    outputs: vec![output("fixture-output", 2)],
                },
            })),
        }
    }
    /// A desktop view with `count` (at most 256) long-named outputs.
    pub(crate) fn wide_view(at: Duration, count: usize) -> View {
        let mut view = view(at);
        view.context.as_mut().unwrap().snapshot.outputs = (1..=count as u64)
            .map(|i| {
                let mut output = output(&format!("{}-{i:03}", "o".repeat(240)), i);
                output.geometry.origin.x = f64::from_bits(0x3fb999999999999a);
                output
            })
            .collect();
        view
    }
    fn fence() -> Fence {
        Fence {
            broker: protocol::Epoch([1, 2]),
            configuration: 1,
            session: 1,
        }
    }
    #[test]
    fn context_transfer_preserves_full_metadata_and_rejects_stale_or_reordered_chunks() {
        let view = View {
            fence: fence(),
            sampled_at: Duration::ZERO,
            state: State::Unknown,
            context: None,
            error: Some("unavailable".into()),
        };
        let export = Export::new(&view, Duration::ZERO).unwrap();
        let (bytes, data) = export.chunk(fence(), 0, Duration::ZERO).unwrap();
        let mut assembly = Assembly::new(fence(), Duration::ZERO).unwrap();
        assert!(
            assembly
                .push(fence(), 1, bytes, &data, Duration::ZERO)
                .is_err()
        );
        assert_eq!(
            assembly
                .push(fence(), 0, bytes, &data, Duration::ZERO)
                .unwrap(),
            Some(view)
        );
        assert!(export.chunk(fence(), 0, TIMEOUT).is_err());
        let mut assembly = Assembly::new(fence(), Duration::ZERO).unwrap();
        assert!(assembly.push(fence(), 0, bytes, &data, TIMEOUT).is_err());
        let other = Fence {
            session: 2,
            ..fence()
        };
        assert!(export.chunk(other, 0, Duration::ZERO).is_err());
        assert!(
            assembly
                .push(other, 0, bytes, &data, Duration::ZERO)
                .is_err()
        );
        assert!(
            assembly
                .push(fence(), 0, MAX_BYTES + 1, &data, Duration::ZERO)
                .is_err()
        );
    }
    #[test]
    fn full_geometry_spans_chunks_and_retains_exact_float_values() {
        let view = wide_view(Duration::ZERO, 256);
        let export = Export::new(&view, Duration::ZERO).unwrap();
        let (bytes, _) = export.chunk(fence(), 0, Duration::ZERO).unwrap();
        assert!(bytes > CHUNK_BYTES);
        let mut assembly = Assembly::new(fence(), Duration::ZERO).unwrap();
        let mut result = None;
        while assembly.offset() < bytes {
            let offset = assembly.offset();
            let (_, data) = export.chunk(fence(), offset, Duration::ZERO).unwrap();
            result = assembly
                .push(fence(), offset, bytes, &data, Duration::ZERO)
                .unwrap();
        }
        assert_eq!(result, Some(view));
    }
    #[test]
    fn context_body_rejects_fence_state_future_time_and_oversized_diagnostics() {
        let mut changed = view(Duration::ZERO);
        changed.context.as_mut().unwrap().snapshot.generation = 2;
        assert!(changed.validate().is_err());
        changed = view(Duration::ZERO);
        changed.state = State::Locked;
        assert!(changed.validate().is_err());
        changed = view(Duration::ZERO);
        changed.context.as_mut().unwrap().binding.uid = 0;
        assert!(changed.validate().is_err());
        changed = view(Duration::ZERO);
        changed.context.as_mut().unwrap().snapshot.outputs[0]
            .geometry
            .scale = f64::NAN;
        assert!(changed.validate().is_err());
        changed = view(Duration::ZERO);
        changed.error = Some("x".repeat(513));
        assert!(changed.encode().is_err());
        assert!(Export::new(&view(Duration::from_secs(1)), Duration::ZERO).is_err());
        let mut changed = view(Duration::ZERO);
        changed.context.as_mut().unwrap().snapshot.captured_at = Duration::from_secs(1);
        assert!(Export::new(&changed, Duration::ZERO).is_err());
        let data = serde_json::to_vec(&view(Duration::from_secs(1))).unwrap();
        assert!(
            Assembly::new(fence(), Duration::ZERO)
                .unwrap()
                .push(fence(), 0, data.len(), &data, Duration::ZERO)
                .is_err()
        );
        let mut changed = view(Duration::ZERO);
        changed.fence.configuration = 2;
        let data = changed.encode().unwrap();
        assert!(
            Assembly::new(fence(), Duration::ZERO)
                .unwrap()
                .push(fence(), 0, data.len(), &data, Duration::ZERO)
                .is_err()
        );
    }
    #[test]
    fn transfer_chunks_survive_private_packet_encoding() {
        let original = wide_view(Duration::from_secs(1), 256);
        let encoded = original.encode().unwrap();
        assert!(encoded.len() > CHUNK_BYTES);
        let mut assembly = Assembly::new(original.fence, Duration::from_secs(1)).unwrap();
        let mut result = None;
        for (index, chunk) in encoded.chunks(CHUNK_BYTES).enumerate() {
            let wire = protocol::encode(&protocol::Packet {
                version: protocol::VERSION,
                sequence: index as u64 + 1,
                at: Duration::from_secs(1),
                message: protocol::Reply::ContextData {
                    fence: original.fence,
                    offset: index * CHUNK_BYTES,
                    bytes: encoded.len(),
                    data: chunk.to_vec(),
                },
            })
            .unwrap();
            let packet: protocol::Packet<protocol::Reply> = serde_json::from_slice(&wire).unwrap();
            packet.message.validate().unwrap();
            let protocol::Reply::ContextData {
                fence,
                offset,
                bytes,
                data,
            } = packet.message
            else {
                panic!()
            };
            result = assembly
                .push(fence, offset, bytes, &data, Duration::from_secs(1))
                .unwrap();
        }
        assert_eq!(result.unwrap(), original);
    }
}
