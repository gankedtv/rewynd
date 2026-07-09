//! Smooth wheel scrolling: a transparent wrapper around a `scrollable` that glides discrete
//! mouse-wheel steps at the display's refresh rate instead of letting them jump.
//!
//! iced's scrollable applies each wheel notch instantly, which reads as stutter on any
//! display. This wrapper intercepts only Lines wheel deltas (a physical wheel), accumulates
//! them into a target offset, and eases the real scrollable toward it by one step per
//! `RedrawRequested` frame through the widget-operation API. Everything else — touchpad
//! Pixels deltas (already smooth), scrollbar drags, keyboard paging — passes straight
//! through to the native scrollable, and any of those cancels a running glide so the two
//! never fight over the offset.

use std::time::{Duration, Instant};

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::operation::Scrollable;
use iced::advanced::widget::operation::scrollable::AbsoluteOffset;
use iced::advanced::widget::{Id, Operation, Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget, mouse, overlay, renderer};
use iced::{Element, Event, Length, Rectangle, Size, Vector, keyboard, window};

/// Pixels per wheel-line notch, matching what the native scrollable applies so the scroll
/// distance per notch is unchanged.
const LINE_PX: f32 = 60.0;

/// Exponential easing rate (per second): the glide covers ~95% of the remaining distance in
/// `3 / RATE` seconds, so ~140 ms to settle.
const RATE: f32 = 22.0;

/// Cap on one frame's time step, so the first frame after an idle gap (or a hitch) advances
/// the glide instead of snapping it to the end.
const MAX_STEP: Duration = Duration::from_millis(50);

/// Distance (px) under which the glide snaps to its target and ends.
const SETTLE: f32 = 0.5;

/// Wrap a `scrollable` so wheel scrolling glides. The wrapper drives the first scrollable in
/// `content` (its direct child); it is transparent for everything else.
pub fn smooth<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    Element::new(Smooth {
        content: content.into(),
    })
}

struct Smooth<'a, Message> {
    content: Element<'a, Message>,
}

#[derive(Default)]
struct State {
    /// The offset the glide is heading for, while one runs.
    target: Option<f32>,
    /// The previous animation frame's instant, for the easing time step.
    last_tick: Option<Instant>,
    /// Whether Shift is held: the native scrollable turns that wheel into a horizontal
    /// scroll, so it is passed through untouched.
    shift: bool,
}

/// One visit of the wrapped scrollable: reads its geometry and, when a glide step is
/// requested, writes the eased offset back.
struct Visit {
    /// The glide to advance, if any: (target offset, easing factor for this frame).
    step: Option<(f32, f32)>,
    /// The scrollable's current offset, once visited.
    offset: Option<f32>,
    /// How far the content can scroll (0 when it fits).
    max: f32,
    /// Whether the glide reached its target this step.
    done: bool,
}

impl Visit {
    fn probe() -> Self {
        Self {
            step: None,
            offset: None,
            max: 0.0,
            done: false,
        }
    }

    fn step(target: f32, factor: f32) -> Self {
        Self {
            step: Some((target, factor)),
            ..Self::probe()
        }
    }
}

impl Operation for Visit {
    // No descent: only the wrapper's direct scrollable child is driven, so a scrollable
    // nested deeper in the page can never be captured by mistake.
    fn traverse(&mut self, _operate: &mut dyn FnMut(&mut dyn Operation)) {}

    fn scrollable(
        &mut self,
        _id: Option<&Id>,
        bounds: Rectangle,
        content_bounds: Rectangle,
        translation: Vector,
        state: &mut dyn Scrollable,
    ) {
        if self.offset.is_some() {
            return;
        }
        self.offset = Some(translation.y);
        self.max = (content_bounds.height - bounds.height).max(0.0);
        if let Some((target, factor)) = self.step {
            // Clamp per frame, so a resize mid-glide can't ease past the new end.
            let target = target.clamp(0.0, self.max);
            let eased = translation.y + (target - translation.y) * factor;
            self.done = (target - eased).abs() < SETTLE;
            let next = if self.done { target } else { eased };
            state.scroll_to(AbsoluteOffset {
                x: None,
                y: Some(next),
            });
        }
    }
}

impl<Message> Smooth<'_, Message> {
    /// Run `visit` against the wrapped scrollable.
    fn visit(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        visit: &mut Visit,
    ) {
        if let Some(child) = layout.children().next() {
            self.content
                .as_widget_mut()
                .operate(&mut tree.children[0], child, renderer, visit);
        }
    }
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for Smooth<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let child = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        let size = child.size();
        layout::Node::with_children(size, vec![child])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            if let Some(child) = layout.children().next() {
                self.content.as_widget_mut().operate(
                    &mut tree.children[0],
                    child,
                    renderer,
                    operation,
                );
            }
        });
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if let Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) = event {
            tree.state.downcast_mut::<State>().shift = modifiers.shift();
        }

        // A wheel notch over the area starts (or extends) a glide instead of reaching the
        // scrollable. Shift is horizontal scrolling in the native widget: pass it through.
        if let Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Lines { y, .. },
        }) = event
            && cursor.is_over(layout.bounds())
            && !tree.state.downcast_ref::<State>().shift
        {
            let mut probe = Visit::probe();
            self.visit(tree, layout, renderer, &mut probe);
            if let Some(current) = probe.offset
                && probe.max > 0.0
            {
                let state = tree.state.downcast_mut::<State>();
                let from = state.target.unwrap_or(current);
                if state.target.is_none() {
                    state.last_tick = Some(Instant::now());
                }
                state.target = Some((from - y * LINE_PX).clamp(0.0, probe.max));
                shell.capture_event();
                shell.request_redraw();
                return;
            }
        }

        // Touchpad deltas, presses and touches interact with the offset directly; a running
        // glide yields to them rather than fighting.
        if matches!(
            event,
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { .. },
            }) | Event::Mouse(mouse::Event::ButtonPressed(_))
                | Event::Touch(_)
        ) {
            tree.state.downcast_mut::<State>().target = None;
        }

        if let Some(child) = layout.children().next() {
            self.content.as_widget_mut().update(
                &mut tree.children[0],
                event,
                child,
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
        }

        // One glide step per frame: this runs at the display's refresh rate for as long as a
        // redraw keeps being requested.
        if let Event::Window(window::Event::RedrawRequested(now)) = event {
            let state = tree.state.downcast_mut::<State>();
            let Some(target) = state.target else {
                return;
            };
            let dt = state
                .last_tick
                .map_or(MAX_STEP, |last| now.duration_since(last))
                .min(MAX_STEP);
            state.last_tick = Some(*now);
            let factor = 1.0 - (-RATE * dt.as_secs_f32()).exp();

            let mut step = Visit::step(target, factor);
            self.visit(tree, layout, renderer, &mut step);
            let state = tree.state.downcast_mut::<State>();
            if step.done || step.offset.is_none() {
                state.target = None;
            } else {
                shell.request_redraw();
            }
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        layout
            .children()
            .next()
            .map_or_else(Default::default, |child| {
                self.content.as_widget().mouse_interaction(
                    &tree.children[0],
                    child,
                    cursor,
                    viewport,
                    renderer,
                )
            })
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        if let Some(child) = layout.children().next() {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                child,
                cursor,
                viewport,
            );
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout.children().next()?,
            renderer,
            viewport,
            translation,
        )
    }
}
