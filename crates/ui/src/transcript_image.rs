//! Drawing an attached picture in the transcript.
//!
//! A turn can carry an image — `--image shot.png`, `@shot.png`, a Ctrl+V
//! paste — and the transcript used to show only the text `[image: image/png]`.
//! This module turns the attached bytes into rows the existing message list can
//! paint.
//!
//! **Why half blocks rather than a graphics protocol.** kitty, WezTerm, Ghostty
//! and iTerm2 can place real pixels, and that is what `ratatui-image` does. But
//! those protocols write *outside* ratatui's cell buffer: the image is anchored
//! to a screen position and the terminal keeps it there while the text beneath
//! scrolls. Codape's transcript virtualises, scrolls by rendered line, caches
//! line spans, and hit-tests mod UI blocks by row — an image that lives outside
//! that buffer would be painted at a position the buffer knows nothing about,
//! so it would drift, duplicate, and survive a redraw that moved the text.
//!
//! Half blocks avoid all of that: `▀` with the top pixel as the foreground and
//! the bottom pixel as the background puts two rows of pixels in one cell, and
//! the picture becomes ordinary `Span`s. Scrolling, the line cache, the
//! selection snapshot and the virtualised viewport then work on an image
//! exactly as they work on text, with no special cases. The cost is resolution
//! — two pixels per cell rather than one per sub-cell — which is the price of
//! not fighting the renderer.
//!
//! Where 24-bit colour is unavailable the row keeps its `[image: …]` caption;
//! that is the caller's decision, made once from [`ImageProtocol`].

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::terminal_profile::{TerminalKind, TerminalProfile};
use crate::themes::Theme;
use codape_core::types::ImageBlock;

/// Environment variable that overrides the automatic image-rendering choice.
///
/// `blocks` / `on` forces half blocks, `off` / `none` forces the caption. Any
/// other value, including unset, falls back to detection.
pub const IMAGE_PROTOCOL_ENV: &str = "CODAPE_IMAGE_PROTOCOL";

/// The character that carries two pixels: foreground on top, background below.
const HALF_BLOCK: &str = "\u{2580}";

/// Rows a picture may occupy in the transcript.
///
/// A tall screenshot is common, and without a ceiling one of them would push
/// the conversation it belongs to off the screen. The picture is scaled to fit,
/// never cropped.
pub const MAX_IMAGE_ROWS: usize = 20;

/// Decoded pixels above this count are not drawn.
///
/// The attachment limit is on *encoded* bytes, so a 5 MiB PNG can decode to a
/// couple of hundred megapixels. Scaling that down costs more than the draw is
/// worth, and the caption still names the attachment.
pub const MAX_DECODE_PIXELS: u64 = 40_000_000;

/// How the transcript can show a picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageProtocol {
    /// No picture: the row keeps its `[image: …]` caption.
    #[default]
    None,
    /// Half-block cells with 24-bit colour.
    HalfBlocks,
}

impl ImageProtocol {
    /// Detect from the real process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self::detect(&TerminalProfile::from_env(), &|name| {
            std::env::var(name).ok()
        })
    }

    /// Detect from a caller-supplied terminal profile and environment lookup.
    ///
    /// Pure, so every rule below is testable without touching the process
    /// environment.
    #[must_use]
    pub fn detect(profile: &TerminalProfile, get: &impl Fn(&str) -> Option<String>) -> Self {
        match get(IMAGE_PROTOCOL_ENV).as_deref() {
            Some("blocks" | "on" | "half" | "truecolor") => return Self::HalfBlocks,
            Some("off" | "none" | "text") => return Self::None,
            _ => {}
        }
        if supports_truecolor(profile.kind, get("COLORTERM").as_deref()) {
            Self::HalfBlocks
        } else {
            Self::None
        }
    }

    /// Whether this choice draws pictures.
    #[must_use]
    pub const fn can_draw(self) -> bool {
        matches!(self, Self::HalfBlocks)
    }
}

