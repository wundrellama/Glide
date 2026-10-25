//! Pure gesture recognition for the layer-shell capture backend.
//!
//! Translates compositor touchpad gestures into existing wire events:
//! two-finger horizontal scrolls (libinput reports them as finger scroll
//! axes, not swipe gestures) become Back/Forward buttons, three-finger
//! swipes become workspace-switch key chords. Everything is threshold-
//! and dominance-gated so ordinary scrolling is never swallowed.

use std::collections::VecDeque;

use input_event::{BTN_BACK, BTN_FORWARD, Event, KeyboardEvent, PointerEvent};

/// wl_pointer vertical scroll axis id
pub const AXIS_VERTICAL: u8 = 0;
/// wl_pointer horizontal scroll axis id
pub const AXIS_HORIZONTAL: u8 = 1;

/* Both wl_keyboard and zwp_virtual_keyboard use Linux evdev codes.
 * Add 8 only when looking up symbols through an XKB keymap, never on
 * the capture/network/virtual-keyboard path. */
const KEY_TAB: u32 = input_event::scancode::Linux::KeyTab as u32;
const KEY_LEFT_SHIFT: u32 = input_event::scancode::Linux::KeyLeftShift as u32;
const KEY_LEFT_META: u32 = input_event::scancode::Linux::KeyLeftMeta as u32;

/// accumulated horizontal finger-scroll distance that distinguishes a
/// deliberate two-finger swipe from incidental horizontal scrolling
pub const TWO_FINGER_THRESHOLD: f64 = 80.0;
/// minimum |dx|:|dy| ratio for a horizontal gesture
pub const HORIZONTAL_DOMINANCE: f64 = 2.0;
/// accumulated swipe delta (surface coordinates) required for a
/// three-finger workspace switch
pub const SWIPE_THRESHOLD: f64 = 200.0;
/// cap on scroll events buffered while awaiting a gesture decision;
/// on overflow the recognizer fails closed and replays everything as
/// ordinary scrolling
pub const MAX_BUFFERED_SCROLL_EVENTS: usize = 64;

/// a recognized gesture, rendered as a balanced press+release wire
/// event sequence that never touches keys outside its own chord
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GestureAction {
    /// browser back (BTN_BACK)
    Back,
    /// browser forward (BTN_FORWARD)
    Forward,
    /// previous workspace (Super+Shift+Tab)
    PrevWorkspace,
    /// next workspace (Super+Tab)
    NextWorkspace,
}

impl GestureAction {
    pub fn events(self, time: u32) -> Vec<Event> {
        log::info!("Glide gesture: {self:?}");
        let key = |key: u32, state: u8| Event::Keyboard(KeyboardEvent::Key { time, key, state });
        let button = |button: u32, state: u32| {
            Event::Pointer(PointerEvent::Button {
                time,
                button,
                state,
            })
        };
        match self {
            GestureAction::Back => vec![button(BTN_BACK, 1), button(BTN_BACK, 0)],
            GestureAction::Forward => vec![button(BTN_FORWARD, 1), button(BTN_FORWARD, 0)],
            GestureAction::NextWorkspace => vec![
                key(KEY_LEFT_META, 1),
                key(KEY_TAB, 1),
                key(KEY_TAB, 0),
                key(KEY_LEFT_META, 0),
            ],
            GestureAction::PrevWorkspace => vec![
                key(KEY_LEFT_META, 1),
                key(KEY_LEFT_SHIFT, 1),
                key(KEY_TAB, 1),
                key(KEY_TAB, 0),
                key(KEY_LEFT_SHIFT, 0),
                key(KEY_LEFT_META, 0),
            ],
        }
    }
}

fn axis_event(time: u32, axis: u8, value: f64) -> Event {
    Event::Pointer(PointerEvent::Axis { time, axis, value })
}

/// recognizes deliberate two-finger horizontal swipes from finger-source
/// scroll axes. With libinput, two-finger swipes arrive as ordinary
/// finger scroll events, so a candidate is buffered until the horizontal
/// axis stop; below the threshold or without horizontal dominance the
/// buffered events are replayed unchanged as ordinary scrolling.
pub struct TwoFingerScroll {
    threshold: f64,
    dominance: f64,
    max_buffered: usize,
    buffered: VecDeque<(u32, f64)>,
    dx: f64,
    dy: f64,
    passthrough: bool,
}

