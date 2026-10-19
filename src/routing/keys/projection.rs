use super::*;
use crate::{
    input::{Frame, InputPosition},
    router::InputSpan,
};

impl Routing {
    pub(crate) fn software_key_scope(
        &self,
        fence: crate::protocol::Fence,
        at: std::time::Duration,
    ) -> Option<(u64, InputPosition, std::time::Duration)> {
        if !self.ordered_keys
            || !self.key_lane(fence, at)
            || self.authority().state(at) != crate::context::State::Desktop
        {
            return None;
        }
        let snapshot = self.authority().fresh(at).ok()?;
        let until = snapshot
            .captured_at
            .saturating_add(crate::context::SNAPSHOT_MAX_AGE)
            .saturating_add(std::time::Duration::from_nanos(1));
        Some((
            self.key_epoch,
            InputPosition::after_frame(self.engine.input_watermark()),
            until.min(self.capability_expires),
        ))
    }
    pub(crate) fn software_key_prefix(
        &self,
        seat: &str,
    ) -> std::collections::BTreeMap<Source, u64> {
        self.routes
            .iter()
            .filter(|(source, route)| source.seat == seat && route.key_domain)
            .map(|(source, _)| {
                (
                    source.clone(),
                    self.engine.source_input_sequence(source).unwrap_or(0),
                )
            })
            .collect()
    }
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
        seat: &str,
    ) -> bool {
        self.ordered_keys
            && position.frame <= self.engine.input_watermark()
            && self
                .routes
                .iter()
                .filter(|(source, route)| source.seat == seat && route.key_domain)
                .map(|(_, route)| route)
                .all(|route| route.router.position_prefix_delivered(position, inclusive))
    }

    pub(in crate::routing) fn ordered_position(
        &self,
        intent: &RouteIntent,
    ) -> Option<InputPosition> {
        if !self.ordered_keys {
            return None;
        }
        let position = (if self.split_inputs {
            intent.observation_position()
        } else {
            intent.position()
        })
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
        self.stage_key_input(source, frame, previous, outcomes, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Key,
        input::{EV_KEY, EV_SYN, RawEvent, SYN_REPORT},
        keys::worker::tests::{ms, projected, rule, source},
    };

    #[test]
    fn a_rewritten_trigger_and_frame_cannot_claim_a_different_original_press() {
        let mut rule = rule("origin", Action::Remap { key: Key(42) });
        rule.delivery = Delivery::Buffer;
        let mut routing = projected(vec![rule]);
        let source = source(1);
        let mut frame = Frame {
            source: source.clone(),
            at: ms(10),
            sequence: 1,
            events: vec![
                RawEvent {
                    kind: EV_KEY,
                    code: 30,
                    value: 1,
                },
                RawEvent {
                    kind: EV_SYN,
                    code: SYN_REPORT,
                    value: 0,
                },
            ],
        };
        routing
            .engine
            .push(Record::Frame {
                frame: frame.clone(),
            })
            .unwrap();
        let mut outcomes = routing.engine.take_route_outcomes();
        assert_eq!(outcomes.len(), 1);
        outcomes[0].intent.rule.trigger = Trigger::Key { key: Key(31) };
        frame.events[0].code = 31;
        assert!(
            routing
                .project_key_frame(&source, frame, &[], &outcomes, ms(10))
                .is_err()
        );
        assert_eq!(routing.take_recoveries().len(), 1);
    }
}
