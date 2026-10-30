//! The linking QR code as a matrix of modules, and its rendering for a terminal.
//!
//! Behind the `qr` feature. Every program that pairs a wallet has to SHOW the payload of
//! [`crate::linking::pairing_qr`] somehow; this is the one encoder they share, so a terminal
//! rendering and an image built from [`QrMatrix::is_dark`] always encode the same bytes.

use qrcode::{Color, QrCode};
use radixdlt_i18n::{tr, Lang};

/// Light border, in modules, that the QR specification requires around the code for scanners
/// to find it. [`QrMatrix::to_unicode`] includes it; an image renderer must add it too.
pub const QUIET_ZONE: usize = 4;

/// The payload could not be encoded as a QR code (it is too long). `Display` is localized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrError(String);

impl std::fmt::Display for QrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lang = Lang::detect();
        let e = &self.0;
        f.write_str(&tr!(
            lang,
            format!("could not build the QR code: {e}"),
            format!("no se pudo construir el código QR: {e}")
        ))
    }
}

impl std::error::Error for QrError {}

/// A QR code: a square of `width × width` modules, each dark or light. No quiet zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrMatrix {
    width: usize,
    dark: Vec<bool>,
}

impl QrMatrix {
    /// Encodes `payload` (as bytes) with the smallest QR version that holds it.
    ///
    /// # Errors
    /// [`QrError`] when the payload does not fit in any QR version.
    pub fn encode(payload: &str) -> Result<Self, QrError> {
        let code = QrCode::new(payload.as_bytes()).map_err(|e| QrError(e.to_string()))?;
        Ok(QrMatrix {
            width: code.width(),
            dark: code.to_colors().into_iter().map(|c| c == Color::Dark).collect(),
        })
    }

    /// Modules per side.
    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    /// Whether the module at column `x`, row `y` is dark. Anything outside the code is light,
    /// which is exactly what the quiet zone around it must be.
    #[must_use]
    pub fn is_dark(&self, x: usize, y: usize) -> bool {
        x < self.width
            && y < self.width
            && self
                .dark
                .get(y.saturating_mul(self.width).saturating_add(x))
                .copied()
                .unwrap_or(false)
    }

    /// The code as Unicode half blocks, with its quiet zone: each character stacks two modules
    /// vertically, so it stays compact. Dark modules are filled glyphs on the terminal's
    /// background, so a terminal with a dark theme shows the code inverted; if a phone does not
    /// read it, render [`is_dark`](Self::is_dark) as an image instead.
    #[must_use]
    pub fn to_unicode(&self) -> String {
        let size = self.width + QUIET_ZONE * 2;
        // In terminal coordinates, shifted by the quiet zone.
        let dark = |x: usize, y: usize| {
            x >= QUIET_ZONE && y >= QUIET_ZONE && self.is_dark(x - QUIET_ZONE, y - QUIET_ZONE)
        };
        let mut out = String::with_capacity(size * (size / 2 + 1) * 3);
        for y in (0..size).step_by(2) {
            for x in 0..size {
                out.push(match (dark(x, y), dark(x, y + 1)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                });
            }
            out.push('\n');
        }
        out
    }
}

/// [`QrMatrix::encode`] then [`QrMatrix::to_unicode`].
///
/// # Errors
/// As [`QrMatrix::encode`].
pub fn render_unicode(payload: &str) -> Result<String, QrError> {
    Ok(QrMatrix::encode(payload)?.to_unicode())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminal_rendering_has_the_quiet_zone_and_both_halves() {
        let matrix = QrMatrix::encode("radix-connect-pairing-payload-example").unwrap();
        let art = matrix.to_unicode();
        let rows: Vec<&str> = art.lines().collect();
        let size = matrix.width() + QUIET_ZONE * 2;
        assert_eq!(rows.len(), size.div_ceil(2));
        assert!(rows.iter().all(|row| row.chars().count() == size));
        // The quiet zone is light all around.
        assert!(rows[0].chars().all(|c| c == ' '));
        assert!(rows
            .iter()
            .all(|row| row.starts_with("    ") && row.ends_with("    ")));
        assert!(art.contains('█') && art.contains('▀') && art.contains('▄'));
    }

    #[test]
    fn modules_outside_the_code_are_light() {
        let matrix = QrMatrix::encode("x").unwrap();
        // Finder pattern corner is dark; beyond the edge is the quiet zone.
        assert!(matrix.is_dark(0, 0));
        assert!(!matrix.is_dark(matrix.width(), 0));
        assert!(!matrix.is_dark(0, usize::MAX));
    }

    #[test]
    fn a_payload_too_long_for_any_qr_is_an_error() {
        assert!(render_unicode(&"x".repeat(8000)).is_err());
    }
}
