//! Atomic preparation of a bounded broker-owned source snapshot and catalogs.
use crate::{
    config::Config,
    lua::{Definition, HandlerRegistration, RecognizerBinding},
};
use std::{io, sync::Arc};

#[derive(Default)]
pub(super) struct Prepared {
    pub baseline: Option<Arc<Config>>,
    pub bytes: Option<Vec<u8>>,
    pub handlers: Vec<HandlerRegistration>,
    pub recognizers: Vec<RecognizerBinding>,
}
impl Prepared {
    pub fn new(definition: Option<&Definition>) -> io::Result<Self> {
        let Some(definition) = definition else {
            return Ok(Self::default());
        };
        let config = definition.validate().map_err(io::Error::other)?;
        crate::worker::preflight(&config)?;
        let bytes = crate::lua::bounded_json(definition, crate::lua::MAX_SNAPSHOT_BYTES)?;
        Ok(Self {
            baseline: Some(Arc::new(config)),
            bytes: Some(bytes),
            handlers: definition.handlers.clone(),
            recognizers: definition.recognizers.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Millis, Trigger};
    #[test]
    fn complete_composed_source_catalogs_use_the_same_bounded_broker_install_preparation() {
        let definition = Definition::compile(
            include_str!("../../examples/lua/composed_recognizers.lua"),
            "composed.lua",
        )
        .unwrap();
        let prepared = Prepared::new(Some(&definition)).unwrap();
        assert!(matches!(
            prepared.baseline.as_ref().unwrap().rules[0].trigger,
            Trigger::Sequence { .. }
        ));
        assert_eq!(prepared.recognizers, definition.recognizers);
        assert_eq!(
            serde_json::from_slice::<Definition>(prepared.bytes.as_ref().unwrap()).unwrap(),
            definition
        );
        for change in ["missing", "metadata", "subscription", "selector"] {
            let mut changed = definition.clone();
            match change {
                "missing" => {
                    changed.recognizers.pop();
                }
                "metadata" => changed.recognizers[1].definition.reply_within = Millis(99),
                "subscription" => {
                    changed.recognizers[1].subscription = changed.recognizers[0].subscription
                }
                _ => {
                    changed.recognizers[1]
                        .definition
                        .selectors
                        .insert("unapproved".into());
                }
            }
            assert!(Prepared::new(Some(&changed)).is_err(), "{change}");
            assert_eq!(prepared.recognizers, definition.recognizers);
        }
        assert!(Prepared::new(None).unwrap().bytes.is_none());
    }
}
