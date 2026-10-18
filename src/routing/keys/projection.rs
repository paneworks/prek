use super::*;
use crate::{
    input::{EV_KEY, Frame, InputPosition},
    router::{InputSpan, KeyClaim},
    rules::Stage,
};

impl Routing {
    pub fn input_span(&self, source: &Source, ticket: u64) -> Option<InputSpan> {
        self.routes.get(source)?.router.input_span(ticket)
    }

    pub fn key_input_position(&self, frame: &Frame, event: usize) -> Option<InputPosition> {
        let (watermark, original) = self.last_key_input.as_ref()?;
        (self.ordered_keys && original == frame && event < frame.events.len()).then_some(
            InputPosition {
                frame: *watermark,
                event,
            },
        )
    }

    pub(crate) fn key_repeat_position(&self, frame: &Frame, event: usize) -> Option<InputPosition> {
        let position = self.key_input_position(frame, event)?;
        Some(if self.routes.get(&frame.source)?.project_keys {
            position
        } else {
            InputPosition::after_frame(position.frame)
        })
    }

    pub(crate) fn key_position_prefix_ready(
        &self,
        position: InputPosition,
        inclusive: bool,
    ) -> bool {
        self.ordered_keys
            && position.frame <= self.engine.input_watermark()
            && self
                .routes
                .values()
                .all(|route| route.router.position_prefix_delivered(position, inclusive))
    }

    pub(in crate::routing) fn ordered_position(
        &self,
        intent: &RouteIntent,
    ) -> Option<InputPosition> {
        if !self.ordered_keys {
            return None;
        }
        let position = intent
            .position()
            .unwrap_or(InputPosition::after_frame(self.engine.input_watermark()));
        Some(
            if self
                .routes
                .get(intent.source())
                .is_some_and(|route| route.project_keys)
            {
                position
            } else {
                InputPosition::after_frame(position.frame)
            },
        )
    }

    pub(in crate::routing) fn project_key_frame(
        &mut self,
        source: &Source,
        frame: Frame,
        previous: &[RouteIntent],
        outcomes: &[RouteOutcome],
        now: Duration,
    ) -> io::Result<()> {
        let mut eligible: Vec<_> = outcomes
            .iter()
            .filter(|outcome| {
                outcome.intent.source() == source
                    && matches!(outcome.intent.rule.trigger, Trigger::Key { .. })
                    && self.available(&outcome.intent, now)
            })
            .collect();
        eligible.sort_by(|a, b| {
            b.intent
                .rule
                .priority
                .cmp(&a.intent.rule.priority)
                .then_with(|| a.intent.rule.id.cmp(&b.intent.rule.id))
        });
        let watermark = self.engine.input_watermark();
        let mut winners = BTreeMap::new();
        let mut cuts = BTreeSet::new();
        for outcome in &eligible {
            let position = outcome
                .intent
                .position()
                .ok_or_else(|| io::Error::other("key outcome has no original position"))?;
            let Trigger::Key { key } = outcome.intent.rule.trigger else {
                unreachable!()
            };
            let event = frame
                .events
                .get(position.event)
                .filter(|event| {
                    event.kind == EV_KEY && event.code == key.0 && position.frame == watermark
                })
                .ok_or_else(|| io::Error::other("key outcome differs from original input"))?;
            if outcome.intent.rule.delivery == Delivery::Observe {
                if matches!(
                    outcome.intent.rule.action,
                    Action::Keys { .. } | Action::Remap { .. }
                ) {
                    cuts.insert(position.event);
                }
            } else if matches!(
                outcome.event,
                Event::Matched {
                    stage: Stage::Begin,
                    ..
                }
            ) && event.value == 1
            {
                winners
                    .entry(position.event)
                    .or_insert_with(|| outcome.intent.clone());
            }
        }
        for (index, event) in frame
            .events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.kind == EV_KEY && event.value == 2)
        {
            if previous.iter().chain(eligible.iter().filter(|outcome| matches!(outcome.event, Event::Matched { stage: Stage::Begin, .. }) && outcome.intent.position().is_some_and(|position| position.event < index)).map(|outcome| &outcome.intent))
                .any(|intent| intent.source() == source && intent.rule.delivery == Delivery::Observe && matches!(intent.rule.action, Action::Remap { .. }) && matches!(intent.rule.trigger, Trigger::Key { key } if key.0 == event.code) && self.available(intent, now)
                    && !outcomes.iter().any(|outcome| outcome.intent.id() == intent.id() && matches!(outcome.event, Event::Matched { stage: Stage::End, .. }) && outcome.intent.position().is_some_and(|position| position.event < index))) {
                cuts.insert(index);
            }
        }
        let Some(route) = self.routes.get_mut(source) else {
            return Ok(());
        };
        let claims: Vec<_> = winners
            .iter()
            .map(|(&event, intent)| KeyClaim {
                event,
                id: intent.id(),
                authorization: route.authorization,
            })
            .collect();
        let input_at = frame.at;
        if let Err(error) = route
            .router
            .push_key_frame(frame, watermark, &claims, &cuts, input_at)
        {
            self.recover(source);
            return Err(fault(error));
        }
        for intent in winners.into_values() {
            self.won.insert(
                intent.id(),
                Won {
                    intent,
                    emitted: false,
                    chain: None,
                },
            );
        }
        Ok(())
    }
}
