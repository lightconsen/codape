//! An attached picture survives the trip from disk to transcript rows.
//!
//! The two halves are in different crates: `image_input` reads the file and
//! base64-encodes it, `transcript_image` decodes it and turns it into
//! half-block cells. A mismatch between those two — a data-URI prefix on one
//! side, a different base64 alphabet, a guessed media type on the other — makes
//! every picture a caption and nothing else. This drives the real seam.

use base64::Engine as _;
use codape_ui::themes::Theme;
use codape_ui::transcript_image::ImageState;
use ratatui::style::Color;

/// An 8x8 PNG, red on the left half and blue on the right.
fn write_test_png(path: &std::path::Path) {
    let mut pixels = Vec::with_capacity(8 * 8 * 4);
    for _ in 0..8 {
        for x in 0..8 {
            if x < 4 {
                pixels.extend_from_slice(&[255, 0, 0, 255]);
            } else {
                pixels.extend_from_slice(&[0, 0, 255, 255]);
            }
        }
    }
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let encoder = image::codecs::png::PngEncoder::new(&mut buffer);
        image::ImageEncoder::write_image(encoder, &pixels, 8, 8, image::ExtendedColorType::Rgba8)
            .expect("encode png");
    }
    std::fs::write(path, buffer.into_inner()).expect("write png");
}

#[test]
fn a_picture_loaded_from_disk_renders_as_half_blocks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("shot.png");
    write_test_png(&path);

    let block = codape_cli::image_input::load_image(&path).expect("load image");
    assert_eq!(block.media_type, "image/png");
    // The block is base64 with no data-URI prefix; a `data:image/png;base64,`
    // head here would decode to nothing and the caption would be all that is
    // ever drawn.
    assert!(!block.data.starts_with("data:"));
    base64::engine::general_purpose::STANDARD
        .decode(block.data.as_bytes())
        .expect("the payload is plain base64");

    let state = ImageState::new(block);
    assert_eq!(state.caption(), "[image: image/png]");

    // Eight pixels across is eight cells; eight pixels down is four cell rows.
    assert_eq!(state.cell_size(200), (8, 4));

    let lines = state
        .lines(200, &Theme::dark())
        .expect("the picture renders");
    assert_eq!(lines.len(), 4);
    for line in &lines {
        assert_eq!(line.spans.len(), 8);
        for span in &line.spans {
            assert_eq!(span.content.as_ref(), "\u{2580}");
        }
    }
    // Red on the left, blue on the right, top and bottom of every cell.
    let style = lines[0].spans[0].style;
    assert_eq!(style.fg, Some(Color::Rgb(255, 0, 0)));
    assert_eq!(style.bg, Some(Color::Rgb(255, 0, 0)));
    let style = lines[0].spans[7].style;
    assert_eq!(style.fg, Some(Color::Rgb(0, 0, 255)));
    assert_eq!(style.bg, Some(Color::Rgb(0, 0, 255)));
}

#[test]
fn a_narrow_pane_scales_the_picture_instead_of_cropping_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("wide.png");
    write_test_png(&path);

    let state = ImageState::new(codape_cli::image_input::load_image(&path).expect("load image"));
    let (cols, rows) = state.cell_size(3);
    assert_eq!(cols, 3, "the picture was cropped rather than scaled");
    assert!((1..=4).contains(&rows), "rows {rows}");

    let lines = state.lines(3, &Theme::dark()).expect("still renders");
    assert_eq!(lines[0].spans.len(), 3);
}
