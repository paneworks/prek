//! Read-only catalog of permitted metadata actually received in this VM.
use super::*;
use crate::{
    input::Source,
    input_events::{DeviceInfo, Event, ResetReason},
    protocol::Fence,
};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

const MAX_DEVICES: usize = 256;
const MAX_BYTES: usize = 4 * 1024 * 1024;
#[derive(Default)]
pub(super) struct Catalog {
    fence: Option<Fence>,
    entries: BTreeMap<Source, (DeviceInfo, usize)>,
    revision: u64,
    bytes: usize,
    received: bool,
    custom: bool,
}
impl Catalog {
    pub(super) fn apply(
        &mut self,
        config: &Config,
        payload: &serde_json::Value,
    ) -> Result<(), ConfigError> {
        if !config.policy.allow_input_events
            || config.mode != crate::config::Mode::Observe
            || payload["input"]["kind"] != "reset"
        {
            return Ok(());
        }
        let input: Event = serde_json::from_value(payload["input"].clone()).map_err(error)?;
        input.validate().map_err(error)?;
        let fence: Fence = serde_json::from_value(payload["fence"].clone()).map_err(error)?;
        fence.validate().map_err(error)?;
        let Event::Reset {
            source,
            reason,
            metadata,
            ..
        } = input
        else {
            unreachable!()
        };
        if metadata
            .as_ref()
            .is_some_and(|info| Event::selected(config, &info.device) != info.selectors)
        {
            return Err(error("device metadata exceeds approved selectors"));
        }
        self.apply_info(fence, source, reason, metadata)
    }
    fn apply_info(
        &mut self,
        fence: Fence,
        source: Source,
        reason: ResetReason,
        metadata: Option<Box<DeviceInfo>>,
    ) -> Result<(), ConfigError> {
        if self.fence.is_some_and(|current| current != fence) {
            *self = Self::default();
        }
        self.fence = Some(fence);
        if reason == ResetReason::Removed {
            if let Some((_, bytes)) = self.entries.get(&source) {
                let revision = self
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| error("device catalog revision exhausted"))?;
                self.bytes -= bytes;
                self.entries.remove(&source);
                self.revision = revision;
            }
            return Ok(());
        }
        let Some(info) = metadata else {
            return Ok(());
        };
        if self
            .entries
            .keys()
            .any(|key| key.device == source.device && key.generation > source.generation)
        {
            return Err(error("device catalog generation regressed"));
        }
        if self
            .entries
            .get(&source)
            .is_some_and(|(current, _)| current == info.as_ref())
        {
            return Ok(());
        }
        let bytes = runtime::bounded_json(&info, MAX_BYTES)
            .map_err(error)?
            .len();
        let retired: usize = self
            .entries
            .iter()
            .filter(|(key, _)| key.device == source.device)
            .map(|(_, (_, bytes))| bytes)
            .sum();
        let count = self
            .entries
            .keys()
            .filter(|key| key.device != source.device)
            .count();
        if count >= MAX_DEVICES || self.bytes - retired + bytes > MAX_BYTES {
            return Err(error("permitted device catalog exceeds limits"));
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| error("device catalog revision exhausted"))?;
        self.entries.retain(|key, _| key.device != source.device);
        self.entries.insert(source, (*info, bytes));
        self.bytes = self.bytes - retired + bytes;
        self.received = true;
        self.revision = revision;
        Ok(())
    }
    pub(super) fn context(&mut self, view: &crate::context_transfer::View) {
        if self.fence.is_some_and(|fence| fence != view.fence)
            || view.state != crate::context::State::Desktop
            || view.context.as_ref().is_none_or(|context| {
                context.binding.kind != crate::context::SessionKind::User
                    || context.snapshot.state != crate::context::State::Desktop
            })
        {
            *self = Self::default();
        }
    }
}

fn source<'gc>(ctx: luna::Context<'gc>, value: Value<'gc>) -> Result<Source, luna::Error<'gc>> {
    let Value::Table(table) = value else {
        return Err(io::Error::other("expected a Source table").into());
    };
    let seat: luna::String = table.get(ctx, "seat")?;
    Ok(Source {
        device: values::canonical_identity(table.get(ctx, "device")?)?,
        generation: values::canonical_identity(table.get(ctx, "generation")?)?,
        seat: seat.to_str().map_err(io::Error::other)?.into(),
    })
}

