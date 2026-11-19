//! Toolbar layout in DIPs (96 DPI units). Pure so it can be unit-tested and
//! shared with the end-to-end test, which clicks buttons by position.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    Grip,
    Mode,
    Color(usize),
    Undo,
    Clear,
    Exit,
}

pub const HEIGHT: f32 = 40.0;

/// Pen colors as linear 0..1 RGB.
pub const COLORS: [[f32; 3]; 4] = [
    [0.93, 0.16, 0.16], // red
    [0.13, 0.75, 0.29], // green
    [0.15, 0.45, 0.95], // blue
    [1.00, 0.84, 0.00], // yellow
];

const ITEMS: [(Button, f32); 9] = [
    (Button::Grip, 18.0),
    (Button::Mode, 64.0),
    (Button::Color(0), 36.0),
    (Button::Color(1), 36.0),
    (Button::Color(2), 36.0),
    (Button::Color(3), 36.0),
    (Button::Undo, 56.0),
    (Button::Clear, 56.0),
    (Button::Exit, 40.0),
];

pub fn width() -> f32 {
    ITEMS.iter().map(|i| i.1).sum()
}

/// Each button with its [x0, x1) span in DIPs.
pub fn items() -> impl Iterator<Item = (Button, f32, f32)> {
    ITEMS.iter().scan(0.0, |x, &(b, w)| {
        let x0 = *x;
        *x += w;
        Some((b, x0, *x))
    })
}

pub fn button_at(x: f32) -> Option<Button> {
    items().find(|&(_, x0, x1)| x >= x0 && x < x1).map(|i| i.0)
}

#[allow(dead_code)] // used by tests/overlay_e2e.rs
pub fn center_of(b: Button) -> Option<(f32, f32)> {
    items().find(|i| i.0 == b).map(|(_, x0, x1)| ((x0 + x1) / 2.0, HEIGHT / 2.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_testing_matches_layout() {
        assert_eq!(button_at(0.0), Some(Button::Grip));
        assert_eq!(button_at(18.0), Some(Button::Mode));
        assert_eq!(button_at(width() - 0.1), Some(Button::Exit));
        assert_eq!(button_at(width()), None);
        assert_eq!(button_at(-1.0), None);
        for (b, _, _) in items() {
            let (cx, _) = center_of(b).unwrap();
            assert_eq!(button_at(cx), Some(b));
        }
    }
}