/// Whether the terminal's SGR understands 24-bit colour.
///
/// `COLORTERM` is the only portable signal, and the terminals that omit it are
/// listed by name. Terminal.app is deliberately absent: it never gained 24-bit
/// SGR, and asking it for `38;2;r;g;b` leaves the cell at the default colour —
/// a half block in the wrong colour reads as a broken picture, which is worse
/// than the caption.
#[must_use]
fn supports_truecolor(kind: TerminalKind, colorterm: Option<&str>) -> bool {
    if matches!(
        colorterm.map(str::to_ascii_lowercase).as_deref(),
        Some("truecolor" | "24bit")
    ) {
        return true;
    }
    matches!(
        kind,
        TerminalKind::Kitty
            | TerminalKind::WezTerm
            | TerminalKind::Ghostty
            | TerminalKind::Iterm2
            | TerminalKind::VsCode
            | TerminalKind::Cursor
            | TerminalKind::WindowsTerminal
    )
}

/// One decoded picture, as colours to paint.
#[derive(Debug)]
pub struct RenderedImage {
    /// Cells across.
    pub cols: usize,
    /// Cell rows down.
    pub rows: usize,
    /// One `(top, bottom)` pixel pair per cell, row-major.
    pub cells: Vec<(Color, Color)>,
}

/// An attached picture plus the last rendering of it.
///
/// The pixel size is read from the image header when the row is created, so the
/// layout can reserve the right number of rows without decoding anything. The
/// decode itself happens the first time the row is actually drawn, and the
/// result is kept for the width it was drawn at.
#[derive(Debug)]
pub struct ImageState {
    /// The attached bytes.
    block: ImageBlock,
    /// Pixel size from the header, when it could be read.
    pixels: Option<(u32, u32)>,
    /// The last rendering, for the width and background it was made at.
    ///
    /// A failed decode is cached too (`None` inside the pair): a corrupt or
    /// truncated payload would otherwise be re-decoded on every frame that
    /// keeps its row on screen.
    rendered: Mutex<Option<(usize, (u8, u8, u8), Option<Arc<RenderedImage>>)>>,
}

impl ImageState {
    /// Wrap an attached image.
    #[must_use]
    pub fn new(block: ImageBlock) -> Arc<Self> {
        let pixels = header_pixels(&block);
        Arc::new(Self {
            block,
            pixels,
            rendered: Mutex::new(None),
        })
    }

    /// The media type, for the caption.
    #[must_use]
    pub fn media_type(&self) -> &str {
        &self.block.media_type
    }

    /// The text the row shows when no picture can be drawn.
    #[must_use]
    pub fn caption(&self) -> String {
        format!("[image: {}]", self.block.media_type)
    }

    /// The cell size this picture would take at `avail_cols`.
    ///
    /// `(0, 0)` means the header could not be read: the picture stays a caption.
    #[must_use]
    pub fn cell_size(&self, avail_cols: usize) -> (usize, usize) {
        self.pixels.map_or((0, 0), |pixels| fit(pixels, avail_cols))
    }

    /// The picture as transcript lines, one per cell row.
    ///
    /// Returns `None` when nothing can be drawn — no header, too many pixels, or
    /// a decode failure — and the caller keeps the caption.
    #[must_use]
    pub fn lines(&self, avail_cols: usize, theme: &Theme) -> Option<Vec<Line<'static>>> {
        let rendered = self.rendered_at(avail_cols, theme)?;
        Some(
            (0..rendered.rows)
                .map(|row| {
                    let mut spans = Vec::with_capacity(rendered.cols);
                    for column in 0..rendered.cols {
                        let (top, bottom) = rendered.cells[row * rendered.cols + column];
                        spans.push(Span::styled(
                            HALF_BLOCK,
                            Style::default().fg(top).bg(bottom),
                        ));
                    }
                    Line::from(spans)
                })
                .collect(),
        )
    }

    /// The rendering for `avail_cols`, decoding it if this is the first draw.
    fn rendered_at(&self, avail_cols: usize, theme: &Theme) -> Option<Arc<RenderedImage>> {
        let background = rgb_of(theme.background);
        if let Ok(cache) = self.rendered.lock()
            && let Some((width, cached_background, rendered)) = cache.as_ref()
            && *width == avail_cols
            && *cached_background == background
        {
            return rendered.as_ref().map(Arc::clone);
        }

        // A failure is cached as well: a truncated payload would otherwise be
        // decoded again on every frame that keeps its row on screen.
        let rendered = self.decode(avail_cols, background).map(Arc::new);
        if let Ok(mut cache) = self.rendered.lock() {
            *cache = Some((avail_cols, background, rendered.as_ref().map(Arc::clone)));
        }
        rendered
    }

    /// Decode the image and scale it to the target cell box.
    fn decode(&self, avail_cols: usize, background: (u8, u8, u8)) -> Option<RenderedImage> {
        let (cols, rows) = self.cell_size(avail_cols);
        if cols == 0 || rows == 0 {
            return None;
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(self.block.data.as_bytes())
            .ok()?;
        let decoded = image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .ok()?
            .decode()
            .ok()?;

        // Two pixels per cell row, so the box is `cols` by `rows * 2` pixels.
        let target_w = cols as u32;
        let target_h = u32::try_from(rows.saturating_mul(2)).ok()?;
        let scaled =
            decoded.resize_exact(target_w, target_h, image::imageops::FilterType::Triangle);
        let rgba = scaled.to_rgba8();

        let mut cells = Vec::with_capacity(cols * rows);
        for row in 0..rows {
            for column in 0..cols {
                let top = rgba.get_pixel(column as u32, (row * 2) as u32);
                let bottom = rgba.get_pixel(column as u32, (row * 2 + 1) as u32);
                cells.push((composite(*top, background), composite(*bottom, background)));
            }
        }
        Some(RenderedImage { cols, rows, cells })
    }
}

