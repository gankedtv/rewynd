//! The crop overlay: a draggable rectangle over the clip preview, sitting between the preview
//! frame and the play/fullscreen controls. All the rectangle math lives in [`crop`]; this widget
//! only turns pointer and key events into it and paints the result.
//!
//! The rect is in 0..1 fractions of the SOURCE frame, never of the widget, so it survives a
//! window resize and means the same thing to the exporter. The picture it is drawn over is
//! [`crop::contain_rect`] of the source dimensions — the same letterbox the video shader and the
//! `image` widget produce. Decoded preview frames are integer-scaled, so a preview's own aspect
//! can differ from the source's by up to ~0.3%; at preview size that is sub-pixel, while using
//! the source dimensions keeps the overlay's fractions and the exported crop the same numbers.
//!
//! Two `Stack` rules shape the event handling. `Stack::update` walks its children top-down and
//! stops at the first one that captures the event, and a child whose `mouse_interaction` is not
//! `Interaction::None` levitates the cursor away from every layer beneath it. So outside the
//! picture this widget captures nothing and reports `Interaction::None`, leaving the controls
//! stacked above it and the click-to-pause area below it untouched. Inside the picture it owns
//! the press — including a press that lands outside the rect, which is why a press that never
//! moves comes back out as `on_tap` for the caller to map to play/pause.

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget, mouse, renderer};
use iced::keyboard::{self, Key, key::Named};
use iced::{Background, Border, Color, Element, Event, Length, Point, Rectangle, Size};

use crate::crop::{self, CropRect, Handle};
use crate::theme::palette;

/// How near (px) a press must be to a handle to grab it instead of moving the whole rect.
const GRAB: f32 = 12.0;
/// The drawn size (px) of one handle square.
const HANDLE: f32 = 10.0;

/// A draggable crop rectangle over the picture of a `frame`-sized clip.
pub struct CropBox<'a, Message> {
    rect: CropRect,
    /// SOURCE clip dimensions; the picture area is [`crop::contain_rect`] of these.
    frame: (u32, u32),
    /// The preset lock, already resolved against the frame ratio (`None` is free-form).
    rect_ratio: Option<f32>,
    on_change: Box<dyn Fn(CropRect) -> Message + 'a>,
    on_reset: Option<Message>,
    on_tap: Option<Message>,
}

/// A press in progress.
struct Drag {
    /// `None` is a body move rather than an edge/corner resize.
    handle: Option<Handle>,
    /// Where the press landed, and the rect it found: a body move is measured from these, so
    /// it cannot drift or lose a step whatever order a frame's moves arrive in.
    press: (f32, f32),
    origin: CropRect,
    /// The previous pointer position, in fractions of the picture area.
    last: (f32, f32),
    /// Whether the pointer moved at all; a press that did not is a tap, not a drag.
    moved: bool,
}

#[derive(Default)]
struct State {
    drag: Option<Drag>,
    /// Whether keyboard input edits this rect (gained by pressing the picture, lost by
    /// pressing elsewhere or Escape).
    focused: bool,
    /// The last rect this widget published. iced runs every event of a frame through `update`
    /// before rebuilding the view, so `self.rect` is stale within a batch and each edit has to
    /// build on this instead; cleared once the caller is certain to have caught up.
    live: Option<CropRect>,
}

impl<'a, Message> CropBox<'a, Message> {
    pub fn new(
        rect: CropRect,
        frame: (u32, u32),
        rect_ratio: Option<f32>,
        on_change: impl Fn(CropRect) -> Message + 'a,
    ) -> Self {
        Self {
            rect,
            frame,
            rect_ratio,
            on_change: Box::new(on_change),
            on_reset: None,
            on_tap: None,
        }
    }

    /// Publish `message` when Escape is pressed while the overlay has keyboard focus.
    pub fn on_reset(mut self, message: Message) -> Self {
        self.on_reset = Some(message);
        self
    }

    /// Publish `message` when a press inside the picture is released without moving. The
    /// overlay swallows every press over the picture, so this is the only way the click-to-play
    /// layer underneath can still be reached.
    pub fn on_tap(mut self, message: Message) -> Self {
        self.on_tap = Some(message);
        self
    }