impl Default for TwoFingerScroll {
    fn default() -> Self {
        Self::with_limits(
            TWO_FINGER_THRESHOLD,
            HORIZONTAL_DOMINANCE,
            MAX_BUFFERED_SCROLL_EVENTS,
        )
    }
}

impl TwoFingerScroll {
    pub fn with_limits(threshold: f64, dominance: f64, max_buffered: usize) -> Self {
        Self {
            threshold,
            dominance,
            max_buffered,
            buffered: VecDeque::new(),
            dx: 0.0,
            dy: 0.0,
            passthrough: false,
        }
    }

    /// true while horizontal candidate events are held back awaiting a
    /// decision at axis stop
    pub fn is_buffering(&self) -> bool {
        !self.buffered.is_empty()
    }

    /// horizontal finger-source axis event; swallowed into the candidate
    /// buffer (empty vec) unless the bounded buffer overflows, in which
    /// case everything is failed closed back to ordinary scrolling
    pub fn horizontal_axis(&mut self, time: u32, value: f64) -> Vec<Event> {
        if self.passthrough {
            return vec![axis_event(time, AXIS_HORIZONTAL, value)];
        }
        if self.buffered.len() >= self.max_buffered {
            let mut events = self.flush();
            self.passthrough = true;
            events.push(axis_event(time, AXIS_HORIZONTAL, value));
            return events;
        }
        self.buffered.push_back((time, value));
        self.dx += value;
        Vec::new()
    }

    /// vertical finger-source axis event: forwarded immediately and
    /// unchanged; tracked only for gesture dominance while a horizontal
    /// candidate is open
    pub fn vertical_axis(&mut self, time: u32, value: f64) -> Vec<Event> {
        if self.is_buffering() {
            self.dy += value.abs();
        }
        vec![axis_event(time, AXIS_VERTICAL, value)]
    }

    /// finger-source horizontal axis stop: the gesture decision point.
    /// A dominant, threshold-clearing displacement becomes Back/Forward;
    /// anything else replays the buffered scroll events unchanged.
    /// With natural scrolling, negative dx is a physical left-to-right
    /// swipe (Back) and positive dx is right-to-left (Forward).
    pub fn horizontal_stop(&mut self, time: u32) -> Vec<Event> {
        if self.passthrough {
            self.reset();
            return Vec::new();
        }
        if self.buffered.is_empty() {
            return Vec::new();
        }
        let (dx, dy) = (self.dx, self.dy);
        if dx.abs() >= self.threshold && dx.abs() >= self.dominance * dy.abs() {
            self.reset();
            let action = if dx < 0.0 {
                GestureAction::Back
            } else {
                GestureAction::Forward
            };
            action.events(time)
        } else {
            self.flush()
        }
    }

    /// replay buffered scroll events unchanged and reset; used when a
    /// non-finger axis event interleaves a pending candidate so event
    /// order is preserved
    pub fn flush(&mut self) -> Vec<Event> {
        let events = self
            .buffered
            .drain(..)
            .map(|(time, value)| axis_event(time, AXIS_HORIZONTAL, value))
            .collect();
        self.dx = 0.0;
        self.dy = 0.0;
        events
    }

    /// drop any pending candidate without emitting; used when capture
    /// ownership is lost so no action fires after the grab is gone
    pub fn reset(&mut self) {
        self.buffered.clear();
        self.dx = 0.0;
        self.dy = 0.0;
        self.passthrough = false;
    }
}

/// Defer scroll interpretation until wl_pointer.frame: compositors may
/// deliver Axis before AxisSource or AxisValue120 within the same frame.
#[derive(Default)]
pub struct ScrollFrame {
    pub finger_source: bool,
    pub axes: Vec<PointerEvent>,
    pub stops: Vec<(u32, u8)>,
}