pub(super) fn getter<'gc>(
    ctx: &luna::Context<'gc>,
    host: Rc<RefCell<runtime::Host>>,
    field: &'static str,
) -> Callback<'gc> {
    Callback::from_fn(ctx, move |ctx, _, mut stack| {
        let host = host.borrow();
        if host.configuring {
            return Err(
                io::Error::other("device and seat queries require runtime evaluation").into(),
            );
        }
        let requested = if matches!(field, "get" | "position") {
            Some(source(ctx, stack.from_front::<Value>(ctx)?)?)
        } else {
            None
        };
        let coordinates = if field == "position" {
            Some(stack.consume::<(i32, i32)>(ctx)?)
        } else {
            None
        };
        let Some(view) = context::desktop(&host) else {
            stack.replace(ctx, Value::Nil);
            return Ok(CallbackReturn::Return);
        };
        let provided = view.context.as_ref().unwrap();
        let available = host.devices.received
            && host.devices.fence == Some(view.fence)
            && (!host.devices.custom
                || super::capabilities::current(&host).is_some_and(|caps| {
                    use crate::protocol::Capability;
                    caps.fence == view.fence
                        && [
                            Capability::CustomRecognizers,
                            Capability::LuaEvents,
                            Capability::Observations,
                            Capability::Context,
                        ]
                        .iter()
                        .all(|cap| caps.negotiated.contains(cap))
                }));
        let entry = requested
            .as_ref()
            .filter(|source| source.seat == provided.binding.seat)
            .and_then(|source| host.devices.entries.get(source));
        let mut value = match field {
            "seat" => {
                let table = Table::new(&ctx);
                table.set(ctx, "binding", values::binding(ctx, &provided.binding)?)?;
                table.set(ctx, "fence", values::context_fence(ctx, &view.fence)?)?;
                table.set(ctx, "state", "desktop")?;
                Value::Table(table)
            }
            "list" if available => {
                let table = Table::new(&ctx);
                for (index, (info, _)) in host
                    .devices
                    .entries
                    .values()
                    .filter(|(info, _)| info.device.source.seat == provided.binding.seat)
                    .enumerate()
                {
                    table.set(ctx, index + 1, values::device_info(ctx, info)?)?;
                }
                Value::Table(table)
            }
            "get" if available => entry
                .map(|(info, _)| values::device_info(ctx, info))
                .transpose()?
                .unwrap_or(Value::Nil),
            "revision" if available => {
                Value::String(ctx.intern(host.devices.revision.to_string().as_bytes()))
            }
            "position" if available => entry
                .and_then(|(info, _)| info.mapping.as_ref())
                .map(|mapping| {
                    let (x, y) = coordinates.unwrap();
                    let position = mapping.map(x, y).map_err(io::Error::other)?;
                    luna_util::serde::to_value(ctx, &position).map_err(io::Error::other)
                })
                .transpose()?
                .unwrap_or(Value::Nil),
            "list" | "get" | "revision" | "position" => Value::Nil,
            _ => return Err(io::Error::other("unknown permitted device query").into()),
        };
        if context::desktop(&host).is_none()
            || field != "seat"
                && host.devices.custom
                && super::capabilities::current(&host).is_none()
        {
            value = Value::Nil;
        }
        stack.replace(ctx, value);
        Ok(CallbackReturn::Return)
    })
}

