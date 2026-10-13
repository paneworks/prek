use std::time::Duration;

use prek::{Button, Buttons, Timing};

// A deterministic double tap: no hardware access, sleeps or desktop required.
fn main() {
    let mut buttons = Buttons::new(Timing::default()).expect("valid timing");
    let button = Button {
        device: 1,
        code: 115,
    }; // KEY_VOLUMEUP on Linux
    for (down, millis) in [(true, 0), (false, 50), (true, 150), (false, 200)] {
        assert!(
            buttons
                .update(button, down, Duration::from_millis(millis))
                .is_empty()
        );
    }
    for gesture in buttons.advance(Duration::from_millis(500)) {
        println!("{gesture:?}");
    }
}
