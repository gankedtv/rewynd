//! An editor-style trim bar: two draggable handles over the filmstrip. It is the top layer of
//! the trim card's stack, so the filmstrip frames show through beneath; the kept `[start, end]`
//! range stays bright while the rest is scrimmed. Dragging a handle publishes the new time via
//! the caller's message constructors (which clamp, so the widget stays dumb about limits).
//!
//! Clicking the bar also focuses it for keyboard editing: arrows seek, I/O set the trim
//! in/out points at the playhead, Home/End jump to the range edges, Space toggles playback,
//! Escape unfocuses.

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget, mouse, renderer};
use iced::keyboard::{self, Key, key::Named};
use iced::{Background, Border, Color, Element, Event, Length, Rectangle, Size};

use crate::theme::palette;

/// Which handle the pointer grabbed on press.
#[derive(Clone, Copy)]
enum Handle {
    Start,
    End,
    /// Anywhere between the edges: move the playhead (seek).
    Seek,
}

/// How near (px) a press must be to a trim edge to grab it; further away it seeks instead.
const GRAB: f32 = 10.0;

#[derive(Default)]
struct State {
    drag: Option<Handle>,
    /// Whether keyboard input edits this bar (gained by clicking it, lost by clicking
    /// elsewhere or Escape).
    focused: bool,
    /// A keyboard edit (seek or mark) is in progress; the release message goes out once,
    /// on key release, like a drag end. Publishing it per press would tear down and
    /// respawn the preview player at the OS key-repeat rate.
    keys_engaged: bool,
    /// Space is held; auto-repeat must not toggle playback on and off.
    space_down: bool,
}

/// A draggable trim range over a clip of `dur` seconds.
pub struct TrimBar<'a, Message> {
    start: f32,
    end: f32,
    dur: f32,
    /// Playback position to mark with a vertical line, when the preview is playing.
    playhead: Option<f32>,
    /// Normalized audio peaks (`0.0..=1.0`) drawn as a lane along the bottom, so speech and
    /// action are visible while picking the range.
    waveform: Option<&'a [f32]>,
    on_start: Box<dyn Fn(f32) -> Message + 'a>,
    on_end: Box<dyn Fn(f32) -> Message + 'a>,
    /// Pressing/dragging between the edges moves the playhead here; without it such a press
    /// grabs the nearest edge instead.
    on_seek: Option<Box<dyn Fn(f32) -> Message + 'a>>,
    /// Published when a drag (of any handle) is let go.
    on_released: Option<Message>,
    /// Published when Space is pressed while the bar has keyboard focus.
    on_toggle: Option<Message>,
}

impl<'a, Message> TrimBar<'a, Message> {
    pub fn new(
        start: f32,
        end: f32,
        dur: f32,
        on_start: impl Fn(f32) -> Message + 'a,
        on_end: impl Fn(f32) -> Message + 'a,
    ) -> Self {
        Self {
            start,
            end,
            // A zero duration would divide by zero when placing the handles.
            dur: dur.max(f32::EPSILON),
            playhead: None,
            waveform: None,
            on_start: Box::new(on_start),
            on_end: Box::new(on_end),
            on_seek: None,
            on_released: None,
            on_toggle: None,
        }
    }

    /// Let presses between the edges move the playhead (seek) instead of grabbing an edge.
    pub fn on_seek(mut self, seek: impl Fn(f32) -> Message + 'a) -> Self {
        self.on_seek = Some(Box::new(seek));
        self
    }

    /// Publish `message` when a drag is released.
    pub fn on_released(mut self, message: Message) -> Self {
        self.on_released = Some(message);
        self
    }

    /// Publish `message` when Space is pressed while the bar has keyboard focus.
    pub fn on_toggle(mut self, message: Message) -> Self {
        self.on_toggle = Some(message);
        self
    }

    /// Mark the playback position with a vertical line.
    pub fn playhead(mut self, secs: Option<f32>) -> Self {
        self.playhead = secs;
        self
    }

    /// Draw an audio-peak lane along the bottom of the bar.
    pub fn waveform(mut self, peaks: Option<&'a [f32]>) -> Self {
        self.waveform = peaks;
        self
    }

    /// The x pixel for clip time `secs`, within a bar spanning `[left, left + width]`.
    fn pixel_of(&self, secs: f32, left: f32, width: f32) -> f32 {
        left + (secs / self.dur).clamp(0.0, 1.0) * width
    }

