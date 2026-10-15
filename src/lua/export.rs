//! Deterministic, bounded Lua export of validated declarative configurations.
use super::{Config, ConfigError, MAX_CONFIG_BYTES, error};
use crate::config::Action;
use serde_json::Value;

/// Export the complete model, preserving values rather than preset references.
/// Closures and runtime registrations require their original approved source.
pub fn export_config(config: &Config) -> Result<String, ConfigError> {
    config.validate()?;
    if config
        .rules
        .iter()
        .any(|rule| matches!(rule.action, Action::Lua { .. }))
    {
        return Err(error(
            "named Lua handlers require their original approved source",
        ));
    }
    super::runtime::bounded_json(config, MAX_CONFIG_BYTES)
        .map_err(|_| error("configuration export model exceeds 1 MiB"))?;
    // Keep the original f64 values. Parsing serialized JSON with its default
    // fast float parser can change shape coordinates by one bit.
    let value = serde_json::to_value(config).map_err(error)?;
    let mut output = Output(String::new());
    output.push("local prek = require(\"prek\")\nreturn prek.config.new ")?;
    output.value(&value, 0)?;
    output.push("\n")?;
    Ok(output.0)
}

struct Output(String);
impl Output {
    fn push(&mut self, text: &str) -> Result<(), ConfigError> {
        if self.0.len() + text.len() > MAX_CONFIG_BYTES {
            return Err(error("exported Lua configuration exceeds 1 MiB"));
        }
        self.0.push_str(text);
        Ok(())
    }
    fn indent(&mut self, depth: usize) -> Result<(), ConfigError> {
        self.push(&"  ".repeat(depth))
    }
    fn string(&mut self, text: &str) -> Result<(), ConfigError> {
        self.push("\"")?;
        for character in text.chars() {
            match character {
                '\\' => self.push("\\\\")?,
                '"' => self.push("\\\"")?,
                '\n' => self.push("\\n")?,
                '\r' => self.push("\\r")?,
                '\t' => self.push("\\t")?,
                value if value.is_ascii_control() => {
                    self.push(&format!("\\{:03}", value as u32))?
                }
                value => self.push(value.encode_utf8(&mut [0; 4]))?,
            }
        }
        self.push("\"")
    }
    fn value(&mut self, value: &Value, depth: usize) -> Result<(), ConfigError> {
        if depth > 64 {
            return Err(error("Lua export exceeds value nesting limit"));
        }
        match value {
            Value::Null => self.push("nil"),
            Value::Bool(value) => self.push(if *value { "true" } else { "false" }),
            Value::Number(value) => self.push(&value.to_string()),
            Value::String(value) => self.string(value),
            Value::Array(values) => {
                self.push("{")?;
                for value in values {
                    self.push("\n")?;
                    self.indent(depth + 1)?;
                    self.value(value, depth + 1)?;
                    self.push(",")?;
                }
                if !values.is_empty() {
                    self.push("\n")?;
                    self.indent(depth)?;
                }
                self.push("}")
            }
            Value::Object(values) => {
                self.push("{")?;
                for (key, value) in values {
                    self.push("\n")?;
                    self.indent(depth + 1)?;
                    self.push("[")?;
                    self.string(key)?;
                    self.push("] = ")?;
                    self.value(value, depth + 1)?;
                    self.push(",")?;
                }
                if !values.is_empty() {
                    self.push("\n")?;
                    self.indent(depth)?;
                }
                self.push("}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{EffectKind, Key, Trigger},
        lua::{Definition, EventKind},
    };
    use std::collections::BTreeSet;

    #[test]
    fn full_presets_and_literal_strings_round_trip_through_luna() {
        for name in ["minimal", "desktop", "phone"] {
            let config = Config::preset(name).unwrap();
            let source = export_config(&config).unwrap();
            assert_eq!(source, export_config(&config).unwrap());
            assert_eq!(
                super::super::compile(&source, "export.lua").unwrap(),
                config
            );
        }
        let mut config = Config::preset("desktop").unwrap();
        config.policy.allow_power_actions = true;
        config.policy.allow_hyprland_actions = true;
        config.rules[0].output = Some("eDP-1".into());
        let base = config.rules[0].clone();
        config.rules.clear();
        for (index, action) in [
            Action::Command {
                argv: vec![
                    "printf".into(),
                    "\"\\\n\t\u{0001}7✓ ]]; os.execute('bad') --".into(),
                ],
            },
            Action::Keys {
                keys: vec![Key(30)],
            },
            Action::Remap { key: Key(48) },
            Action::Event {
                name: "quoted \" name\nline".into(),
            },
        ]
        .into_iter()
        .enumerate()
        {
            let mut rule = base.clone();
            rule.id = format!("export-{index}");
            rule.trigger = Trigger::Key { key: Key(30) };
            rule.action = action;
            config.rules.push(rule);
        }
        let source = export_config(&config).unwrap();
        assert_eq!(
            super::super::compile(&source, "literals.lua").unwrap(),
            config
        );
        let definition = Definition::compile("local p=require('prek');local c=p.config.preset('minimal');assert(type(p.config.export(c))=='string');return c", "api-export.lua").unwrap();
        assert_eq!(
            definition.validate().unwrap(),
            Config::preset("minimal").unwrap()
        );
    }

    #[test]
    fn export_rejects_missing_closures_oversize_and_runtime_use() {
        let mut config = Config::preset("desktop").unwrap();
        config.rules[0].action = Action::Lua {
            handler: "missing".into(),
            requires: BTreeSet::from([EffectKind::Commands]),
        };
        assert!(
            export_config(&config)
                .unwrap_err()
                .0
                .contains("original approved source")
        );
        config.rules[0].action = Action::Command {
            argv: (0..256).map(|_| "x".repeat(4096)).collect(),
        };
        assert!(
            export_config(&config)
                .unwrap_err()
                .0
                .contains("exceeds 1 MiB")
        );
        let definition = Definition::compile("local p=require('prek');p.events.on('matched',function()p.config.export(p.config.preset('minimal'))end);return {version=1}", "runtime-export.lua").unwrap();
        assert!(
            definition
                .instantiate()
                .unwrap()
                .dispatch(EventKind::Matched, &serde_json::json!({}))
                .is_err()
        );
    }
}
