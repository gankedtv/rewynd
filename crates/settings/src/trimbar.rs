//! An editor-style trim bar: two draggable handles over the filmstrip. It is the top layer of
//! the trim card's stack, so the filmstrip frames show through beneath; the kept `[start, end]`
//! range stays bright while the rest is scrimmed. Dragging a handle publishes the new time via
//! the caller's message constructors (which clamp, so the widget stays dumb about limits).

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget, mouse, renderer};
use iced::{Background, Border, Color, Element, Event, Length, Rectangle, Size};

use crate::theme::palette;

/// Which handle the pointer grabbed on press.
#[derive(Clone, Copy)]
enum Handle {
    Start,
    End,
}

#[derive(Default)]
struct State {
    drag: Option<Handle>,
}

/// A draggable trim range over a clip of `dur` seconds.
pub struct TrimBar<'a, Message> {
    start: f32,
    end: f32,
    dur: f32,
    /// Playback position to mark with a vertical line, when the preview is playing.
    playhead: Option<f32>,
    on_start: Box<dyn Fn(f32) -> Message + 'a>,
    on_end: Box<dyn Fn(f32) -> Message + 'a>,
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
            on_start: Box::new(on_start),
            on_end: Box::new(on_end),
        }
    }

    /// Mark the playback position with a vertical line.
    pub fn playhead(mut self, secs: Option<f32>) -> Self {
        self.playhead = secs;
        self
    }

    /// The x pixel for clip time `secs`, within a bar spanning `[left, left + width]`.
    fn pixel_of(&self, secs: f32, left: f32, width: f32) -> f32 {
        left + (secs / self.dur).clamp(0.0, 1.0) * width
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
                    let sx = self.pixel_of(self.start, bounds.x, bounds.width);
                    let ex = self.pixel_of(self.end, bounds.x, bounds.width);
                    let handle = if nearer_is_start(pos.x, sx, ex) {
                        Handle::Start
                    } else {
                        Handle::End
                    };
                    state.drag = Some(handle);
                    // Snap the grabbed edge to the click, then the drag fine-tunes from there.
                    let t = time_at(pos.x, bounds.x, bounds.width, self.dur);
                    shell.publish(match handle {
                        Handle::Start => (self.on_start)(t),
                        Handle::End => (self.on_end)(t),
                    });
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                if let Some(handle) = state.drag
                    && let Some(pos) = cursor.position()
                {
                    let t = time_at(pos.x, bounds.x, bounds.width, self.dur);
                    shell.publish(match handle {
                        Handle::Start => (self.on_start)(t),
                        Handle::End => (self.on_end)(t),
                    });
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.drag = None;
            }
            _ => {}
        }
    }

    fn draw(
        &self,
        _tree: &Tree,
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
            mouse::Interaction::Grabbing
        } else if cursor.is_over(layout.bounds()) {
            mouse::Interaction::ResizingHorizontally
        } else {
            mouse::Interaction::default()
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
    Message: 'a,
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
}