    /// The picture the rect is measured against: the frame fitted into `bounds`.
    fn area(&self, bounds: Rectangle) -> Rectangle {
        crop::contain_rect(self.frame, bounds)
    }

    /// Publish `next`, unless the edit changed nothing. Measured against the rect the edit was
    /// built on, which within a batch is [`State::live`] rather than `self.rect`.
    fn publish(&self, current: CropRect, next: CropRect, shell: &mut Shell<'_, Message>) {
        if next != current {
            shell.publish((self.on_change)(next));
        }
    }

    /// Drop keyboard focus, and the working rect with it: the next edit starts from whatever
    /// the caller has by then.
    fn unfocus(&self, state: &mut State, shell: &mut Shell<'_, Message>) {
        state.live = None;
        state.focused = false;
        shell.request_redraw();
    }
}

/// Inclusive on the far edges, unlike [`Rectangle::contains`]: the S/E/Se handles sit exactly on
/// the picture's bottom/right edge, and a press there has to grab them.
fn contains(area: Rectangle, point: Point) -> bool {
    area.width > 0.0
        && area.height > 0.0
        && point.x >= area.x
        && point.x <= area.x + area.width
        && point.y >= area.y
        && point.y <= area.y + area.height
}

/// Whether a picture fraction falls inside the crop rect itself.
fn inside_rect(rect: CropRect, (fx, fy): (f32, f32)) -> bool {
    fx >= rect.x && fx <= rect.x + rect.width && fy >= rect.y && fy <= rect.y + rect.height
}

