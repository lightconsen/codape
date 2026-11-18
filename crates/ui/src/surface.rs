//! The terminal surface: the only surface Codape ships.
//!
//! The abstraction lives in [`codape_core::surface`]; this module is its
//! terminal implementation. It reuses the plain-text renderer so headless
//! transcripts, wire clients and tests present mod UI exactly as the TUI's
//! compact form does.

use codape_core::surface::{SurfaceDescriptor, UiSurface};
use codape_core::ui_protocol::UiElement;

use crate::mod_elements::render_mod_ui_text;
use crate::transcript_image::ImageProtocol;

/// The terminal surface.
///
/// Keyboard focus and mouse clicks can activate buttons, and the picture
/// capability is whatever the terminal this process is attached to can
/// actually do — see [`TerminalSurface::new`].
#[derive(Debug, Clone)]
pub struct TerminalSurface {
    descriptor: SurfaceDescriptor,
}

impl TerminalSurface {
    /// Build the terminal surface descriptor from the live terminal.
    ///
    /// [`SurfaceDescriptor::terminal`] is the conservative constant and says
    /// `images: false`, which was true when it was written and is now only
    /// true of some terminals. The transcript has drawn pictures as half-block
    /// cells for a while ([`crate::transcript_image`]), and a mod that reads
    /// `e.surfaceCapabilities.images` was being told it could not — so a mod
    /// that would have shown a plot showed a caption instead.
    ///
    /// The answer comes from the same detection the transcript uses, so the
    /// capability and the renderer cannot disagree: a terminal without
    /// truecolor gets `false` here exactly as it gets `ImageProtocol::None`
    /// there.
    #[must_use]
    pub fn new() -> Self {
        Self::with_images(ImageProtocol::from_env().can_draw())
    }

    /// Build the descriptor with the picture capability stated outright.
    ///
    /// The pure seam: a caller that already knows what the surface can do —
    /// a test, or a host presenting a session somewhere other than a terminal
    /// it can probe — does not have to consult the environment.
    #[must_use]
    pub fn with_images(images: bool) -> Self {
        let mut descriptor = SurfaceDescriptor::terminal();
        descriptor.capabilities.images = images;
        Self { descriptor }
    }
}

impl Default for TerminalSurface {
    fn default() -> Self {
        Self::new()
    }
}

impl UiSurface for TerminalSurface {
    fn descriptor(&self) -> &SurfaceDescriptor {
        &self.descriptor
    }

    fn render_text(&self, elements: &[UiElement], width: usize) -> String {
        render_mod_ui_text(elements, width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codape_core::surface::UiSurface;
    use codape_core::ui_protocol::UiStyle;

    #[test]
    fn the_terminal_surface_names_itself_and_reports_its_capabilities() {
        let surface = TerminalSurface::with_images(true);
        let descriptor = surface.descriptor();
        assert_eq!(descriptor.id.as_str(), "terminal");
        assert!(descriptor.capabilities.interactive);
        assert!(descriptor.capabilities.mouse);
        assert!(descriptor.capabilities.images);
    }

    /// A terminal without truecolor draws no pictures, and the descriptor has
    /// to agree with the renderer rather than always claiming the capability.
    #[test]
    fn the_picture_capability_follows_the_terminal() {
        assert!(
            !TerminalSurface::with_images(false)
                .descriptor()
                .capabilities
                .images
        );
    }

    #[test]
    fn the_terminal_surface_renders_the_shared_text_form() {
        let surface = TerminalSurface::with_images(false);
        let elements = vec![
            UiElement::Text {
                text: "hello".to_string(),
                style: UiStyle::Plain,
            },
            UiElement::Button {
                label: "Retry".to_string(),
                id: "retry".to_string(),
            },
        ];
        let rendered = surface.render_text(&elements, 40);
        assert!(rendered.contains("hello"), "{rendered}");
        assert!(rendered.contains("Retry"), "{rendered}");
    }
}
