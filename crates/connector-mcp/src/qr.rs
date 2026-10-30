//! QR rendering for the pairing payload, in two forms delivered together:
//!   * a Unicode half-block string for terminal agents (Claude Code, …), and
//!   * a base64 PNG for agents with a graphical UI (Claude Desktop, Antigravity).
//!
//! Both are produced from the same QR matrix (`radixdlt_connect_types::qr`, the encoder
//! every pairing program shares), so they encode exactly the same payload. The raw payload
//! string is also returned to the caller so a user can regenerate the QR with a **local**
//! tool if neither rendering scans.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};
use radixdlt_connect_types::qr::{QrMatrix, QUIET_ZONE};

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
    let matrix = QrMatrix::encode(payload).map_err(|e| e.to_string())?;
    Ok(RenderedQr {
        unicode: matrix.to_unicode(),
        png_base64: to_png_base64(&matrix)?,
    })
}

/// Scale, in pixels, of each QR module in the PNG.
const SCALE: usize = 8;

/// Renders the matrix as a grayscale PNG (black modules on white) and returns it
/// base64-encoded.
fn to_png_base64(matrix: &QrMatrix) -> Result<String, String> {
    let width = matrix.width();
    let side = (width + QUIET_ZONE * 2) * SCALE;
    let mut pixels = vec![255u8; side * side]; // white background

    for my in 0..width {
        for mx in 0..width {
            if !matrix.is_dark(mx, my) {
                continue;
            }
            let px0 = (mx + QUIET_ZONE) * SCALE;
            let py0 = (my + QUIET_ZONE) * SCALE;
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
