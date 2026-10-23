//! Per-device Hyprland settings for a physical device's routed mirror.
use super::*;

const KEYBOARD_FIELDS: [(&str, &str); 5] = [
    ("kb_rules", "rules"),
    ("kb_model", "model"),
    ("kb_layout", "layout"),
    ("kb_variant", "variant"),
    ("kb_options", "options"),
];

#[derive(Clone, Copy)]
enum Kind {
    Text,
    Integer,
    Float,
    Boolean,
}

/// Lua `hl.device` field and value kind for a declarable hyprlang option.
fn lua_field(option: &str) -> Option<(&'static str, Kind)> {
    Some(match option {
        "kb_rules" => ("kb_rules", Kind::Text),
        "kb_model" => ("kb_model", Kind::Text),
        "kb_layout" => ("kb_layout", Kind::Text),
        "kb_variant" => ("kb_variant", Kind::Text),
        "kb_options" => ("kb_options", Kind::Text),
        "repeat_rate" => ("repeat_rate", Kind::Integer),
        "repeat_delay" => ("repeat_delay", Kind::Integer),
        "numlock_by_default" => ("numlock_by_default", Kind::Boolean),
        "resolve_binds_by_sym" => ("resolve_binds_by_sym", Kind::Boolean),
        "sensitivity" => ("sensitivity", Kind::Float),
        "accel_profile" => ("accel_profile", Kind::Text),
        "scroll_points" => ("scroll_points", Kind::Text),
        "natural_scroll" => ("natural_scroll", Kind::Boolean),
        "scroll_factor" => ("scroll_factor", Kind::Float),
        "left_handed" => ("left_handed", Kind::Boolean),
        "middle_button_emulation" => ("middle_button_emulation", Kind::Boolean),
        "tap-to-click" => ("tap_to_click", Kind::Boolean),
        "tap-and-drag" => ("tap_and_drag", Kind::Boolean),
        "drag_lock" => ("drag_lock", Kind::Integer),
        "drag_3fg" => ("drag_3fg", Kind::Integer),
        "disable_while_typing" => ("disable_while_typing", Kind::Boolean),
        "clickfinger_behavior" => ("clickfinger_behavior", Kind::Boolean),
        "tap_button_map" => ("tap_button_map", Kind::Text),
        "scroll_method" => ("scroll_method", Kind::Text),
        "scroll_button" => ("scroll_button", Kind::Integer),
        "scroll_button_lock" => ("scroll_button_lock", Kind::Boolean),
        "rotation" => ("rotation", Kind::Integer),
        "flip_x" => ("flip_x", Kind::Boolean),
        "flip_y" => ("flip_y", Kind::Boolean),
        "output" => ("output", Kind::Text),
        "transform" => ("transform", Kind::Integer),
        _ => return None,
    })
}

fn lua_value(kind: Kind, value: &str) -> io::Result<String> {
    let refused = || invalid("device setting value does not fit its Hyprland type");
    match kind {
        Kind::Text => Ok(format!(
            "\"{}\"",
            value
                .bytes()
                .map(|byte| format!("\\{byte:03}"))
                .collect::<String>()
        )),
        Kind::Integer => value
            .parse::<i64>()
            .map(|number| number.to_string())
            .map_err(|_| refused()),
        Kind::Float => value
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite())
            .map(|number| format!("{number:?}"))
            .ok_or_else(refused),
        Kind::Boolean => match value {
            "1" | "true" | "yes" | "on" => Ok("true".into()),
            "0" | "false" | "no" | "off" => Ok("false".into()),
            _ => Err(refused()),
        },
    }
}

/// One request that writes `settings` for the Hyprland device named `mirror`
/// and applies them to an existing device before the reply.
fn request(
    dialect: dispatch::Dialect,
    mirror: &str,
    settings: &BTreeMap<String, String>,
) -> io::Result<String> {
    if settings
        .iter()
        .any(|(option, value)| lua_field(option).is_none() || !crate::config::hyprland_value(value))
    {
        return Err(invalid(
            "device setting is not a declarable Hyprland option",
        ));
    }
    match dialect {
        dispatch::Dialect::Hyprlang => {
            if mirror
                .chars()
                .any(|character| character.is_control() || "[]#;=$".contains(character))
            {
                return Err(invalid("mirror device name is unsafe for hyprlang"));
            }
            let lines: Vec<_> = settings
                .iter()
                .map(|(option, value)| format!("keyword device[{mirror}]:{option} {value}"))
                .collect();
            Ok(format!("[[BATCH]]{}", lines.join(";")))
        }
        dispatch::Dialect::Lua => {
            let mut fields = vec![format!("name={}", lua_value(Kind::Text, mirror)?)];
            for (option, value) in settings {
                let (field, kind) = lua_field(option).unwrap();
                fields.push(format!("{field}={}", lua_value(kind, value)?));
            }
            Ok(format!("r/eval hl.device({{{}}})", fields.join(",")))
        }
    }
}