impl ScrollFrame {
    pub fn finish(&mut self, scroll: &mut TwoFingerScroll, grabbed: bool) -> Vec<Event> {
        let frame = std::mem::take(self);
        if !grabbed {
            scroll.reset();
            return Vec::new();
        }
        let mut output = Vec::new();
        for event in &frame.axes {
            match *event {
                PointerEvent::Axis { time, axis, value } => {
                    // High-resolution wheel metadata replaces its companion
                    // continuous Axis regardless of their delivery order.
                    if frame.axes.iter().any(|other| matches!(other,
                        PointerEvent::AxisDiscrete120 { axis: other_axis, .. } if *other_axis == axis)) {
                        continue;
                    }
                    if frame.finger_source && axis == AXIS_HORIZONTAL {
                        output.extend(scroll.horizontal_axis(time, value));
                    } else if frame.finger_source && axis == AXIS_VERTICAL {
                        output.extend(scroll.vertical_axis(time, value));
                    } else {
                        output.extend(scroll.flush());
                        output.push(Event::Pointer(*event));
                    }
                }
                _ => {
                    output.extend(scroll.flush());
                    output.push(Event::Pointer(*event));
                }
            }
        }
        for (time, axis) in frame.stops {
            if axis == AXIS_HORIZONTAL {
                // A buffered candidate already proves finger provenance.
                // Stop-only frames need not repeat the source metadata.
                output.extend(scroll.horizontal_stop(time));
            }
        }
        output
    }
}

/// recognizes three-finger horizontal swipes from the pointer-gestures
/// protocol. Only swipes that started with exactly three fingers are
/// tracked; a non-cancelled end with a dominant, threshold-clearing
/// horizontal displacement yields one workspace action. Positive dx is
/// a physical left-to-right swipe, which maps to the previous workspace
/// (Super+Shift+Tab); negative dx maps to the next workspace (Super+Tab).
pub struct ThreeFingerSwipe {
    threshold: f64,
    dominance: f64,
    tracking: bool,
    dx: f64,
    dy: f64,
}

impl Default for ThreeFingerSwipe {
    fn default() -> Self {
        Self::with_limits(SWIPE_THRESHOLD, HORIZONTAL_DOMINANCE)
    }
}

impl ThreeFingerSwipe {
    pub fn with_limits(threshold: f64, dominance: f64) -> Self {
        Self {
            threshold,
            dominance,
            tracking: false,
            dx: 0.0,
            dy: 0.0,
        }
    }

    pub fn begin(&mut self, fingers: u32) {
        self.tracking = fingers == 3;
        self.dx = 0.0;
        self.dy = 0.0;
    }

    pub fn update(&mut self, dx: f64, dy: f64) {
        if self.tracking {
            self.dx += dx;
            self.dy += dy;
        }
    }

    /// swipe end; `modifiers_held` must reflect any physically depressed
    /// modifier on the source keyboard so the synthesized chord can
    /// never corrupt user-held modifier state
    pub fn end(&mut self, time: u32, cancelled: bool, modifiers_held: bool) -> Vec<Event> {
        let (tracking, dx, dy) = (self.tracking, self.dx, self.dy);
        self.reset();
        if !tracking || cancelled || modifiers_held {
            return Vec::new();
        }
        if dx.abs() >= self.threshold && dx.abs() >= self.dominance * dy.abs() {
            let action = if dx > 0.0 {
                GestureAction::PrevWorkspace
            } else {
                GestureAction::NextWorkspace
            };
            action.events(time)
        } else {
            Vec::new()
        }
    }

