//! Turning a picture into something the model can see.
//!
//! A person points at a screenshot — `--image shot.png`, `@shot.png`, or a
//! paste — and the turn carries it as an image block. The bytes are read here
//! and base64-encoded once, so every surface that can attach an image shares
//! the same size limit, the same format list and the same error text.
//!
//! Only the formats a provider accepts as an image source are forwarded. An
//! SVG or an ICO has no usable media type there, and base64 text in the prompt
//! would be charged by the token and read by nothing, so those are refused
//! with a message that says so rather than silently attached.

use std::path::Path;

use codape_core::types::ImageBlock;

/// Image extensions that can be attached to a turn.
///
/// Deliberately narrower than the Read tool's list: that one also names SVG,
/// BMP and ICO so it can at least show the person what it found, while these
/// are the four formats a provider takes as an image block.
pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// Largest image that will be attached.
///
/// The same ceiling the Read tool puts on a forwarded image: the provider
/// rejects anything larger, and an oversized attachment would fail the whole
/// turn rather than one tool call.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// The MIME type for an image path, when its extension names a supported one.
#[must_use]
pub fn media_type_for(path: &Path) -> Option<&'static str> {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())?
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Whether this path names an image type a turn can carry.
#[must_use]
pub fn is_image_path(path: &Path) -> bool {
    media_type_for(path).is_some()
}

/// Read an image file into a block ready to attach.
pub fn load_image(path: &Path) -> Result<ImageBlock, String> {
    let media_type = media_type_for(path).ok_or_else(|| {
        format!(
            "{}: not an image the model accepts (supported: {})",
            path.display(),
            IMAGE_EXTENSIONS.join(", ")
        )
    })?;
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    encode(&bytes, media_type, &path.display().to_string())
}

/// Base64-encode bytes as an image block, enforcing the size limit.
fn encode(bytes: &[u8], media_type: &str, what: &str) -> Result<ImageBlock, String> {
    use base64::Engine as _;
    if bytes.is_empty() {
        return Err(format!("{what}: the image is empty"));
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "{what}: {} MiB exceeds the {} MiB image limit",
            bytes.len() / (1024 * 1024),
            MAX_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    Ok(ImageBlock {
        media_type: media_type.to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

/// Take an image off the system clipboard.
///
/// macOS reads it through `osascript`, which is always present; Linux and
/// Windows need a tool that may not be installed. Every failure names what was
/// missing, because the alternative — a paste that does nothing — looks like
/// the program is broken rather than like the clipboard holding no image.
pub fn read_clipboard_image() -> Result<ImageBlock, String> {
    let (bytes, media_type) = read_clipboard_bytes()?;
    encode(&bytes, media_type, "clipboard")
}

#[cfg(target_os = "macos")]
fn read_clipboard_bytes() -> Result<(Vec<u8>, &'static str), String> {
    // `osascript` is part of macOS, so there is no extra dependency to
    // install. The image is written to a private temp file and read back:
    // AppleScript cannot hand binary data through stdout reliably.
    let dir = tempfile::tempdir().map_err(|error| format!("clipboard temp dir: {error}"))?;
    let path = dir.path().join("clipboard.png");
    let script = format!(
        r#"try
    set imageData to the clipboard as «class PNGf»
on error
    return "no-image"
end try
set fileHandle to open for access POSIX file "{}" with write permission
set eof fileHandle to 0
write imageData to fileHandle
close access fileHandle
return "ok""#,
        path.display()
    );
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .map_err(|error| format!("osascript: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "osascript: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if String::from_utf8_lossy(&output.stdout).trim() != "ok" {
        return Err("the clipboard holds no image".to_string());
    }
    let bytes = std::fs::read(&path).map_err(|error| format!("clipboard image: {error}"))?;
    Ok((bytes, "image/png"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn read_clipboard_bytes() -> Result<(Vec<u8>, &'static str), String> {
    use std::process::{Command, Stdio};

    let candidates: [(&str, &[&str]); 2] = [
        ("wl-paste", &["--no-newline", "--type", "image/png"]),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
    ];
    let mut last_error = String::from("no clipboard tool found (install wl-clipboard or xclip)");
    for (program, args) in candidates {
        let spawned = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output();
        match spawned {
            Ok(output) if output.status.success() && !output.stdout.is_empty() => {
                return Ok((output.stdout, "image/png"));
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                last_error = if stderr.trim().is_empty() {
                    format!("{program}: the clipboard holds no image")
                } else {
                    format!("{program}: {}", stderr.trim())
                };
            }
            Err(error) => last_error = format!("{program}: {error}"),
        }
    }
    Err(last_error)
}

#[cfg(windows)]
fn read_clipboard_bytes() -> Result<(Vec<u8>, &'static str), String> {
    use std::process::Command;

    // PowerShell is the only clipboard reader guaranteed on Windows. The
    // image is saved as PNG because that is what the provider wants, and the
    // exit code alone cannot say whether the clipboard held an image — the
    // empty-file check below does.
    let dir = tempfile::tempdir().map_err(|error| format!("clipboard temp dir: {error}"))?;
    let path = dir.path().join("clipboard.png");
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms; \
         $img = [System.Windows.Forms.Clipboard]::GetImage(); \
         if ($img -eq $null) {{ exit 1 }}; \
         $img.Save('{}', [System.Drawing.Imaging.ImageFormat]::Png)",
        path.display()
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .map_err(|error| format!("powershell: {error}"))?;
    if !output.status.success() {
        return Err("the clipboard holds no image".to_string());
    }
    let bytes = std::fs::read(&path).map_err(|error| format!("clipboard image: {error}"))?;
    Ok((bytes, "image/png"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_png(dir: &Path) -> std::path::PathBuf {
        // One-pixel PNG; the loader never decodes, so only the bytes matter.
        let path = dir.join("shot.png");
        std::fs::write(
            &path,
            [
                0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, b'I', b'H', b'D',
                b'R',
            ],
        )
        .expect("write png");
        path
    }

    #[test]
    fn a_png_loads_as_an_image_block() {
        let dir = tempfile::tempdir().expect("temp");
        let path = write_png(dir.path());
        let image = load_image(&path).expect("load");
        assert_eq!(image.media_type, "image/png");
        assert!(!image.data.is_empty());
    }

    #[test]
    fn a_text_file_is_refused_with_its_path() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "hello").expect("write");
        let error = load_image(&path).expect_err("not an image");
        assert!(error.contains("notes.txt"), "{error}");
    }

    #[test]
    fn an_oversized_image_is_refused_rather_than_sent() {
        let dir = tempfile::tempdir().expect("temp");
        let path = write_png(dir.path());
        let bytes = vec![0u8; MAX_IMAGE_BYTES + 1];
        let error =
            encode(&bytes, "image/png", &path.display().to_string()).expect_err("too large");
        assert!(error.contains("exceeds"), "{error}");
    }

    #[test]
    fn an_empty_image_is_refused() {
        assert!(encode(&[], "image/png", "clip").is_err());
    }

    #[test]
    fn extensions_are_matched_case_insensitively() {
        assert!(is_image_path(Path::new("A.PNG")));
        assert!(is_image_path(Path::new("photo.JPEG")));
        assert!(!is_image_path(Path::new("notes.txt")));
        // The Read tool can display these, but a provider cannot accept them.
        assert!(!is_image_path(Path::new("icon.svg")));
    }
}