/// Blend one RGBA pixel over an opaque background.
fn composite(pixel: image::Rgba<u8>, background: (u8, u8, u8)) -> Color {
    let channels = pixel.0;
    let alpha = u32::from(channels[3]);
    if alpha == 255 {
        return Color::Rgb(channels[0], channels[1], channels[2]);
    }
    let blend = |channel: u8, under: u8| -> u8 {
        let over = u32::from(channel) * alpha;
        let under = u32::from(under) * (255 - alpha);
        u8::try_from((over + under) / 255).unwrap_or(u8::MAX)
    };
    Color::Rgb(
        blend(channels[0], background.0),
        blend(channels[1], background.1),
        blend(channels[2], background.2),
    )
}

/// A theme colour as an opaque background triple.
const fn rgb_of(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0, 0, 0),
    }
}

/// The pixel size from an image header, without decoding the pixels.
///
/// The base64 payload still has to be decoded to reach the header, but the
/// picture itself is not: `into_dimensions` stops after the few hundred bytes a
/// header needs. That is what lets the layout reserve rows on the frame that
/// adds the message, before anything is drawn. `None` means the format or the
/// header could not be read, or the image is larger than [`MAX_DECODE_PIXELS`].
fn header_pixels(block: &ImageBlock) -> Option<(u32, u32)> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(block.data.as_bytes())
        .ok()?;
    let (width, height) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_DECODE_PIXELS {
        return None;
    }
    Some((width, height))
}