    /// cancel tracking without emitting; used on ungrab, focus loss and
    /// device removal so no action fires after ownership is gone
    pub fn reset(&mut self) {
        self.tracking = false;
        self.dx = 0.0;
        self.dy = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn btn(time: u32, button: u32, state: u32) -> Event {
        Event::Pointer(PointerEvent::Button {
            time,
            button,
            state,
        })
    }

    fn axis(time: u32, axis: u8, value: f64) -> Event {
        Event::Pointer(PointerEvent::Axis { time, axis, value })
    }

    #[test]
    fn two_finger_swipe_right_with_natural_scroll_emits_back() {
        let mut r = TwoFingerScroll::default();
        // physical left-to-right swipe; natural scroll reports negative dx
        assert_eq!(r.horizontal_axis(100, -30.0), vec![]);
        assert_eq!(r.horizontal_axis(110, -30.0), vec![]);
        assert_eq!(r.horizontal_axis(120, -30.0), vec![]);
        let events = r.horizontal_stop(130);
        assert_eq!(events, vec![btn(130, BTN_BACK, 1), btn(130, BTN_BACK, 0)]);
        // only one action per gesture
        assert_eq!(r.horizontal_stop(140), vec![]);
    }

    #[test]
    fn two_finger_swipe_left_with_natural_scroll_emits_forward() {
        let mut r = TwoFingerScroll::default();
        // physical right-to-left swipe; natural scroll reports positive dx
        r.horizontal_axis(100, 45.0);
        r.horizontal_axis(110, 45.0);
        let events = r.horizontal_stop(120);
        assert_eq!(
            events,
            vec![btn(120, BTN_FORWARD, 1), btn(120, BTN_FORWARD, 0)]
        );
    }

    #[test]
    fn small_horizontal_scroll_is_replayed_unchanged() {
        let mut r = TwoFingerScroll::default();
        r.horizontal_axis(100, 10.0);
        r.horizontal_axis(110, 20.0);
        r.horizontal_axis(120, 25.0); // total 55 < 80 threshold
        let events = r.horizontal_stop(130);
        assert_eq!(
            events,
            vec![
                axis(100, AXIS_HORIZONTAL, 10.0),
                axis(110, AXIS_HORIZONTAL, 20.0),
                axis(120, AXIS_HORIZONTAL, 25.0),
            ]
        );
    }

    #[test]
    fn vertical_finger_scroll_is_forwarded_immediately() {
        let mut r = TwoFingerScroll::default();
        assert_eq!(
            r.vertical_axis(100, -15.0),
            vec![axis(100, AXIS_VERTICAL, -15.0)]
        );
        assert_eq!(
            r.vertical_axis(110, 20.0),
            vec![axis(110, AXIS_VERTICAL, 20.0)]
        );
    }

    #[test]
    fn diagonal_scroll_is_replayed_unchanged() {
        let mut r = TwoFingerScroll::default();
        r.horizontal_axis(100, -50.0);
        r.vertical_axis(105, -40.0);
        r.horizontal_axis(110, -50.0);
        r.vertical_axis(115, -40.0);
        // |dx| = 100 clears the threshold but not 2:1 dominance over |dy| = 80
        let events = r.horizontal_stop(130);
        assert_eq!(
            events,
            vec![
                axis(100, AXIS_HORIZONTAL, -50.0),
                axis(110, AXIS_HORIZONTAL, -50.0),
            ]
        );
    }

    #[test]
    fn candidate_buffer_is_bounded_and_fails_closed() {
        let mut r = TwoFingerScroll::with_limits(80.0, 2.0, 4);
        for i in 0..4 {
            assert_eq!(r.horizontal_axis(i, 1.0), vec![]);
        }
        // the 5th event overflows: everything comes back as plain scrolling
        let events = r.horizontal_axis(4, 1.0);
        assert_eq!(
            events,
            vec![
                axis(0, AXIS_HORIZONTAL, 1.0),
                axis(1, AXIS_HORIZONTAL, 1.0),
                axis(2, AXIS_HORIZONTAL, 1.0),
                axis(3, AXIS_HORIZONTAL, 1.0),
                axis(4, AXIS_HORIZONTAL, 1.0),
            ]
        );
        // the recognizer has reset: a later stop emits nothing
        assert_eq!(r.horizontal_stop(10), vec![]);
    }

    #[test]
    fn ownership_loss_drops_pending_candidate_without_emitting() {
        let mut r = TwoFingerScroll::default();
        r.horizontal_axis(100, -90.0);
        r.reset();
        assert_eq!(r.horizontal_stop(110), vec![]);
        // a fresh scroll afterwards behaves normally
        r.horizontal_axis(120, 10.0);
        assert_eq!(
            r.horizontal_stop(130),
            vec![axis(120, AXIS_HORIZONTAL, 10.0)]
        );
    }

    #[test]
    fn flush_replays_buffered_events_in_order_and_resets() {
        let mut r = TwoFingerScroll::default();
        r.horizontal_axis(100, -5.0);
        r.horizontal_axis(110, -7.0);
        assert_eq!(
            r.flush(),
            vec![
                axis(100, AXIS_HORIZONTAL, -5.0),
                axis(110, AXIS_HORIZONTAL, -7.0),
            ]
        );
        assert_eq!(r.horizontal_stop(120), vec![]);
    }

    fn key(time: u32, key: u32, state: u8) -> Event {
        Event::Keyboard(KeyboardEvent::Key { time, key, state })
    }

    #[test]
    fn three_finger_swipe_right_emits_prev_workspace_chord() {
        let mut s = ThreeFingerSwipe::default();
        s.begin(3);
        s.update(120.0, 5.0);
        s.update(120.0, 5.0);
        let events = s.end(1000, false, false);
        assert_eq!(
            events,
            vec![
                key(1000, KEY_LEFT_META, 1),
                key(1000, KEY_LEFT_SHIFT, 1),
                key(1000, KEY_TAB, 1),
                key(1000, KEY_TAB, 0),
                key(1000, KEY_LEFT_SHIFT, 0),
                key(1000, KEY_LEFT_META, 0),
            ]
        );
        // one action per swipe: a second end emits nothing
        assert_eq!(s.end(1001, false, false), vec![]);
    }

    #[test]
    fn three_finger_swipe_left_emits_next_workspace_chord() {
        let mut s = ThreeFingerSwipe::default();
        s.begin(3);
        s.update(-240.0, 0.0);
        let events = s.end(1000, false, false);
        assert_eq!(
            events,
            vec![
                key(1000, KEY_LEFT_META, 1),
                key(1000, KEY_TAB, 1),
                key(1000, KEY_TAB, 0),
                key(1000, KEY_LEFT_META, 0),
            ]
        );
    }

    #[test]
    fn cancelled_swipe_emits_nothing() {
        let mut s = ThreeFingerSwipe::default();
        s.begin(3);
        s.update(300.0, 0.0);
        assert_eq!(s.end(1000, true, false), vec![]);
    }

    #[test]
    fn other_finger_counts_are_ignored() {
        for fingers in [1, 2, 4] {
            let mut s = ThreeFingerSwipe::default();
            s.begin(fingers);
            s.update(300.0, 0.0);
            assert_eq!(s.end(1000, false, false), vec![]);
        }
    }

    #[test]
    fn small_swipe_emits_nothing() {
        let mut s = ThreeFingerSwipe::default();
        s.begin(3);
        s.update(100.0, 0.0);
        assert_eq!(s.end(1000, false, false), vec![]);
    }

    #[test]
    fn swipe_with_held_modifier_emits_nothing_and_releases_nothing() {
        let mut s = ThreeFingerSwipe::default();
        s.begin(3);
        s.update(300.0, 0.0);
        // a physically held modifier must suppress the chord entirely:
        // emitting anything would corrupt the user's held modifier state
        assert_eq!(s.end(1000, false, true), vec![]);
    }

    #[test]
    fn diagonal_swipe_emits_nothing() {
        let mut s = ThreeFingerSwipe::default();
        s.begin(3);
        // |dx| = 250 clears the threshold but not 2:1 dominance over |dy| = 150
        s.update(250.0, 150.0);
        assert_eq!(s.end(1000, false, false), vec![]);
    }

    #[test]
    fn end_without_begin_emits_nothing() {
        let mut s = ThreeFingerSwipe::default();
        assert_eq!(s.end(1000, false, false), vec![]);
    }

    #[test]
    fn swipe_reset_cancels_tracking() {
        let mut s = ThreeFingerSwipe::default();
        s.begin(3);
        s.update(300.0, 0.0);
        s.reset();
        assert_eq!(s.end(1000, false, false), vec![]);
    }

    #[test]
    fn all_actions_emit_balanced_press_release_sequences() {
        for action in [
            GestureAction::Back,
            GestureAction::Forward,
            GestureAction::PrevWorkspace,
            GestureAction::NextWorkspace,
        ] {
            let events = action.events(0);
            let mut held: Vec<u32> = Vec::new();
            for event in events {
                let (id, state) = match event {
                    Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                        (key, u32::from(state))
                    }
                    Event::Pointer(PointerEvent::Button { button, state, .. }) => (button, state),
                    other => panic!("gesture emitted a non-key/button event: {other:?}"),
                };
                match state {
                    1 => held.push(id),
                    0 => {
                        let pos = held
                            .iter()
                            .rposition(|&h| h == id)
                            .unwrap_or_else(|| panic!("release of never-pressed {id:#x}"));
                        held.remove(pos);
                    }
                    other => panic!("unexpected state {other}"),
                }
            }
            assert!(
                held.is_empty(),
                "unbalanced sequence, still held: {held:#x?}"
            );
        }
    }