/// The arrow key's nudge, as a fraction of the frame on each axis.
fn nudge(named: Named) -> Option<(f32, f32)> {
    match named {
        Named::ArrowLeft => Some((-crop::STEP, 0.0)),
        Named::ArrowRight => Some((crop::STEP, 0.0)),
        Named::ArrowUp => Some((0.0, -crop::STEP)),
        Named::ArrowDown => Some((0.0, crop::STEP)),
        _ => None,
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for CropBox<'_, Message>
where
    Message: Clone,
    Renderer: renderer::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: Length::Fill,
            height: Length::Fill,
        }
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fill)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();
        let area = self.area(layout.bounds());
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let Some(pos) = cursor.position().filter(|p| contains(area, *p)) {
                    let frac = crop::point_fraction(area, pos);
                    let current = state.live.unwrap_or(self.rect);
                    state.focused = true;
                    state.drag = Some(Drag {
                        handle: crop::hit_test_handle(
                            current,
                            frac,
                            (area.width, area.height),
                            GRAB,
                        ),
                        press: frac,
                        origin: current,
                        last: frac,
                        moved: false,
                    });
                    shell.request_redraw();
                    shell.capture_event();
                } else if state.focused {
                    // A press on the letterbox bars, or anywhere else, only moves focus away;
                    // it stays uncaptured so whatever was actually pressed still gets it.
                    self.unfocus(state, shell);
                }
            }
            // The event's own position, not the cursor's: a drag that wanders under a control
            // stacked above this layer arrives with a levitating (positionless) cursor.
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let Some(mut drag) = state.drag.take() else {
                    return;
                };
                let frac = crop::point_fraction(area, *position);
                if frac != drag.last {
                    let current = state.live.unwrap_or(self.rect);
                    let next = match drag.handle {
                        Some(handle) => crop::resize_rect(current, handle, frac, self.rect_ratio),
                        // Measured from the press rather than the previous move: the whole
                        // frame's moves reach `update` before the view rebuilds, so a delta
                        // against a rect that has not caught up would keep only the last one.
                        None => crop::move_rect(
                            drag.origin,
                            frac.0 - drag.press.0,
                            frac.1 - drag.press.1,
                        ),
                    };
                    drag.last = frac;
                    drag.moved = true;
                    state.live = Some(next);
                    self.publish(current, next, shell);
                }
                state.drag = Some(drag);
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if let Some(drag) = state.drag.take() {
                    state.live = None;
                    if !drag.moved
                        && let Some(message) = &self.on_tap
                    {
                        shell.publish(message.clone());
                    }
                    shell.capture_event();
                }
            }
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. })
                if state.focused =>
            {
                // Chorded presses (Ctrl/Alt/Super) belong to someone else's shortcut; nudging
                // the crop on them would both move the rect and shadow the shortcut. Shift
                // stays available as the resize modifier.
                if modifiers.control() || modifiers.alt() || modifiers.logo() {
                    return;
                }
                match key.as_ref() {
                    Key::Named(Named::Escape) => {
                        if let Some(message) = &self.on_reset {
                            shell.publish(message.clone());
                        }
                        self.unfocus(state, shell);
                        shell.capture_event();
                    }
                    Key::Named(named) => {
                        let Some((dx, dy)) = nudge(named) else {
                            return;
                        };
                        // Every nudge builds on the last one, so a held key's repeats add up
                        // instead of collapsing into a single step.
                        let current = state.live.unwrap_or(self.rect);
                        let next = if modifiers.shift() {
                            // Shift resizes instead of moving: the south-east corner follows
                            // the key and the north-west one stays put, ratio lock and all.
                            crop::resize_rect(
                                current,
                                Handle::Se,
                                (
                                    current.x + current.width + dx,
                                    current.y + current.height + dy,
                                ),
                                self.rect_ratio,
                            )
                        } else {
                            crop::move_rect(current, dx, dy)
                        };
                        state.live = Some(next);
                        self.publish(current, next, shell);
                        shell.capture_event();
                    }
                    _ => {}
                }
            }
            // The run of nudges is over; the next one starts from what the caller has by then.
            // Only an arrow ends it: letting go of Shift (or of anything else) mid-run would
            // rebase the next nudge on a rect the caller has not caught up with, losing a step.
            Event::Keyboard(keyboard::Event::KeyReleased { key, .. })
                if state.focused
                    && matches!(key.as_ref(), Key::Named(named) if nudge(named).is_some()) =>
            {
                state.live = None;
            }
            _ => {}
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let area = self.area(layout.bounds());
        if area.width <= 0.0 || area.height <= 0.0 {
            return;
        }
        let left = area.x + self.rect.x * area.width;
        let top = area.y + self.rect.y * area.height;
        let width = (self.rect.width * area.width).max(0.0);
        let height = (self.rect.height * area.height).max(0.0);
        let right = left + width;
        let bottom = top + height;

        // The scrim over everything outside the rect, so what stays lit is what gets exported:
        // full-width bands above and below, then the two pieces beside the rect.
        let scrim = Color {
            a: 0.6,
            ..palette::BACKGROUND
        };
        fill(
            renderer,
            px(area.x, area.y, area.width, top - area.y),
            scrim,
        );
        fill(
            renderer,
            px(area.x, bottom, area.width, area.y + area.height - bottom),
            scrim,
        );
        fill(renderer, px(area.x, top, left - area.x, height), scrim);
        fill(
            renderer,
            px(right, top, area.x + area.width - right, height),
            scrim,
        );

        outline(
            renderer,
            px(left, top, width, height),
            palette::ACCENT,
            1.5,
            0.0,
        );
        for handle in Handle::ALL {
            let (hx, hy) = handle.position(self.rect);
            chip(
                renderer,
                px(
                    area.x + hx * area.width - HANDLE / 2.0,
                    area.y + hy * area.height - HANDLE / 2.0,
                    HANDLE,
                    HANDLE,
                ),
                palette::ACCENT,
            );
        }
        // A focus ring around the picture, so keyboard users see where the nudges go.
        if tree.state.downcast_ref::<State>().focused {
            outline(renderer, area, palette::ACCENT, 1.0, 6.0);
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if tree.state.downcast_ref::<State>().drag.is_some() {
            return mouse::Interaction::Grabbing;
        }
        let area = self.area(layout.bounds());
        let Some(pos) = cursor.position().filter(|p| contains(area, *p)) else {
            return mouse::Interaction::None;
        };
        let frac = crop::point_fraction(area, pos);
        if let Some(handle) =
            crop::hit_test_handle(self.rect, frac, (area.width, area.height), GRAB)
        {
            return match handle {
                Handle::E | Handle::W => mouse::Interaction::ResizingHorizontally,
                Handle::N | Handle::S => mouse::Interaction::ResizingVertically,
                Handle::Nw | Handle::Se => mouse::Interaction::ResizingDiagonallyDown,
                Handle::Ne | Handle::Sw => mouse::Interaction::ResizingDiagonallyUp,
            };
        }
        if inside_rect(self.rect, frac) {
            mouse::Interaction::Grab
        } else {
            // Deliberately `None` between the rect and the picture's edge: the press there is
            // still ours (and comes back as `on_tap`), but reporting an interaction would
            // levitate the cursor off the click-to-pause layer below and make the preview feel
            // dead while framing a crop.
            mouse::Interaction::None
        }
    }
}