    /// Send the drag's message for time `t` through the right callback.
    fn publish(&self, handle: Handle, t: f32, shell: &mut Shell<'_, Message>) {
        match handle {
            Handle::Start => shell.publish((self.on_start)(t)),
            Handle::End => shell.publish((self.on_end)(t)),
            Handle::Seek => {
                if let Some(seek) = &self.on_seek {
                    shell.publish(seek(t));
                }
            }
        }
    }

    /// Publish the release message, so keyboard edits end like a drag would (the caller's
    /// resume-playback logic runs either way).
    fn release(&self, shell: &mut Shell<'_, Message>)
    where
        Message: Clone,
    {
        if let Some(message) = &self.on_released {
            shell.publish(message.clone());
        }
    }

    /// Seek the playhead to `t`, clamped inside the kept range.
    fn seek(&self, t: f32, shell: &mut Shell<'_, Message>) {
        if let Some(seek) = &self.on_seek {
            shell.publish(seek(t.clamp(self.start, self.end)));
        }
    }
}

/// Arrow-key seek size in seconds: fine with Shift, coarse with Ctrl, one second otherwise.
fn seek_step(modifiers: keyboard::Modifiers) -> f32 {
    if modifiers.shift() {
        0.1
    } else if modifiers.control() {
        5.0
    } else {
        1.0
    }
}

/// The clip time (seconds) under pixel `x`, for a bar at `left` of `width` px over `dur` seconds.
fn time_at(x: f32, left: f32, width: f32, dur: f32) -> f32 {
    if width <= 0.0 {
        return 0.0;
    }
    ((x - left) / width).clamp(0.0, 1.0) * dur
}

/// Whether the start handle (at `start_x`) is at least as near pixel `x` as the end (at `end_x`);
/// ties go to the start so a click exactly between them grabs the left edge.
fn nearer_is_start(x: f32, start_x: f32, end_x: f32) -> bool {
    (x - start_x).abs() <= (x - end_x).abs()
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for TrimBar<'_, Message>
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
        let bounds = layout.bounds();
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let Some(pos) = cursor.position_over(bounds) {
                    state.focused = true;
                    let sx = self.pixel_of(self.start, bounds.x, bounds.width);
                    let ex = self.pixel_of(self.end, bounds.x, bounds.width);
                    // Near an edge grabs it to trim; anywhere else seeks the playhead (or,
                    // without a seek handler, snaps the nearest edge like before).
                    let handle = if (pos.x - sx).abs() <= GRAB {
                        Handle::Start
                    } else if (pos.x - ex).abs() <= GRAB {
                        Handle::End
                    } else if self.on_seek.is_some() {
                        Handle::Seek
                    } else if nearer_is_start(pos.x, sx, ex) {
                        Handle::Start
                    } else {
                        Handle::End
                    };
                    state.drag = Some(handle);
                    let t = time_at(pos.x, bounds.x, bounds.width, self.dur);
                    self.publish(handle, t, shell);
                    shell.capture_event();
                } else if state.focused {
                    // A click anywhere else moves focus away; the click itself stays uncaptured
                    // so whatever was pressed still gets it.
                    state.focused = false;
                    shell.request_redraw();
                }
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                if let Some(handle) = state.drag
                    && let Some(pos) = cursor.position()
                {
                    let t = time_at(pos.x, bounds.x, bounds.width, self.dur);
                    self.publish(handle, t, shell);
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if state.drag.take().is_some()
                    && let Some(message) = &self.on_released
                {
                    shell.publish(message.clone());
                }
            }
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. })
                if state.focused =>
            {
                // Chorded presses (Ctrl/Alt/Super) belong to someone else's shortcut; editing
                // the trim on them would both corrupt the range and shadow the shortcut.
                // Shift stays available (it is the fine arrow step and the I/O capitals).
                let chord = modifiers.control() || modifiers.alt() || modifiers.logo();
                // The playhead the edit applies to; before playback starts it sits at the
                // relevant range edge.
                let head = |fallback: f32| {
                    self.playhead
                        .unwrap_or(fallback)
                        .clamp(self.start, self.end)
                };
                match key.as_ref() {
                    Key::Named(Named::Escape) => {
                        state.focused = false;
                        shell.request_redraw();
                        shell.capture_event();
                    }
                    Key::Named(dir @ (Named::ArrowLeft | Named::ArrowRight))
                        if !(modifiers.alt() || modifiers.logo()) =>
                    {
                        let sign = if dir == Named::ArrowLeft { -1.0 } else { 1.0 };
                        self.seek(head(self.start) + sign * seek_step(*modifiers), shell);
                        state.keys_engaged = true;
                        shell.capture_event();
                    }
                    Key::Named(Named::Home) if !chord => {
                        self.seek(self.start, shell);
                        state.keys_engaged = true;
                        shell.capture_event();
                    }
                    Key::Named(Named::End) if !chord => {
                        self.seek(self.end, shell);
                        state.keys_engaged = true;
                        shell.capture_event();
                    }
                    // The editor in/out idiom: mark a trim point at the playhead. The seek
                    // right after keeps the playhead where it was; the mark alone would
                    // clear it and send the next arrow press back to the range edge.
                    Key::Character("i" | "I") if !chord => {
                        let t = head(self.start);
                        shell.publish((self.on_start)(t));
                        self.seek(t, shell);
                        state.keys_engaged = true;
                        shell.capture_event();
                    }
                    Key::Character("o" | "O") if !chord => {
                        let t = head(self.end);
                        shell.publish((self.on_end)(t));
                        self.seek(t, shell);
                        state.keys_engaged = true;
                        shell.capture_event();
                    }
                    Key::Named(Named::Space) | Key::Character(" ") if !chord => {
                        if !state.space_down
                            && let Some(message) = &self.on_toggle
                        {
                            shell.publish(message.clone());
                        }
                        state.space_down = true;
                        // Captured even when held so the page does not scroll.
                        shell.capture_event();
                    }
                    _ => {}
                }
            }
            Event::Keyboard(keyboard::Event::KeyReleased { key, .. }) if state.focused => {
                if matches!(key.as_ref(), Key::Named(Named::Space) | Key::Character(" ")) {
                    state.space_down = false;
                }
                if state.keys_engaged {
                    state.keys_engaged = false;
                    self.release(shell);
                    shell.capture_event();
                }
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
        let b = layout.bounds();
        let sx = self.pixel_of(self.start, b.x, b.width);
        let ex = self.pixel_of(self.end, b.x, b.width);
        // The audio lane first, so the scrim dims it outside the kept window like the frames.
        if let Some(peaks) = self.waveform
            && !peaks.is_empty()
        {
            let lane = b.height * 0.45;
            let step = b.width / peaks.len() as f32;
            let tint = Color {
                a: 0.55,
                ..palette::ACCENT
            };
            for (i, peak) in peaks.iter().enumerate() {
                let h = (peak * lane).max(1.0);
                fill(
                    renderer,
                    rect(b.x + i as f32 * step, b.y + b.height - h, step, h),
                    tint,
                );
            }
        }
        // The scrim over everything outside the kept window, so what stays lit is what is kept.
        let scrim = Color {
            a: 0.55,
            ..palette::BACKGROUND
        };
        if sx > b.x {
            fill(renderer, rect(b.x, b.y, sx - b.x, b.height), scrim);
        }
        if ex < b.x + b.width {
            fill(renderer, rect(ex, b.y, b.x + b.width - ex, b.height), scrim);
        }
        // A mint frame around the kept window (transparent fill, so the frames show through).
        renderer.fill_quad(
            renderer::Quad {
                bounds: rect(sx, b.y, (ex - sx).max(0.0), b.height),
                border: Border {
                    color: palette::ACCENT,
                    width: 2.0,
                    radius: 4.0.into(),
                },
                ..renderer::Quad::default()
            },
            Background::Color(Color::TRANSPARENT),
        );
        if let Some(secs) = self.playhead {
            let x = self.pixel_of(secs, b.x, b.width);
            fill(
                renderer,
                rect(
                    (x - 1.0).clamp(b.x, b.x + b.width - 2.0),
                    b.y,
                    2.0,
                    b.height,
                ),
                palette::TEXT,
            );
        }
        handle(renderer, sx, b);
        handle(renderer, ex, b);
        // A focus ring around the whole bar, so keyboard users see where input goes.
        if tree.state.downcast_ref::<State>().focused {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: b,
                    border: Border {
                        color: palette::ACCENT,
                        width: 1.0,
                        radius: 6.0.into(),
                    },
                    ..renderer::Quad::default()
                },
                Background::Color(Color::TRANSPARENT),
            );
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
        let dragging = tree.state.downcast_ref::<State>().drag.is_some();
        if dragging {
            return mouse::Interaction::Grabbing;
        }
        let bounds = layout.bounds();
        let Some(pos) = cursor.position_over(bounds) else {
            return mouse::Interaction::default();
        };
        let sx = self.pixel_of(self.start, bounds.x, bounds.width);
        let ex = self.pixel_of(self.end, bounds.x, bounds.width);
        if (pos.x - sx).abs() <= GRAB || (pos.x - ex).abs() <= GRAB || self.on_seek.is_none() {
            mouse::Interaction::ResizingHorizontally
        } else {
            mouse::Interaction::Pointer
        }
    }
}

