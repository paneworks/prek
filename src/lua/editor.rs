//! Editor metadata for the implemented host; never loaded by the Lua runtime.

/// Lua Language Server definitions, also installed in share/prek/lua/prek.lua.
/// This export is offline and performs no device or configuration access.
pub fn editor_definitions() -> &'static str {
    include_str!("../../docs/lua/prek.lua")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::{Definition, api};
    use std::collections::BTreeSet;

    #[test]
    fn editor_exports_match_the_live_manifest_and_parse_with_the_pinned_luna() {
        let documented: BTreeSet<_> = editor_definitions()
            .lines()
            .filter_map(|line| line.strip_prefix("function "))
            .map(|line| {
                let name = line.split_once('(').unwrap().0;
                if let Some((class, method)) = name.split_once('.')
                    && let Some(namespace) = class.strip_suffix("API")
                {
                    format!("{}.{method}", namespace.to_ascii_lowercase())
                } else {
                    name.replace(':', ".")
                }
            })
            .collect();
        let manifest: BTreeSet<_> = api()["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|symbol| symbol["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(documented, manifest);

        // Parse the shipped file as ordinary Lua, then prove it cannot supply
        // a silent no-op runtime in place of the embedded host.
        let source = format!(
            "local function metadata()\n{}\nend\nlocal e=metadata(); assert(type(e.config.preset)=='function'); assert(not pcall(e.config.preset,'minimal')); return {{version=1}}",
            editor_definitions()
        );
        Definition::compile(&source, "editor-syntax.lua").unwrap();
    }

    #[test]
    fn every_editor_field_is_described_by_the_documentation() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut documentation = std::fs::read_to_string(root.join("README.md")).unwrap();
        for entry in std::fs::read_dir(root.join("docs")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|extension| extension == "md") {
                documentation.push_str(&std::fs::read_to_string(path).unwrap());
            }
        }
        let word = |text: &str, name: &str| {
            text.match_indices(name).any(|(index, _)| {
                let boundary = |c: char| !c.is_ascii_alphanumeric() && c != '_';
                text[..index].chars().next_back().is_none_or(boundary)
                    && text[index + name.len()..]
                        .chars()
                        .next()
                        .is_none_or(boundary)
            })
        };
        let mut missing = Vec::new();
        let mut lines = editor_definitions().lines().peekable();
        while let Some(line) = lines.next() {
            let Some(class) = line.strip_prefix("---@class (exact) prek.") else {
                continue;
            };
            while let Some(field) = lines.next_if(|line| line.starts_with("---@field ")) {
                let name = field["---@field ".len()..]
                    .split([' ', '?'])
                    .next()
                    .unwrap();
                if !word(&documentation, name) {
                    missing.push(format!("{class}.{name}"));
                }
            }
        }
        assert!(missing.is_empty(), "undocumented fields: {missing:?}");
    }

    #[test]
    fn editor_configuration_fields_match_serialized_rust_models() {
        use crate::config::{Config, Key, Layer, LayerActivation, NamedMode, Selector};

        fn fields(class: &str) -> BTreeSet<String> {
            let declaration = format!("---@class (exact) prek.{class}");
            editor_definitions()
                .lines()
                .skip_while(|line| *line != declaration)
                .skip(1)
                .take_while(|line| !line.starts_with("---@class") && !line.starts_with("---@alias"))
                .filter_map(|line| line.strip_prefix("---@field "))
                .map(|field| {
                    field
                        .split_whitespace()
                        .next()
                        .unwrap()
                        .trim_end_matches('?')
                        .to_owned()
                })
                .collect()
        }
        let mut config = Config::preset("desktop").unwrap();
        assert!(
            serde_json::to_value(&config)
                .unwrap()
                .get("recognizers")
                .is_none()
        );
        config.policy.allow_input_events = true;
        config.recognizers.push(crate::recognition::Definition {
            name: "editor".into(),
            selectors: BTreeSet::from(["keyboard".into()]),
            inputs: BTreeSet::from([crate::recognition::InputKind::Key]),
            reply_within: crate::config::Millis(100),
            continuous: false,
        });
        config.validate().unwrap();
        let layer = Layer {
            id: "test".into(),
            key: Key(30),
            toggle: false,
            activation: LayerActivation::Physical,
        };
        for (name, model) in [
            (
                "CustomEvidence",
                serde_json::to_value(crate::rules::Evidence::Custom {
                    recognizer: "editor".into(),
                    data: serde_json::Value::Null,
                })
                .unwrap(),
            ),
            (
                "SequenceEvidence",
                serde_json::to_value(crate::rules::Evidence::Sequence { steps: vec![] }).unwrap(),
            ),
            ("Config", serde_json::to_value(&config).unwrap()),
            (
                "RecognizerDeclaration",
                serde_json::to_value(&config.recognizers[0]).unwrap(),
            ),
            ("Rule", serde_json::to_value(&config.rules[0]).unwrap()),
            ("Layer", serde_json::to_value(layer).unwrap()),
            (
                "NamedMode",
                serde_json::to_value(NamedMode {
                    id: "navigate".into(),
                    active: false,
                })
                .unwrap(),
            ),
            ("Timings", serde_json::to_value(config.timing).unwrap()),
            (
                "ShapeCapture",
                serde_json::to_value(config.shape_capture).unwrap(),
            ),
            ("Policy", serde_json::to_value(config.policy).unwrap()),
            (
                "Selector",
                serde_json::to_value(Selector::default()).unwrap(),
            ),
        ] {
            assert_eq!(
                fields(name),
                model.as_object().unwrap().keys().cloned().collect(),
                "{name}"
            );
        }
    }
}