/// Name-specific settings visible for the original's inventory entry: XKB
/// fields that differ from the global value, and an explicit scroll factor.
fn visible(
    class: Class,
    original: &Value,
    globals: &[Value],
) -> io::Result<BTreeMap<String, String>> {
    let mut settings = BTreeMap::new();
    match class {
        Class::Keyboard | Class::Buttons => {
            for ((option, field), global) in KEYBOARD_FIELDS.iter().zip(globals) {
                let value = original
                    .get(field)
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("compositor keyboard setting is missing"))?;
                let global = global
                    .get("str")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("compositor global keyboard setting is missing"))?;
                if value != global {
                    settings.insert(option.to_string(), value.into());
                }
            }
        }
        Class::Mouse | Class::Touchpad => {
            let factor = original
                .get("scrollFactor")
                .and_then(Value::as_f64)
                .ok_or_else(|| invalid("compositor scroll factor is missing"))?;
            if factor != -1.0 {
                settings.insert("scroll_factor".into(), format!("{factor:?}"));
            }
        }
        Class::Touchscreen | Class::Switches => {}
    }
    if settings
        .values()
        .any(|value| !crate::config::hyprland_value(value))
    {
        return Err(invalid(
            "original device setting cannot be copied safely to its mirror",
        ));
    }
    Ok(settings)
}

/// The inventory group and Hyprland name of a device with this class and
/// kernel name.
pub(super) fn identity(class: Class, name: &str) -> (&'static str, String) {
    match class {
        Class::Keyboard | Class::Buttons => ("keyboards", internal_name(name)),
        Class::Mouse | Class::Touchpad => ("mice", internal_name(name)),
        Class::Touchscreen => ("touch", internal_name(name)),
        Class::Switches => ("switches", name.into()),
    }
}

/// The only inventory entry named exactly `name`; a `-N` duplicate is an error.
pub(super) fn unique<'a>(
    inventory: &'a Value,
    group: &str,
    name: &str,
) -> io::Result<Option<&'a Value>> {
    let matches: Vec<_> = inventory[group]
        .as_array()
        .unwrap()
        .iter()
        .filter(|device| {
            let candidate = device["name"].as_str().unwrap();
            candidate == name
                || candidate
                    .strip_prefix(&format!("{name}-"))
                    .is_some_and(|suffix| {
                        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                    })
        })
        .collect();
    if matches.is_empty() {
        return Ok(None);
    }
    if matches.len() != 1 || matches[0]["name"].as_str() != Some(name) {
        return Err(invalid(
            "compositor device identity is ambiguous or renamed",
        ));
    }
    Ok(Some(matches[0]))
}

impl Client {
    /// Copies the original's visible name-specific settings and the declared
    /// `consumer_settings` onto a physical mirror's Hyprland device name.
    pub fn configure_mirror(&self, descriptor: &Descriptor) -> io::Result<()> {
        descriptor.validate().map_err(invalid)?;
        let original = descriptor
            .physical_origin()
            .ok_or_else(|| invalid("only a physical mirror has an original device"))?;
        let until = Instant::now() + REQUEST_TIME;
        let bytes = self.exchange(
            "[[BATCH]]j/devices;j/getoption input:kb_rules;j/getoption input:kb_model;\
             j/getoption input:kb_layout;j/getoption input:kb_variant;j/getoption input:kb_options",
            until,
        )?;
        let values = serde_json::Deserializer::from_slice(&bytes)
            .into_iter::<Value>()
            .collect::<Result<Vec<_>, _>>()
            .map_err(io::Error::other)?;
        let [inventory, globals @ ..] = values.as_slice() else {
            return Err(invalid("compositor device batch is incomplete"));
        };
        if globals.len() != KEYBOARD_FIELDS.len() {
            return Err(invalid("compositor device batch is incomplete"));
        }
        validate_inventory(inventory)?;
        let (group, name) = identity(descriptor.class, &original.name);
        let mut settings = match unique(inventory, group, &name)? {
            Some(entry) => visible(descriptor.class, entry, globals)?,
            None => BTreeMap::new(),
        };
        settings.extend(descriptor.consumer_settings.clone());
        if settings.is_empty() {
            return Ok(());
        }
        let (_, mirror) = identity(descriptor.class, &descriptor.name);
        let request = request(self.dispatch_dialect_until(until)?, &mirror, &settings)?;
        let reply = self.exchange(&request, until)?;
        let reply = String::from_utf8_lossy(&reply);
        if reply.split("\n\n\n").all(|part| part.trim() == "ok") {
            Ok(())
        } else {
            Err(invalid("compositor refused a mirror device setting"))
        }
    }
}

#[cfg(test)]
mod tests;