/// A rectangle from top-left + size.
fn rect(x: f32, y: f32, width: f32, height: f32) -> Rectangle {
    Rectangle {
        x,
        y,
        width,
        height,
    }
}

/// Fill `bounds` with a flat colour.
fn fill<Renderer: renderer::Renderer>(renderer: &mut Renderer, bounds: Rectangle, color: Color) {
    renderer.fill_quad(
        renderer::Quad {
            bounds,
            ..renderer::Quad::default()
        },
        Background::Color(color),
    );
}

/// A handle: a full-height mint bar centred on `x`, with a fatter rounded grip in the middle.
fn handle<Renderer: renderer::Renderer>(renderer: &mut Renderer, x: f32, b: Rectangle) {
    let bar_w = 3.0;
    let bar_x = (x - bar_w / 2.0).clamp(b.x, b.x + b.width - bar_w);
    renderer.fill_quad(
        renderer::Quad {
            bounds: rect(bar_x, b.y, bar_w, b.height),
            ..renderer::Quad::default()
        },
        Background::Color(palette::ACCENT),
    );
    let grip_w = 8.0;
    let grip_h = (b.height * 0.42).min(24.0);
    let grip_x = (x - grip_w / 2.0).clamp(b.x, b.x + b.width - grip_w);
    renderer.fill_quad(
        renderer::Quad {
            bounds: rect(grip_x, b.y + (b.height - grip_h) / 2.0, grip_w, grip_h),
            border: Border {
                radius: 3.0.into(),
                ..Border::default()
            },
            ..renderer::Quad::default()
        },
        Background::Color(palette::ACCENT),
    );
}