/// A rectangle from top-left + size, never negative.
fn px(x: f32, y: f32, width: f32, height: f32) -> Rectangle {
    Rectangle {
        x,
        y,
        width: width.max(0.0),
        height: height.max(0.0),
    }
}

/// Fill `bounds` with a flat colour.
fn fill<Renderer: renderer::Renderer>(renderer: &mut Renderer, bounds: Rectangle, color: Color) {
    if bounds.width <= 0.0 || bounds.height <= 0.0 {
        return;
    }
    renderer.fill_quad(
        renderer::Quad {
            bounds,
            ..renderer::Quad::default()
        },
        Background::Color(color),
    );
}

/// A border around `bounds` with nothing inside it.
fn outline<Renderer: renderer::Renderer>(
    renderer: &mut Renderer,
    bounds: Rectangle,
    color: Color,
    width: f32,
    radius: f32,
) {
    renderer.fill_quad(
        renderer::Quad {
            bounds,
            border: Border {
                color,
                width,
                radius: radius.into(),
            },
            ..renderer::Quad::default()
        },
        Background::Color(Color::TRANSPARENT),
    );
}

/// A small rounded square: one crop handle.
fn chip<Renderer: renderer::Renderer>(renderer: &mut Renderer, bounds: Rectangle, color: Color) {
    renderer.fill_quad(
        renderer::Quad {
            bounds,
            border: Border {
                radius: 2.0.into(),
                ..Border::default()
            },
            ..renderer::Quad::default()
        },
        Background::Color(color),
    );
}

