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
    /// Native connection bindings verified by the broker; no rendered regions
    /// or controller authority are inferred from these role states.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub morf: Vec<crate::morf::ShellState>,
    /// Native primary-frame bounds remain inspection data. They do not add
    /// scoped regions, keyboard absence or authentication to the snapshot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub morf_rendered: Vec<crate::morf::rendered::OutputContext>,
}
impl View {
    pub fn validate(&self) -> io::Result<()> {
        self.fence.validate()?;
        if self.morf.len() > 64 {
            return Err(protocol::invalid("too many native Morf shell bindings"));
        }
        let mut outputs = std::collections::BTreeSet::new();
        for state in &self.morf {
            use crate::morf::NativeState;
            state.validate()?;
            let Some(context) = &self.context else {
                return Err(protocol::invalid("Morf binding lacks owning context"));
            };
            let binding = &context.binding;
            if self.state != State::Desktop
                || context.snapshot.secure_overlay
                || state.compositor()
                    != (
                        binding.compositor_pid,
                        binding.compositor_start,
                        binding.uid,
                    )
                || !outputs.insert(&state.output)
                || !context
                    .snapshot
                    .outputs
                    .iter()
                    .any(|output| output.geometry.name == state.output)
            {
                return Err(protocol::invalid(
                    "Morf binding differs from owning desktop/output",
                ));
            }
        }
        let mut rendered_outputs = std::collections::BTreeSet::new();
        if self.morf_rendered.len() > self.morf.len() {
            return Err(protocol::invalid("too many native rendered contexts"));
        }
        for rendered in &self.morf_rendered {
            let state = self
                .morf
                .iter()
                .find(|state| state.output == rendered.output)
                .ok_or_else(|| protocol::invalid("rendered context lacks native output binding"))?;
            if !rendered_outputs.insert(&rendered.output) {
                return Err(protocol::invalid("duplicate rendered output context"));
            }
            rendered.context.validate(state, self.sampled_at, None)?;
            if let Some(layout) = &rendered.layout {
                let output = self
                    .context
                    .as_ref()
                    .unwrap()
                    .snapshot
                    .outputs
                    .iter()
                    .find(|output| output.geometry.name == rendered.output)
                    .unwrap();
                layout.validate(&rendered.context, output, self.sampled_at)?;
            }
        }
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
            context.validate_layouts()?;
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
    pub(crate) fn native_view(at: Duration, count: usize) -> View {
        use crate::hyprland::{
            layouts::{Column, Position, Snapshot as Layouts, Window, Workspace},
            scrolling::profile::{Reserved, Settings},
        };
        let mut view = view(at);
        let context = view.context.as_mut().unwrap();
        let original = context.snapshot.windows[0].clone();
        context.snapshot.windows = (1..=count)
            .map(|id| {
                let mut window = original.clone();
                window.id = format!("0x{id:x}:{id:x}");
                window
            })
            .collect();
        let windows: Vec<_> = context
            .snapshot
            .windows
            .iter()
            .enumerate()
            .map(|(index, window)| Window {
                id: window.id.clone(),
                rect: [
                    window.rect.origin.x,
                    window.rect.origin.y,
                    window.rect.size.x,
                    window.rect.size.y,
                ],
                hidden: false,
                floating: false,
                grouped: false,
                fullscreen: 0,
                column: Some(Position { index, row: 0 }),
            })
            .collect();
        let columns = windows
            .iter()
            .enumerate()
            .map(|(index, window)| Column {
                index,
                width: 0.5,
                members: vec![window.id.clone()],
            })
            .collect();
        context.layouts = Some(Layouts {
            generation: context.snapshot.generation,
            revision: context.snapshot.revision,
            continuity: 1,
            captured_at: context.snapshot.captured_at,
            workspaces: vec![Workspace {
                id: 3,
                output: Some("fixture-output".into()),
                monitor: Some(0),
                active: true,
                special: false,
                layout: "scrolling".into(),
                windows,
                columns,
            }],
            profile: Some(Settings {
                direction: "down".into(),
                gaps_in: [0; 4],
                gaps_out: [0; 4],
                border: 0,
                workspace_rules: 0,
                reserved: vec![Reserved {
                    output: "fixture-output".into(),
                    area: [0.0; 4],
                }],
            }),
        });
        view
    }

    #[test]
    fn native_layout_graph_is_bound_to_the_exact_desktop_sample() {
        let original = native_view(Duration::from_secs(1), 2);
        original.validate().unwrap();
        for case in 0..14 {
            let mut changed = original.clone();
            let context = changed.context.as_mut().unwrap();
            let layouts = context.layouts.as_mut().unwrap();
            match case {
                0 => layouts.generation += 1,
                1 => layouts.revision += 1,
                2 => layouts.captured_at += Duration::from_nanos(1),
                3 => context.binding.kind = crate::context::SessionKind::Greeter,
                4 => context.snapshot.secure_overlay = true,
                5 => layouts.workspaces[0].output = Some("other".into()),
                6 => layouts.workspaces[0].id += 1,
                7 => layouts.workspaces[0].windows[0].rect[0] += 1.,
                8 => {
                    context.snapshot.windows.pop().unwrap();
                }
                9 => layouts.workspaces[0].active = false,
                10 => layouts.workspaces.clear(),
                11 => context.snapshot.windows[0].mapped = false,
                12 => layouts.profile.as_mut().unwrap().reserved[0].area[1] = f64::NAN,
                _ => context.binding.uid = 0,
            }
            assert!(changed.validate().is_err(), "case {case}");
        }
    }

    #[test]
    fn native_layout_context_uses_full_chunked_transfer_without_private_packet_pressure() {
        let original = native_view(Duration::from_secs(1), 160);
        let encoded = original.encode().unwrap();
        assert!(encoded.len() > protocol::MAX_PACKET);
        assert!(original.context.as_ref().unwrap().layouts.is_some());
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
    #[test]
    fn rendered_native_bounds_require_the_same_live_output_and_do_not_create_scopes() {
        let at = Duration::from_secs(1);
        let mut current = view(at);
        let binding = &current.context.as_ref().unwrap().binding;
        current.morf.push(crate::morf::ShellState {
            version: 1,
            role: "shell".into(),
            epoch: "a".repeat(32),
            revision: 2,
            phase: crate::morf::ShellPhase::Configured,
            compositor_pid: binding.compositor_pid,
            compositor_start: binding.compositor_start,
            compositor_uid: binding.uid,
            output: "fixture-output".into(),
        });
        current
            .morf_rendered
            .push(crate::morf::rendered::OutputContext {
                layout: None,
                output: "fixture-output".into(),
                context: crate::morf::rendered::tests::sample(at),
            });
        let snapshot = current.context.as_ref().unwrap().snapshot.clone();
        let decoded: View = serde_json::from_slice(&current.encode().unwrap()).unwrap();
        assert_eq!(decoded, current);
        assert_eq!(decoded.context.unwrap().snapshot, snapshot);
        for case in 0..6 {
            let mut changed = current.clone();
            match case {
                0 => changed.morf_rendered[0].output = "other".into(),
                1 => changed.morf_rendered[0].context.epoch = "b".repeat(32),
                2 => changed.morf.clear(),
                3 => changed.morf_rendered.push(changed.morf_rendered[0].clone()),
                4 => changed.morf_rendered[0].context.sampled_ns += 1,
                _ => changed.sampled_at += Duration::from_millis(250),
            }
            assert!(changed.validate().is_err(), "case {case}");
        }
    }
    #[test]
    fn native_morf_metadata_cannot_cross_compositor_output_or_secure_context() {
        let mut original = view(Duration::from_secs(1));
        let binding = &original.context.as_ref().unwrap().binding;
        original.morf.push(crate::morf::ShellState {
            version: 1,
            role: "shell".into(),
            epoch: "a".repeat(32),
            revision: 2,
            phase: crate::morf::ShellPhase::Configured,
            compositor_pid: binding.compositor_pid,
            compositor_start: binding.compositor_start,
            compositor_uid: binding.uid,
            output: "fixture-output".into(),
        });
        original.validate().unwrap();
        let decoded: View = serde_json::from_slice(&original.encode().unwrap()).unwrap();
        assert_eq!(decoded, original);
        for change in 0..8 {
            let mut changed = original.clone();
            match change {
                0 => changed.morf[0].output = "other-output".into(),
                1 => changed.morf[0].compositor_start += 1,
                2 => changed.morf[0].compositor_uid += 1,
                3 => changed.morf.push(changed.morf[0].clone()),
                4 => changed.context = None,
                5 => changed.context.as_mut().unwrap().snapshot.secure_overlay = true,
                6 => changed.state = State::Locked,
                _ => changed.morf = vec![changed.morf[0].clone(); 65],
            }
            assert!(changed.validate().is_err(), "tampering case {change}");
        }
    }
    pub(crate) fn view(at: Duration) -> View {
        use crate::{
            config::Screen,
            context::{
                Binding, Keyboard, OutputState, Rect, Region, SessionKind, Snapshot, Window,
            },
            gestures::{Output, Point, Transform},
        };
        let rect = Rect {
            origin: Point {
                x: f64::from_bits(0x3fb999999999999a),
                y: 10.0,
            },
            size: Point { x: 100.0, y: 120.0 },
        };
        View {
            morf: Vec::new(),
            morf_rendered: Vec::new(),
            fence: fence(),
            sampled_at: at,
            state: State::Desktop,
            error: None,
            context: Some(Box::new(ProvidedContext {
                layouts: None,
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
                    secure_overlay: false,
                    regions: vec![Region {
                        id: "widget-1".into(),
                        scope: None,
                        output: "fixture-output".into(),
                        rect,
                        interactive: true,
                    }],
                    outputs: vec![OutputState {
                        geometry: Output {
                            name: "fixture-output".into(),
                            pixel_width: 1000.0,
                            pixel_height: 800.0,
                            scale: 1.0,
                            origin: Point { x: 0.0, y: 0.0 },
                            transform: Transform::Normal,
                        },
                        geometry_generation: 2,
                        active_workspace: Some(3),
                        screen: Screen::On,
                        keyboard_known: true,
                        keyboard: Some(Keyboard {
                            rect,
                            layout: "us".into(),
                        }),
                    }],
                    windows: vec![Window {
                        id: "window-1".into(),
                        output: "fixture-output".into(),
                        workspace: 3,
                        rect,
                        content: rect,
                        mapped: true,
                        visible: true,
                        focused: true,
                        z_order: None,
                    }],
                },
            })),
        }
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
            morf: Vec::new(),
            morf_rendered: Vec::new(),
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
        let mut view = view(Duration::ZERO);
        let snapshot = &mut view.context.as_mut().unwrap().snapshot;
        for i in 2..=512 {
            let mut window = snapshot.windows[0].clone();
            window.id = format!("window-{i}");
            window.focused = false;
            snapshot.windows.push(window);
        }
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
    fn byte_budget_rejects_complete_valid_geometry_without_truncation() {
        let mut view = view(Duration::ZERO);
        let snapshot = &mut view.context.as_mut().unwrap().snapshot;
        let output = "x".repeat(256);
        snapshot.outputs[0].geometry.name = output.clone();
        let mut window = snapshot.windows[0].clone();
        window.output = output.clone();
        let mut region = snapshot.regions[0].clone();
        region.output = output;
        snapshot.windows = (0..4096)
            .map(|i| {
                let mut window = window.clone();
                window.id = format!("{}-{i}", "w".repeat(240));
                window
            })
            .collect();
        snapshot.regions = (0..4096)
            .map(|i| {
                let mut region = region.clone();
                region.id = format!("{}-{i}", "r".repeat(240));
                region
            })
            .collect();
        view.validate().unwrap();
        assert!(
            view.encode()
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
        assert_eq!(view.context.unwrap().snapshot.windows.len(), 4096);
    }
}
