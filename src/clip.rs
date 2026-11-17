//! A wrapper that keeps what it holds from drawing outside its own box.
//!
//! Why it exists: the software renderer (tiny-skia in Iced 0.13) ignores a text's own
//! clip box, and a text field only clips its words while they are being selected. So a
//! value longer than its field (an install path, a pasted file path) is drawn straight
//! over whatever sits to the right of it. `container(..).clip(true)` does not help: it
//! only narrows that same ignored box. A real layer is the one thing the renderer
//! honours, and this widget is nothing but that layer: everything else is handed
//! through to the content untouched (the pattern of Iced's own `Themer`).

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{tree, Operation, Tree};
use iced::advanced::{mouse, overlay, renderer, Clipboard, Renderer as _, Shell, Widget};
use iced::{event, Element, Event, Length, Rectangle, Renderer, Size, Theme, Vector};

struct Clip<'a, Message> {
    content: Element<'a, Message>,
}

/// `content`, drawn only inside its own bounds.
pub(crate) fn clip<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    Element::new(Clip { content: content.into() })
}

impl<Message> Widget<Message, Theme, Renderer> for Clip<'_, Message> {
    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }

    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }

    fn diff(&self, tree: &mut Tree) {
        self.content.as_widget().diff(tree);
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(&self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget().layout(tree, renderer, limits)
    }

    fn operate(&self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        self.content.as_widget().operate(tree, layout, renderer, operation);
    }

    fn on_event(
        &mut self,
        tree: &mut Tree,
        event: Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) -> event::Status {
        self.content.as_widget_mut().on_event(tree, event, layout, cursor, renderer, clipboard, shell, viewport)
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        // The one thing this widget does.
        if let Some(visible) = layout.bounds().intersection(viewport) {
            renderer.with_layer(visible, |renderer| {
                self.content.as_widget().draw(tree, renderer, theme, style, layout, cursor, &visible);
            });
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(tree, layout, renderer, translation)
    }
}