impl<'a, Message, Theme, Renderer> From<CropBox<'a, Message>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: Clone + 'a,
    Theme: 'a,
    Renderer: renderer::Renderer + 'a,
{
    fn from(overlay: CropBox<'a, Message>) -> Self {
        Element::new(overlay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use iced::advanced::clipboard;
    use iced::keyboard::key::{Code, Physical};
    use iced::keyboard::{Location, Modifiers};

    /// The preview box the tests drive: 400x225 is exactly 16:9, so a 16:9 frame fills it and
    /// picture fractions are node fractions.
    const W: f32 = 400.0;
    const H: f32 = 225.0;

    #[derive(Clone, Debug, PartialEq)]
    enum Msg {
        Change(CropRect),
        Reset,
        Tap,
    }

    #[track_caller]
    fn assert_near(actual: f32, expected: f32, tol: f32) {
        assert!(
            (actual - expected).abs() <= tol,
            "expected {expected} ± {tol}, got {actual}"
        );
    }

    fn node() -> layout::Node {
        layout::Node::new(Size::new(W, H))
    }

    fn at(x: f32, y: f32) -> mouse::Cursor {
        mouse::Cursor::Available(Point::new(x, y))
    }

    fn press() -> Event {
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
    }

    fn release() -> Event {
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
    }

    fn moved(x: f32, y: f32) -> Event {
        Event::Mouse(mouse::Event::CursorMoved {
            position: Point::new(x, y),
        })
    }

    fn key_press(named: Named, code: Code, modifiers: Modifiers) -> Event {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Named(named),
            modified_key: Key::Named(named),
            physical_key: Physical::Code(code),
            location: Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        })
    }

    fn key_release(named: Named, code: Code) -> Event {
        Event::Keyboard(keyboard::Event::KeyReleased {
            key: Key::Named(named),
            modified_key: Key::Named(named),
            physical_key: Physical::Code(code),
            location: Location::Standard,
            modifiers: Modifiers::default(),
        })
    }

    fn char_release(c: &str) -> Event {
        Event::Keyboard(keyboard::Event::KeyReleased {
            key: Key::Character(c.into()),
            modified_key: Key::Character(c.into()),
            physical_key: Physical::Code(Code::KeyA),
            location: Location::Standard,
            modifiers: Modifiers::default(),
        })
    }

    fn rect(x: f32, y: f32, width: f32, height: f32) -> CropRect {
        CropRect {
            x,
            y,
            width,
            height,
        }
    }

    /// The widget plus its tree, wired to every callback so each test can assert on which one
    /// fired.
    fn harness(
        start: CropRect,
        frame: (u32, u32),
        rect_ratio: Option<f32>,
    ) -> (CropBox<'static, Msg>, Tree) {
        let widget = CropBox::new(start, frame, rect_ratio, Msg::Change)
            .on_reset(Msg::Reset)
            .on_tap(Msg::Tap);
        let tree = Tree {
            tag: Widget::<Msg, iced::Theme, ()>::tag(&widget),
            state: Widget::<Msg, iced::Theme, ()>::state(&widget),
            children: vec![],
        };
        (widget, tree)
    }

    /// Feeds one event through `update` with the null renderer/clipboard, returning what was
    /// published and whether the event was captured.
    fn drive(
        widget: &mut CropBox<'static, Msg>,
        tree: &mut Tree,
        event: Event,
        cursor: mouse::Cursor,
    ) -> (Vec<Msg>, bool) {
        let node = node();
        let viewport = Rectangle::new(Point::ORIGIN, Size::new(W, H));
        let mut clipboard = clipboard::Null;
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        Widget::<Msg, iced::Theme, ()>::update(
            widget,
            tree,
            &event,
            Layout::new(&node),
            cursor,
            &(),
            &mut clipboard,
            &mut shell,
            &viewport,
        );
        let captured = shell.is_event_captured();
        (messages, captured)
    }

    fn interaction(
        widget: &CropBox<'static, Msg>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        let node = node();
        let viewport = Rectangle::new(Point::ORIGIN, Size::new(W, H));
        Widget::<Msg, iced::Theme, ()>::mouse_interaction(
            widget,
            tree,
            Layout::new(&node),
            cursor,
            &viewport,
            &(),
        )
    }

    #[track_caller]
    fn only_change(messages: &[Msg]) -> CropRect {
        match messages {
            [Msg::Change(rect)] => *rect,
            other => panic!("expected exactly one change, got {other:?}"),
        }
    }

    #[test]
    fn dragging_the_south_east_corner_resizes_from_the_opposite_one() {
        let (mut widget, mut tree) = harness(crop::FULL_FRAME, (1920, 1080), None);

        let (msgs, captured) = drive(&mut widget, &mut tree, press(), at(W, H));
        assert!(msgs.is_empty(), "the press alone edits nothing: {msgs:?}");
        assert!(captured, "a press on the picture belongs to the overlay");

        let (msgs, captured) = drive(
            &mut widget,
            &mut tree,
            moved(200.0, 112.5),
            at(200.0, 112.5),
        );
        let next = only_change(&msgs);
        assert_near(next.width, 0.5, 0.01);
        assert_near(next.height, 0.5, 0.01);
        assert_near(next.x, 0.0, 0.01);
        assert_near(next.y, 0.0, 0.01);
        assert!(captured);

        let (msgs, captured) = drive(&mut widget, &mut tree, release(), at(200.0, 112.5));
        assert!(msgs.is_empty(), "a drag that moved is no tap: {msgs:?}");
        assert!(captured);
    }

    #[test]
    fn a_press_released_without_moving_is_a_tap() {
        // The overlay swallows every press over the picture, so this is what keeps the
        // click-to-play layer underneath reachable.
        let (mut widget, mut tree) = harness(rect(0.1, 0.1, 0.5, 0.5), (1920, 1080), None);

        let (msgs, _) = drive(&mut widget, &mut tree, press(), at(140.0, 78.0));
        assert!(msgs.is_empty());

        let (msgs, captured) = drive(&mut widget, &mut tree, release(), at(140.0, 78.0));
        assert_eq!(msgs, vec![Msg::Tap]);
        assert!(captured);
    }

    #[test]
    fn a_body_drag_translates_by_the_pointer_delta() {
        let (mut widget, mut tree) = harness(rect(0.1, 0.1, 0.5, 0.5), (1920, 1080), None);

        drive(&mut widget, &mut tree, press(), at(100.0, 100.0));
        let (msgs, _) = drive(
            &mut widget,
            &mut tree,
            moved(150.0, 120.0),
            at(150.0, 120.0),
        );

        let next = only_change(&msgs);
        assert_near(next.x, 0.1 + 50.0 / W, 1e-4);
        assert_near(next.y, 0.1 + 20.0 / H, 1e-4);
        assert_near(next.width, 0.5, 1e-5);
        assert_near(next.height, 0.5, 1e-5);
    }

    #[test]
    fn a_batch_of_moves_keeps_the_whole_displacement() {
        // iced runs every event of a frame through `update` before the view is rebuilt, so
        // `self.rect` stays at the pressed rect for all three moves here. A delta measured
        // against it would throw away everything but the last one.
        let (mut widget, mut tree) = harness(rect(0.1, 0.1, 0.5, 0.5), (1920, 1080), None);
        drive(&mut widget, &mut tree, press(), at(100.0, 100.0));

        let mut last = None;
        for x in [110.0, 120.0, 130.0] {
            let (msgs, _) = drive(&mut widget, &mut tree, moved(x, 110.0), at(x, 110.0));
            last = Some(only_change(&msgs));
        }

        let next = last.expect("three moves, three rects");
        assert_near(next.x, 0.1 + 30.0 / W, 1e-4);
        assert_near(next.y, 0.1 + 10.0 / H, 1e-4);
        assert_near(next.width, 0.5, 1e-5);
    }

    #[test]
    fn a_batch_of_nudges_keeps_every_step() {
        let (mut widget, mut tree) = harness(rect(0.1, 0.1, 0.5, 0.5), (1920, 1080), None);
        drive(&mut widget, &mut tree, press(), at(140.0, 78.0));
        drive(&mut widget, &mut tree, release(), at(140.0, 78.0));

        let away = mouse::Cursor::Unavailable;
        let mut last = None;
        for _ in 0..3 {
            let (msgs, _) = drive(
                &mut widget,
                &mut tree,
                key_press(Named::ArrowRight, Code::ArrowRight, Modifiers::default()),
                away,
            );
            last = Some(only_change(&msgs));
        }

        let next = last.expect("three nudges, three rects");
        assert_near(next.x, 0.1 + 3.0 * crop::STEP, 1e-5);
    }

    #[test]
    fn letting_go_of_another_key_does_not_end_the_nudge_batch() {
        // Shift comes up between two arrows all the time (a resize run turning into a move
        // one). Treating that as the end of the batch rebases the next nudge on the rect the
        // widget was built with, which within a frame is still the pre-nudge one.
        let (mut widget, mut tree) = harness(rect(0.1, 0.1, 0.5, 0.5), (1920, 1080), None);
        drive(&mut widget, &mut tree, press(), at(140.0, 78.0));
        drive(&mut widget, &mut tree, release(), at(140.0, 78.0));
        let away = mouse::Cursor::Unavailable;

        let (msgs, _) = drive(
            &mut widget,
            &mut tree,
            key_press(Named::ArrowRight, Code::ArrowRight, Modifiers::default()),
            away,
        );
        assert_near(only_change(&msgs).x, 0.1 + crop::STEP, 1e-5);

        for event in [
            key_release(Named::Shift, Code::ShiftLeft),
            char_release("a"),
        ] {
            let (msgs, _) = drive(&mut widget, &mut tree, event, away);
            assert!(msgs.is_empty(), "a release edits nothing: {msgs:?}");
        }

        let (msgs, _) = drive(
            &mut widget,
            &mut tree,
            key_press(Named::ArrowRight, Code::ArrowRight, Modifiers::default()),
            away,
        );
        assert_near(only_change(&msgs).x, 0.1 + 2.0 * crop::STEP, 1e-5);
    }

    #[test]
    fn keyboard_edits_after_click_focus() {
        let (mut widget, mut tree) = harness(rect(0.1, 0.1, 0.5, 0.5), (1920, 1080), None);

        // A press on the picture takes keyboard focus; releasing it unmoved is just a tap.
        drive(&mut widget, &mut tree, press(), at(140.0, 78.0));
        drive(&mut widget, &mut tree, release(), at(140.0, 78.0));

        let away = mouse::Cursor::Unavailable;
        let (msgs, captured) = drive(
            &mut widget,
            &mut tree,
            key_press(Named::ArrowRight, Code::ArrowRight, Modifiers::default()),
            away,
        );
        let next = only_change(&msgs);
        assert_near(next.x, 0.1 + crop::STEP, 1e-5);
        assert_near(next.width, 0.5, 1e-5);
        assert!(captured);

        // Shift resizes from the south-east corner instead of moving — of the rect the nudge
        // above published, which is not the one the widget was built with.
        let (msgs, _) = drive(
            &mut widget,
            &mut tree,
            key_press(Named::ArrowLeft, Code::ArrowLeft, Modifiers::SHIFT),
            away,
        );
        let next = only_change(&msgs);
        assert_near(next.width, 0.5 - crop::STEP, 1e-5);
        assert_near(next.x, 0.1 + crop::STEP, 1e-5);

        let (msgs, _) = drive(
            &mut widget,
            &mut tree,
            key_release(Named::ArrowLeft, Code::ArrowLeft),
            away,
        );
        assert!(msgs.is_empty(), "a key release edits nothing: {msgs:?}");

        let (msgs, captured) = drive(
            &mut widget,
            &mut tree,
            key_press(Named::Escape, Code::Escape, Modifiers::default()),
            away,
        );
        assert_eq!(msgs, vec![Msg::Reset]);
        assert!(captured);

        let (msgs, captured) = drive(
            &mut widget,
            &mut tree,
            key_press(Named::ArrowRight, Code::ArrowRight, Modifiers::default()),
            away,
        );
        assert!(
            msgs.is_empty(),
            "an unfocused overlay ignores keys: {msgs:?}"
        );
        assert!(!captured);
    }

    #[test]
    fn the_letterbox_bars_belong_to_the_layers_below() {
        // 3440x1440 in a 16:9 box letterboxes: ~29px of bar above and below the picture.
        let (mut widget, mut tree) = harness(crop::FULL_FRAME, (3440, 1440), None);
        let bar = at(200.0, 5.0);

        let (msgs, captured) = drive(&mut widget, &mut tree, press(), bar);
        assert!(msgs.is_empty(), "{msgs:?}");
        assert!(!captured, "a press on the bar must reach play/pause below");
        assert_eq!(interaction(&widget, &tree, bar), mouse::Interaction::None);
    }

    #[test]
    fn a_locked_ratio_survives_an_edge_drag() {
        // 16:9 out of a 21:9 frame: 0.744 of the frame's width per full height.
        let ratio = 0.744;
        let start = crop::max_rect_for_ratio(Some(ratio));
        let (mut widget, mut tree) = harness(start, (3440, 1440), Some(ratio));

        // The east handle sits on the rect's right edge, mid-height of the picture.
        drive(&mut widget, &mut tree, press(), at(348.8, 112.0));
        let (msgs, _) = drive(
            &mut widget,
            &mut tree,
            moved(300.0, 112.0),
            at(300.0, 112.0),
        );

        let next = only_change(&msgs);
        assert!(next.width < start.width, "the drag narrowed it: {next:?}");
        assert_near(next.width / next.height, ratio, 1e-3);
    }

    #[test]
    fn the_cursor_names_the_edit_under_it() {
        let (widget, tree) = harness(rect(0.25, 0.25, 0.5, 0.5), (1920, 1080), None);

        assert_eq!(
            interaction(&widget, &tree, at(100.0, 56.25)),
            mouse::Interaction::ResizingDiagonallyDown,
            "the north-west corner"
        );
        assert_eq!(
            interaction(&widget, &tree, at(300.0, 56.25)),
            mouse::Interaction::ResizingDiagonallyUp,
            "the north-east corner"
        );
        assert_eq!(
            interaction(&widget, &tree, at(300.0, 112.5)),
            mouse::Interaction::ResizingHorizontally,
            "the east edge"
        );
        assert_eq!(
            interaction(&widget, &tree, at(200.0, 56.25)),
            mouse::Interaction::ResizingVertically,
            "the north edge"
        );
        assert_eq!(
            interaction(&widget, &tree, at(200.0, 112.5)),
            mouse::Interaction::Grab,
            "the body"
        );
        assert_eq!(
            interaction(&widget, &tree, at(10.0, 10.0)),
            mouse::Interaction::None,
            "inside the picture but outside the rect stays clickable for play/pause"
        );
    }
}