#[cfg(target_os = "linux")]
impl Runtime {
    /// Update the received catalog without dispatching an input callback.
    pub fn recognizer_metadata(
        &mut self,
        fence: Fence,
        metadata: &crate::recognition::Metadata,
    ) -> Result<(), ConfigError> {
        let result = self.recognizer_metadata_inner(fence, metadata);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn recognizer_metadata_inner(
        &mut self,
        fence: Fence,
        metadata: &crate::recognition::Metadata,
    ) -> Result<(), ConfigError> {
        if self.failed {
            return Err(error("Lua runtime was retired after callback failure"));
        }
        fence.validate().map_err(error)?;
        let mut host = self.host.borrow_mut();
        let now = host
            .now
            .and_then(|now| {
                now.checked_add(
                    host.context_clock
                        .map_or(Duration::ZERO, |clock| clock.elapsed()),
                )
            })
            .ok_or_else(|| error("recognizer metadata requires a monotonic clock"))?;
        let context = context::desktop(&host)
            .ok_or_else(|| error("recognizer metadata requires owning desktop context"))?;
        let caps = super::capabilities::current(&host)
            .ok_or_else(|| error("recognizer metadata requires current capabilities"))?;
        if context.fence != fence
            || caps.fence != fence
            || !crate::recognition::permitted_subscription(
                &self.config,
                &caps.negotiated,
                context.context.as_deref(),
                &metadata.source.seat,
                fence.session,
                now,
            )
        {
            return Err(error("recognizer metadata lacks current owning authority"));
        }
        let scope = self
            .recognizers
            .iter()
            .flat_map(|binding| binding.definition.selectors.iter().cloned())
            .collect();
        metadata
            .validate_for(&self.config, &scope, now)
            .map_err(error)?;
        host.devices.apply_info(
            fence,
            metadata.source.clone(),
            metadata.reason,
            metadata.metadata.clone(),
        )?;
        host.devices.custom = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    mod recognizer_metadata;
    use crate::{
        input::{ABS_MT_POSITION_X, ABS_MT_POSITION_Y, Axis, Device, EV_ABS},
        input_events::ResetReason,
    };
    fn definition() -> Definition {
        Definition::compile(r#"
local p=require('prek')
p.events.on('input',function(payload)
 local e=payload.input
 if e.reason=='removed' then assert(#p.devices.list()==0 and p.devices.get(e.source)==nil);return end
 local info=p.devices.get(e.source)
 assert(info and #p.devices.list()==1 and p.devices.revision())
 assert(info.device.source.device==e.source.device and info.device.source.generation==e.source.generation)
 assert(e.metadata.device.source.device==e.source.device)
 assert(info.mapping.generation==e.metadata.mapping.generation)
 assert(info.device.axes[53].maximum==100 and e.metadata.device.axes[53].maximum==100)
 local pos=p.devices.position(e.source,50,50)
 assert(pos.normalized.x==0.5 and pos.normalized.y==0.25 and pos.millimetres.x==5)
 if info.mapping.output then assert(pos['local'].x==50 and pos.global.x==60 and pos.global.y==70)
 else assert(pos['local']==nil and pos.global==nil) end
 assert(not pcall(p.devices.position,e.source,101,50))
 assert(p.devices.get{device=e.source.device,generation='1',seat='other'}==nil)
 assert(not pcall(p.devices.get,{device=1,generation='1',seat='seat0'}))
 assert(not pcall(p.devices.get,{device='01',generation='1',seat='seat0'}))
 info.device.name='forged';info.device.axes[53].maximum=999
 assert(p.devices.get(e.source).device.name=='permitted touch')
 assert(p.devices.get(e.source).device.axes[53].maximum==100)
 local seat=p.seats.current();assert(seat.binding.seat=='seat0' and seat.state=='desktop')
 seat.binding.seat='forged';assert(p.seats.current().binding.seat=='seat0')
 p.log.info('device-query',{revision=p.devices.revision()})
end)
p.events.on('ready',function()p.log.info('availability',{devices=p.devices.list()~=nil,seat=p.seats.current()~=nil})end)
return p.config.new{policy={allow_input_events=true,allowed_devices={'touch'}},devices={touch={absolute_axes={53,54}}}}
"#,"device-query.lua").unwrap()
    }
    fn metadata(generation: u64) -> DeviceInfo {
        let x = Axis {
            minimum: 0,
            maximum: 100,
            resolution: 10,
            fuzz: 0,
            flat: 0,
        };
        let y = Axis {
            maximum: 200,
            resolution: 20,
            ..x
        };
        let device = Device {
            source: Source {
                device: 1 << 63,
                generation,
                seat: "seat0".into(),
            },
            identity: "permitted-touch".into(),
            name: "permitted touch".into(),
            path: String::new(),
            physical: String::new(),
            unique: String::new(),
            vendor: 1,
            product: 2,
            properties: vec![],
            axes: BTreeMap::from([(ABS_MT_POSITION_X, x), (ABS_MT_POSITION_Y, y)]),
            capabilities: BTreeMap::from([(EV_ABS, vec![ABS_MT_POSITION_X, ABS_MT_POSITION_Y])]),
            virtual_device: false,
        };
        let mut mapping = crate::gestures::Mapping::from_device(&device).unwrap();
        mapping.generation = u64::MAX;
        DeviceInfo {
            device,
            selectors: vec!["touch".into()],
            mapping: Some(mapping),
        }
    }
    fn payload(info: DeviceInfo, reason: ResetReason) -> serde_json::Value {
        serde_json::json!({"fence":{"broker":[1,2],"configuration":1,"session":1},"input":Event::Reset {
            source:info.device.source.clone(),at:Duration::ZERO,selectors:info.selectors.clone(),reason,keys:Default::default(),contacts:vec![],metadata:if reason==ResetReason::Removed {None} else {Some(Box::new(info))}
        }})
    }
    #[test]
    fn device_queries_are_runtime_only_and_do_not_scan_host_devices() {
        for call in [
            "devices.list()",
            "devices.get({})",
            "devices.revision()",
            "devices.position({},0,0)",
            "seats.current()",
        ] {
            assert!(
                Definition::compile(
                    &format!("local p=require('prek');p.{call};return {{version=1}}"),
                    "early-device-query.lua"
                )
                .unwrap_err()
                .0
                .contains("runtime evaluation")
            );
        }
        let mut runtime = definition().instantiate().unwrap();
        let logs = runtime
            .dispatch(EventKind::Ready, &serde_json::json!({}))
            .unwrap();
        assert_eq!(
            logs[0].fields,
            serde_json::json!({"devices":false,"seat":false})
        );
    }
    #[test]
    fn device_queries_preserve_identities_mapping_copies_and_freshness() {
        let mut runtime = definition().instantiate().unwrap();
        runtime
            .set_context(
                crate::context_transfer::tests::view(Duration::ZERO),
                Duration::ZERO,
            )
            .unwrap();
        let logs = runtime
            .dispatch(
                EventKind::Input,
                &payload(metadata(u64::MAX), ResetReason::Attached),
            )
            .unwrap();
        assert_eq!(logs[0].fields["revision"], "1");
        runtime.set_clock(Duration::from_millis(251)).unwrap();
        let logs = runtime
            .dispatch(EventKind::Ready, &serde_json::json!({}))
            .unwrap();
        assert_eq!(
            logs[0].fields,
            serde_json::json!({"devices":false,"seat":false})
        );
    }
    #[test]
    fn catalog_removal_mapping_changes_and_old_generations_are_distinct() {
        let mut runtime = definition().instantiate().unwrap();
        runtime
            .set_context(
                crate::context_transfer::tests::view(Duration::ZERO),
                Duration::ZERO,
            )
            .unwrap();
        let info = metadata(1);
        runtime
            .dispatch(
                EventKind::Input,
                &payload(info.clone(), ResetReason::Attached),
            )
            .unwrap();
        let mut changed = info.clone();
        changed.mapping.as_mut().unwrap().generation = 9;
        changed.mapping.as_mut().unwrap().output = Some(crate::gestures::Output {
            name: "mapped".into(),
            pixel_width: 200.0,
            pixel_height: 400.0,
            scale: 2.0,
            origin: crate::gestures::Point { x: 10.0, y: 20.0 },
            transform: crate::gestures::Transform::Normal,
        });
        let logs = runtime
            .dispatch(
                EventKind::Input,
                &payload(changed, ResetReason::MappingChanged),
            )
            .unwrap();
        assert_eq!(logs[0].fields["revision"], "2");
        runtime
            .dispatch(EventKind::Input, &payload(info, ResetReason::Removed))
            .unwrap();
        assert!(runtime.host.borrow().devices.entries.is_empty());
        assert_eq!(runtime.host.borrow().devices.revision, 3);
        runtime
            .dispatch(
                EventKind::Input,
                &payload(metadata(2), ResetReason::Attached),
            )
            .unwrap();
        assert!(
            runtime
                .dispatch(
                    EventKind::Input,
                    &payload(metadata(1), ResetReason::Attached)
                )
                .is_err()
        );
        assert!(
            runtime
                .dispatch(EventKind::Ready, &serde_json::json!({}))
                .is_err()
        );
    }
    #[test]
    fn catalog_is_bounded_and_context_inhibition_retires_it() {
        let definition = definition();
        let config = definition.validate().unwrap();
        let mut catalog = Catalog::default();
        for index in 0..MAX_DEVICES {
            let mut info = metadata(1);
            info.device.source.device = index as u64;
            catalog
                .apply(&config, &payload(info, ResetReason::Attached))
                .unwrap();
        }
        let mut info = metadata(1);
        info.device.source.device = 1000;
        assert!(
            catalog
                .apply(&config, &payload(info, ResetReason::Attached))
                .is_err()
        );
        assert_eq!(catalog.entries.len(), MAX_DEVICES);
        assert!(catalog.bytes <= MAX_BYTES);
        let mut view = crate::context_transfer::tests::view(Duration::ZERO);
        view.state = crate::context::State::Locked;
        catalog.context(&view);
        assert!(catalog.entries.is_empty());
        assert!(!catalog.received);
    }
    #[test]
    fn catalog_byte_limit_rejects_an_update_without_exceeding_the_bound() {
        let config = definition().validate().unwrap();
        let mut catalog = Catalog::default();
        let mut info = metadata(1);
        info.device.name = "x".repeat(4096);
        info.device.path = "x".repeat(4096);
        info.device.physical = "x".repeat(4096);
        info.device.unique = "x".repeat(4096);
        info.device
            .capabilities
            .insert(crate::input::EV_KEY, (0..4096).collect());
        let mut rejected = false;
        for index in 0..MAX_DEVICES {
            info.device.source.device = index as u64;
            let event = payload(info.clone(), ResetReason::Attached);
            assert!(serde_json::to_vec(&event).unwrap().len() < 64 * 1024);
            if catalog.apply(&config, &event).is_err() {
                rejected = true;
                break;
            }
        }
        assert!(rejected);
        assert!(catalog.entries.len() < MAX_DEVICES);
        assert!(catalog.bytes <= MAX_BYTES);
    }
}
