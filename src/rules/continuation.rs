//! Native-owned held-contact candidates and rebased motion recognizers.
use super::*;

pub(super) fn candidate(
    previous: &Candidate,
    state: &DeviceState,
    authority: &Authority,
    at: Duration,
    watermark: u64,
    id: u64,
) -> Result<Candidate, &'static str> {
    if previous
        .cancelled
        .load(std::sync::atomic::Ordering::Acquire)
        || previous.contacts.is_empty()
        || previous.source != state.device.source
        || previous.contacts != state.contacts.iter().map(|contact| contact.id).collect()
        || state
            .last_input
            .is_none_or(|(_, physical_at)| physical_at > at)
        || previous.session_generation != authority.generation()
        || previous
            .permit
            .as_ref()
            .is_none_or(|permit| authority.validate(permit, input(state, at)).is_err())
    {
        return Err("touch continuation held state changed");
    }
    let mut candidate = previous.clone();
    candidate.id = id;
    candidate.cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    candidate.position = None;
    candidate.key_start = None;
    candidate.key_end = None;
    candidate.key_tap_end = None;
    candidate.key_presses = presses::Set::empty(state.press_budget.clone());
    candidate.tails = tails::Set::empty(state.press_budget.clone());
    candidate.ordinal = 0;
    candidate.progress = false;
    candidate.started = at;
    candidate.started_input = watermark;
    candidate.continued_from = Some(previous.id);
    candidate.expires = None;
    Ok(candidate)
}

pub(super) fn motion(
    worker: &mut Worker,
    state: &DeviceState,
    previous: &Candidate,
    context: &SequenceContext<'_>,
    config: &Config,
    next_id: &mut u64,
) -> Result<(), &'static str> {
    let at = context.admitted_at;
    if !conditions(&worker.rule, context.keys, &state.device.source.seat)
        || !context
            .authority
            .fresh(at)
            .is_ok_and(|snapshot| snapshot.state == crate::context::State::Desktop)
    {
        return Err("touch continuation is no longer eligible");
    }
    let id = next_id
        .checked_add(1)
        .ok_or("native candidate IDs exhausted")?;
    let mut candidate = candidate(
        previous,
        state,
        context.authority,
        at,
        context.input_watermark,
        id,
    )?;
    let mapping = state
        .mapping
        .as_ref()
        .ok_or("touch continuation mapping is unavailable")?;
    let source = &state.device.source;
    match &mut worker.recognizer {
        Recognizer::Touch(touches) => touches
            .get_mut(&source.device)
            .ok_or("touch continuation recognizer is unavailable")?
            .continue_contacts(source, mapping, &state.contacts, at)?,
        Recognizer::Shape { captures, .. } => {
            let identity = CaptureIdentity {
                source: source.clone(),
                mapping_generation: mapping.generation,
                contacts: previous.contacts.clone(),
            };
            let positions = state
                .contacts
                .iter()
                .map(|contact| {
                    mapping
                        .map(contact.x, contact.y)
                        .map(|position| (contact.id, position))
                })
                .collect::<Result<BTreeMap<_, _>, _>>()?;
            let mut path = PathCapture::new(
                identity.clone(),
                at,
                config
                    .shape_capture
                    .touch_limits(worker.rule.keys_throughout),
                true,
            )
            .map_err(|_| "touch continuation capture refused")?;
            path.sample_contacts(source, mapping.generation, at, &positions, true)
                .map_err(|_| "touch continuation sample refused")?;
            candidate.expires = Some(path.deadline());
            captures.insert(
                source.clone(),
                ShapeCapture {
                    started: at,
                    path: Some(path),
                    identity,
                },
            );
        }
        _ => return Err("unsupported native touch continuation"),
    }
    worker.touch_active.insert(source.clone());
    worker.released.remove(source);
    worker.candidates.insert(source.clone(), candidate);
    *next_id = id;
    Ok(())
}