    #[test]
    fn overflow_stays_plain_scrolling_until_the_gesture_ends() {
        let mut r = TwoFingerScroll::with_limits(80.0, 2.0, 2);
        r.horizontal_axis(1, -1.0);
        r.horizontal_axis(2, -1.0);
        assert_eq!(r.horizontal_axis(3, -1.0).len(), 3);
        assert_eq!(
            r.horizontal_axis(4, -100.0),
            vec![axis(4, AXIS_HORIZONTAL, -100.0)]
        );
        assert!(r.horizontal_stop(5).is_empty());
        r.horizontal_axis(6, -100.0);
        assert_eq!(r.horizontal_stop(7), GestureAction::Back.events(7));
    }

    #[test]
    fn reversing_vertical_scroll_does_not_hide_diagonal_motion() {
        let mut r = TwoFingerScroll::default();
        r.horizontal_axis(1, -100.0);
        r.vertical_axis(2, 80.0);
        r.vertical_axis(3, -80.0);
        assert_eq!(r.horizontal_stop(4), vec![axis(1, AXIS_HORIZONTAL, -100.0)]);
    }

    #[test]
    fn finger_source_after_axis_is_applied_to_the_entire_frame() {
        let mut scroll = TwoFingerScroll::default();
        let mut frame = ScrollFrame::default();
        frame.axes.push(PointerEvent::Axis {
            time: 1,
            axis: AXIS_HORIZONTAL,
            value: -100.0,
        });
        // Hyprland sends Axis first, then AxisSource, then Frame.
        frame.finger_source = true;
        assert!(frame.finish(&mut scroll, true).is_empty());
        frame.stops.push((2, AXIS_HORIZONTAL));
        frame.finger_source = true;
        assert_eq!(
            frame.finish(&mut scroll, true),
            GestureAction::Back.events(2)
        );
    }

