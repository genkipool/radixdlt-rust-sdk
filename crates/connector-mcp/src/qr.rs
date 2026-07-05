//! QR rendering for the pairing payload, in two forms delivered together:
//!   * a Unicode half-block string for terminal agents (Claude Code, …), and
//!   * a base64 PNG for agents with a graphical UI (Claude Desktop, Antigravity).
//!
//! Both are produced from the same QR matrix, so they encode exactly the same
//! payload. The raw payload string is also returned to the caller so a user can
//! regenerate the QR with a **local** tool if neither rendering scans.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};
use qrcode::{Color, QrCode};

/// A rendered QR in both forms.
pub struct RenderedQr {
    /// Unicode half-block art (with a light quiet zone) for terminals.
    pub unicode: String,
    /// Base64-encoded PNG (no data URI prefix) for image content blocks.
    pub png_base64: String,
}

/// Builds a QR from `payload` and renders both forms. Fails only if the payload
/// is too large to encode as a QR code.
pub fn render(payload: &str) -> Result<RenderedQr, String> {
    let code =
        QrCode::new(payload.as_bytes()).map_err(|e| format!("could not build the QR code: {e}"))?;
    let width = code.width();
    let modules: Vec<bool> = code
        .to_colors()
        .into_iter()
        .map(|c| c == Color::Dark)
        .collect();
    Ok(RenderedQr {
        unicode: to_unicode(width, &modules),
        png_base64: to_png_base64(width, &modules)?,
    })
}

/// Quiet zone (light border) in modules, as required by the QR spec for scanning.
const QUIET: usize = 4;

/// Renders the matrix as Unicode half-blocks: each character stacks two vertical
/// modules, so the art is compact. Dark modules are filled glyphs on the terminal
/// background.
fn to_unicode(width: usize, modules: &[bool]) -> String {
    let size = width + QUIET * 2;
    let dark = |x: usize, y: usize| -> bool {
        if x < QUIET || y < QUIET || x >= QUIET + width || y >= QUIET + width {
            false // quiet zone
        } else {
            modules[(y - QUIET) * width + (x - QUIET)]
        }
    };

    let mut out = String::with_capacity(size * (size / 2 + 1));
    let mut y = 0;
    while y < size {
        for x in 0..size {
            let top = dark(x, y);
            let bottom = y + 1 < size && dark(x, y + 1);
            out.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push('\n');
        y += 2;
    }
    out
}

/// Scale, in pixels, of each QR module in the PNG.
const SCALE: usize = 8;

/// Renders the matrix as a grayscale PNG (black modules on white) and returns it
/// base64-encoded.
fn to_png_base64(width: usize, modules: &[bool]) -> Result<String, String> {
    let side = (width + QUIET * 2) * SCALE;
    let mut pixels = vec![255u8; side * side]; // white background

    for my in 0..width {
        for mx in 0..width {
            if !modules[my * width + mx] {
                continue;
            }
            let px0 = (mx + QUIET) * SCALE;
            let py0 = (my + QUIET) * SCALE;
            for dy in 0..SCALE {
                let row = (py0 + dy) * side;
                for dx in 0..SCALE {
                    pixels[row + px0 + dx] = 0; // black module
                }
            }
        }
    }

    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(&pixels, side as u32, side as u32, ExtendedColorType::L8)
        .map_err(|e| format!("could not encode the QR PNG: {e}"))?;
    Ok(B64.encode(&png))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_both_forms() {
        let out = render("radix-connect-pairing-payload-example").unwrap();
        assert!(out.unicode.contains('█') || out.unicode.contains('▀'));
        assert!(!out.png_base64.is_empty());
        // The PNG magic bytes survive the base64 round-trip.
        let bytes = B64.decode(out.png_base64).unwrap();
        assert_eq!(&bytes[1..4], b"PNG");
    }
}