/// Fit `pixels` into `avail_cols` by [`MAX_IMAGE_ROWS`], preserving aspect.
///
/// A cell is about twice as tall as it is wide and carries two pixels
/// vertically, so a `cols`-by-`rows` box is a roughly square pixel grid. The
/// picture is never enlarged past its own pixel size.
#[must_use]
fn fit((width, height): (u32, u32), avail_cols: usize) -> (usize, usize) {
    if width == 0 || height == 0 || avail_cols == 0 {
        return (0, 0);
    }
    #[allow(clippy::cast_precision_loss)]
    let image_w = f64::from(width);
    #[allow(clippy::cast_precision_loss)]
    let image_h = f64::from(height);
    #[allow(clippy::cast_precision_loss)]
    let max_w = avail_cols as f64;
    #[allow(clippy::cast_precision_loss)]
    let max_h = (MAX_IMAGE_ROWS * 2) as f64;

    let scale = (max_w / image_w).min(max_h / image_h).min(1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cols = ((image_w * scale).round() as usize).max(1).min(avail_cols);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pixel_rows = ((image_h * scale).round() as usize).max(1);
    // Two pixels to a row, rounded up so no pixel is dropped.
    let rows = pixel_rows.div_ceil(2).clamp(1, MAX_IMAGE_ROWS);
    (cols, rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(pairs: &'static [(&'static str, &'static str)]) -> TerminalProfile {
        TerminalProfile::detect(&move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        })
    }

    fn detect(pairs: &'static [(&'static str, &'static str)]) -> ImageProtocol {
        ImageProtocol::detect(&profile(pairs), &move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        })
    }

    #[test]
    fn known_24_bit_terminals_get_pictures() {
        assert_eq!(
            detect(&[("KITTY_WINDOW_ID", "1")]),
            ImageProtocol::HalfBlocks
        );
        assert_eq!(
            detect(&[("TERM_PROGRAM", "WezTerm")]),
            ImageProtocol::HalfBlocks
        );
        assert_eq!(
            detect(&[("TERM_PROGRAM", "iTerm.app")]),
            ImageProtocol::HalfBlocks
        );
        assert_eq!(
            detect(&[("TERM_PROGRAM", "vscode")]),
            ImageProtocol::HalfBlocks
        );
    }

    #[test]
    fn terminal_app_and_unknown_stay_text() {
        // Terminal.app never gained 24-bit SGR: a half block there would land
        // in the default colour and read as a broken picture.
        assert_eq!(
            detect(&[("TERM_PROGRAM", "Apple_Terminal")]),
            ImageProtocol::None
        );
        assert_eq!(detect(&[]), ImageProtocol::None);
    }

    #[test]
    fn colorterm_is_enough_on_its_own() {
        assert_eq!(
            detect(&[("COLORTERM", "truecolor")]),
            ImageProtocol::HalfBlocks
        );
        assert_eq!(detect(&[("COLORTERM", "24bit")]), ImageProtocol::HalfBlocks);
        assert_eq!(detect(&[("COLORTERM", "yes")]), ImageProtocol::None);
    }

    #[test]
    fn the_override_wins_both_ways() {
        assert_eq!(
            detect(&[
                ("TERM_PROGRAM", "Apple_Terminal"),
                (IMAGE_PROTOCOL_ENV, "blocks")
            ]),
            ImageProtocol::HalfBlocks
        );
        assert_eq!(
            detect(&[("KITTY_WINDOW_ID", "1"), (IMAGE_PROTOCOL_ENV, "off")]),
            ImageProtocol::None
        );
        // An unrecognised override falls through to detection rather than
        // silently disabling pictures.
        assert_eq!(
            detect(&[("KITTY_WINDOW_ID", "1"), (IMAGE_PROTOCOL_ENV, "maybe")]),
            ImageProtocol::HalfBlocks
        );
    }

    #[test]
    fn fit_never_enlarges() {
        // A 40x30 image in a wide pane keeps its own pixel size.
        assert_eq!(fit((40, 30), 200), (40, 15));
    }

    #[test]
    fn fit_respects_the_row_ceiling() {
        // A tall screenshot is scaled down until it fits both bounds.
        let (cols, rows) = fit((1000, 4000), 80);
        assert!(rows <= MAX_IMAGE_ROWS, "rows {rows}");
        assert!(cols <= 80, "cols {cols}");
        // Aspect ratio survives the clamp: 1000:4000 means one cell across per
        // four pixel rows, i.e. two rows per column.
        assert!(cols * 2 >= rows * 2 / 2, "cols {cols} rows {rows}");
    }

    #[test]
    fn fit_a_zero_sized_pane_draws_nothing() {
        assert_eq!(fit((10, 10), 0), (0, 0));
    }

    #[test]
    fn alpha_is_composited_over_the_background() {
        // A fully transparent pixel takes the background.
        assert_eq!(
            composite(image::Rgba([255, 0, 0, 0]), (10, 20, 30)),
            Color::Rgb(10, 20, 30)
        );
        // A fully opaque one keeps its own colour.
        assert_eq!(
            composite(image::Rgba([255, 0, 0, 255]), (10, 20, 30)),
            Color::Rgb(255, 0, 0)
        );
        // Half alpha lands between the two.
        let Color::Rgb(r, g, b) = composite(image::Rgba([255, 255, 255, 128]), (0, 0, 0)) else {
            panic!("expected an rgb colour");
        };
        assert!((120..=140).contains(&r), "{r}");
        assert_eq!((g, b), (r, r));
    }

    /// A 2x2 PNG: red and green on top, blue and white below.
    fn two_by_two_png() -> String {
        use base64::Engine as _;
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let encoder = image::codecs::png::PngEncoder::new(&mut buffer);
            let pixels: [u8; 4 * 4] = [
                255, 0, 0, 255, // red
                0, 255, 0, 255, // green
                0, 0, 255, 255, // blue
                255, 255, 255, 255,
            ];
            image::ImageEncoder::write_image(
                encoder,
                &pixels,
                2,
                2,
                image::ExtendedColorType::Rgba8,
            )
            .expect("encode png");
        }
        base64::engine::general_purpose::STANDARD.encode(buffer.into_inner())
    }

    fn state() -> Arc<ImageState> {
        ImageState::new(ImageBlock {
            media_type: "image/png".into(),
            data: two_by_two_png(),
        })
    }

    #[test]
    fn the_header_gives_the_cell_size_without_decoding_pixels() {
        let image = state();
        // Two pixels across is two cells; two pixels down is one cell row.
        assert_eq!(image.cell_size(80), (2, 1));
    }

    #[test]
    fn lines_carry_the_pixels_as_half_blocks() {
        let image = state();
        let lines = image
            .lines(80, &Theme::dark())
            .expect("a two by two png renders");
        assert_eq!(lines.len(), 1);
        let spans = &lines[0].spans;
        assert_eq!(spans.len(), 2);
        // First cell: red on top, blue below.
        assert_eq!(spans[0].style.fg, Some(Color::Rgb(255, 0, 0)));
        assert_eq!(spans[0].style.bg, Some(Color::Rgb(0, 0, 255)));
        // Second cell: green on top, white below.
        assert_eq!(spans[1].style.fg, Some(Color::Rgb(0, 255, 0)));
        assert_eq!(spans[1].style.bg, Some(Color::Rgb(255, 255, 255)));
        for span in spans {
            assert_eq!(span.content.as_ref(), HALF_BLOCK);
        }
    }

    #[test]
    fn a_picture_wider_than_the_pane_is_scaled_to_fit() {
        let image = state();
        // One column is not enough for two pixels wide, so it scales down
        // rather than cropping.
        let (cols, rows) = image.cell_size(1);
        assert_eq!(cols, 1);
        assert_eq!(rows, 1);
        let lines = image.lines(1, &Theme::dark()).expect("still renders");
        assert_eq!(lines[0].spans.len(), 1);
    }

    #[test]
    fn unreadable_bytes_leave_the_caption() {
        let image = ImageState::new(ImageBlock {
            media_type: "image/png".into(),
            data: "not base64 at all".into(),
        });
        assert_eq!(image.cell_size(80), (0, 0));
        assert!(image.lines(80, &Theme::dark()).is_none());
        assert_eq!(image.caption(), "[image: image/png]");
        // The failed decode is remembered rather than retried every frame.
        assert!(image.rendered.lock().unwrap().is_some());
    }

    #[test]
    fn the_second_draw_reuses_the_first_rendering() {
        let image = state();
        let first = image.lines(80, &Theme::dark()).expect("renders");
        let second = image.lines(80, &Theme::dark()).expect("renders again");
        assert_eq!(first, second);
        assert!(image.rendered.lock().unwrap().is_some());
    }

    #[test]
    fn a_transparent_pixel_takes_the_theme_background() {
        // Half-block cells cannot be transparent: a transparent pixel would
        // show whatever the terminal had there, and the two rows of a cell
        // would disagree about their backdrop. So alpha is composited here,
        // which means the rendering depends on the theme background and a
        // theme switch has to invalidate it.
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let encoder = image::codecs::png::PngEncoder::new(&mut buffer);
            let pixels: [u8; 16] = [255, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            image::ImageEncoder::write_image(
                encoder,
                &pixels,
                2,
                2,
                image::ExtendedColorType::Rgba8,
            )
            .expect("encode png");
        }
        let image = ImageState::new(ImageBlock {
            media_type: "image/png".into(),
            data: base64::engine::general_purpose::STANDARD.encode(buffer.into_inner()),
        });

        let dark = image.lines(80, &Theme::dark()).expect("renders");
        // The cell under the opaque pixel keeps its colour either way.
        assert_eq!(dark[0].spans[0].style.fg, Some(Color::Rgb(255, 0, 0)));
        // The transparent bottom pixel takes the dark theme's background.
        assert_eq!(dark[0].spans[0].style.bg, Some(Theme::dark().background));

        let light = image.lines(80, &Theme::light()).expect("renders again");
        assert_eq!(light[0].spans[0].style.fg, Some(Color::Rgb(255, 0, 0)));
        assert_eq!(
            light[0].spans[0].style.bg,
            Some(Theme::light().background),
            "the cached rendering was reused across a background change"
        );
    }
}
