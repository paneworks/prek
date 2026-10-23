use super::*;
use crate::input::{EV_SYN, SYN_REPORT};

fn events(values: &[(u16, u16, i32)]) -> Vec<InputEvent> {
    values
        .iter()
        .map(|&(kind, code, value)| InputEvent::new(kind, code, value))
        .collect()
}

#[test]
fn a_prefix_is_balanced_only_when_every_press_contact_and_switch_ends_released() {
    let report = (EV_SYN, SYN_REPORT, 0);
    assert!(balanced(&[]));
    assert!(balanced(&events(&[
        (EV_KEY, 30, 1),
        report,
        (EV_KEY, 30, 0),
        report
    ])));
    assert!(!balanced(&events(&[(EV_KEY, 30, 1), report])));
    assert!(!balanced(&events(&[
        (EV_KEY, 30, 1),
        (EV_KEY, 30, 2),
        report
    ])));
    assert!(balanced(&events(&[(EV_KEY, 30, 2), report])));
    assert!(!balanced(&events(&[(EV_SW, 0, 1), report])));
    assert!(balanced(&events(&[
        (EV_SW, 0, 1),
        report,
        (EV_SW, 0, 0),
        report
    ])));
    assert!(balanced(&events(&[(crate::input::EV_REL, 0, 5), report])));
    let contact = |slot, id| {
        [
            (EV_ABS, ABS_MT_SLOT, slot),
            (EV_ABS, ABS_MT_TRACKING_ID, id),
        ]
    };
    let mut touch = contact(0, 7).to_vec();
    touch.push(report);
    assert!(!balanced(&events(&touch)));
    touch.extend(contact(1, 8));
    touch.push(report);
    touch.extend(contact(0, -1));
    touch.push(report);
    assert!(!balanced(&events(&touch)));
    touch.extend(contact(1, -1));
    touch.push(report);
    assert!(balanced(&events(&touch)));
}