    #[test]
    fn wheel_value120_after_axis_does_not_duplicate_the_scroll() {
        let mut scroll = TwoFingerScroll::default();
        let mut frame = ScrollFrame::default();
        frame.axes.push(PointerEvent::Axis {
            time: 1,
            axis: AXIS_HORIZONTAL,
            value: -120.0,
        });
        frame.axes.push(PointerEvent::AxisDiscrete120 {
            axis: AXIS_HORIZONTAL,
            value: -120,
        });
        assert_eq!(
            frame.finish(&mut scroll, true),
            vec![Event::Pointer(PointerEvent::AxisDiscrete120 {
                axis: AXIS_HORIZONTAL,
                value: -120
            })]
        );
        assert!(scroll.horizontal_stop(2).is_empty());
    }

    #[test]
    fn incomplete_frame_cannot_emit_navigation_after_ungrab() {
        let mut scroll = TwoFingerScroll::default();
        let mut frame = ScrollFrame::default();
        frame.axes.push(PointerEvent::Axis {
            time: 1,
            axis: AXIS_HORIZONTAL,
            value: -100.0,
        });
        frame.stops.push((2, AXIS_HORIZONTAL));
        frame.finger_source = true;
        assert!(frame.finish(&mut scroll, false).is_empty());
        assert!(scroll.horizontal_stop(3).is_empty());
    }

    #[test]
    fn source_less_frame_remains_ordinary_scrolling_after_a_finger_frame() {
        let mut scroll = TwoFingerScroll::default();
        let mut frame = ScrollFrame::default();
        frame.axes.push(PointerEvent::Axis {
            time: 1,
            axis: AXIS_HORIZONTAL,
            value: -10.0,
        });
        frame.finger_source = true;
        assert!(frame.finish(&mut scroll, true).is_empty());
        frame.axes.push(PointerEvent::Axis {
            time: 2,
            axis: AXIS_HORIZONTAL,
            value: -100.0,
        });
        assert_eq!(
            frame.finish(&mut scroll, true),
            vec![
                axis(1, AXIS_HORIZONTAL, -10.0),
                axis(2, AXIS_HORIZONTAL, -100.0)
            ]
        );
        frame.stops.push((3, AXIS_HORIZONTAL));
        assert!(frame.finish(&mut scroll, true).is_empty());
    }

    #[test]
    fn wire_keys_use_unmodified_linux_evdev_codes() {
        // wl_keyboard and zwp_virtual_keyboard use evdev codes directly.
        // The +8 offset belongs only to XKB keymap lookups.
        use input_event::scancode;
        assert_eq!(
            scancode::Linux::try_from(KEY_TAB),
            Ok(scancode::Linux::KeyTab)
        );
        assert_eq!(
            scancode::Linux::try_from(KEY_LEFT_SHIFT),
            Ok(scancode::Linux::KeyLeftShift)
        );
        assert_eq!(
            scancode::Linux::try_from(KEY_LEFT_META),
            Ok(scancode::Linux::KeyLeftMeta)
        );
    }
}