impl<'a, Message, Theme, Renderer> From<TrimBar<'a, Message>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: Clone + 'a,
    Theme: 'a,
    Renderer: renderer::Renderer + 'a,
{
    fn from(bar: TrimBar<'a, Message>) -> Self {
        Element::new(bar)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_at_maps_pixels_to_seconds() {
        // A 200px bar starting at x=50 over a 10s clip.
        assert_eq!(time_at(50.0, 50.0, 200.0, 10.0), 0.0);
        assert_eq!(time_at(250.0, 50.0, 200.0, 10.0), 10.0);
        assert_eq!(time_at(150.0, 50.0, 200.0, 10.0), 5.0);
        // Outside the bar clamps to the ends.
        assert_eq!(time_at(0.0, 50.0, 200.0, 10.0), 0.0);
        assert_eq!(time_at(999.0, 50.0, 200.0, 10.0), 10.0);
        // Degenerate width never divides by zero.
        assert_eq!(time_at(10.0, 0.0, 0.0, 10.0), 0.0);
    }

    #[test]
    fn nearer_handle_picks_the_closer_edge() {
        assert!(nearer_is_start(10.0, 0.0, 100.0), "near the left edge");
        assert!(!nearer_is_start(90.0, 0.0, 100.0), "near the right edge");
        assert!(nearer_is_start(50.0, 0.0, 100.0), "a tie goes to the start");
    }

    #[test]
    fn seek_step_scales_with_modifiers() {
        use keyboard::Modifiers;
        assert_eq!(seek_step(Modifiers::default()), 1.0);
        assert_eq!(seek_step(Modifiers::SHIFT), 0.1);
        assert_eq!(seek_step(Modifiers::CTRL), 5.0);
        // Shift wins when both are held: the finer nudge is the safer guess.
        assert_eq!(seek_step(Modifiers::SHIFT | Modifiers::CTRL), 0.1);
        assert_eq!(seek_step(Modifiers::ALT), 1.0);
    }
}
