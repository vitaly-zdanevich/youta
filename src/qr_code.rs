//! QR-code encoding independent of terminal rendering.
//!
//! The terminal UI owns layout, quiet-zone, and color decisions. This module
//! retains only a compact row-major matrix so alternate renderers can consume
//! the same encoded data without depending on the encoder's rendering APIs.

use qrcode::{Color, QrCode};
use serde::Serialize;
use thiserror::Error;

/// A square QR-code module matrix stored in row-major order.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct QrMatrix {
    width: usize,
    modules: Vec<bool>,
}

impl QrMatrix {
    /// Encodes `payload` as a QR-code module matrix.
    ///
    /// The returned matrix excludes the mandatory quiet zone. Renderers must
    /// surround it with at least four light modules on every side.
    ///
    /// # Errors
    ///
    /// Returns [`QrMatrixError`] when `payload` exceeds the QR format's
    /// bounded storage capacity.
    pub fn encode(payload: &str) -> Result<Self, QrMatrixError> {
        let code = QrCode::new(payload.as_bytes()).map_err(|error| QrMatrixError {
            message: error.to_string(),
        })?;
        let width = code.width();
        let modules = code
            .into_colors()
            .into_iter()
            .map(|color| color == Color::Dark)
            .collect();

        Ok(Self { width, modules })
    }

    /// Returns the matrix width in QR modules.
    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    /// Returns whether the module at `(x, y)` is dark.
    ///
    /// # Panics
    ///
    /// Panics when either coordinate lies outside [`Self::width`]. Renderers
    /// normally iterate over that known square boundary.
    #[must_use]
    pub fn is_dark(&self, x: usize, y: usize) -> bool {
        assert!(
            x < self.width && y < self.width,
            "QR module coordinates ({x}, {y}) exceed matrix width {}",
            self.width
        );
        self.modules[y * self.width + x]
    }
}

/// An error returned when a payload cannot fit in a QR code.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("could not encode QR code: {message}")]
pub struct QrMatrixError {
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const YOUTUBE_URL: &str = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";

    #[test]
    fn encodes_a_canonical_youtube_url_as_a_square_matrix() {
        let matrix = QrMatrix::encode(YOUTUBE_URL).expect("canonical URL should fit");

        assert!(matrix.width() >= 21);
        assert_eq!(matrix.width() % 4, 1, "QR widths follow 21 + 4n");
        assert_eq!(matrix.modules.len(), matrix.width() * matrix.width());

        // Finder-pattern samples prove that coordinates use the expected
        // top-left origin and row-major orientation.
        assert!(matrix.is_dark(0, 0));
        assert!(!matrix.is_dark(1, 1));
        assert!(matrix.is_dark(3, 3));
        assert!(matrix.is_dark(matrix.width() - 1, 0));
        assert!(matrix.is_dark(0, matrix.width() - 1));
    }

    #[test]
    fn rejects_payloads_larger_than_a_qr_code_can_store() {
        let oversized = "x".repeat(5_000);

        let error = QrMatrix::encode(&oversized).expect_err("payload should exceed QR capacity");

        assert!(error.to_string().starts_with("could not encode QR code:"));
    }

    #[test]
    #[should_panic(expected = "exceed matrix width")]
    fn rejects_out_of_bounds_module_coordinates() {
        let matrix = QrMatrix::encode(YOUTUBE_URL).expect("canonical URL should fit");

        let _ = matrix.is_dark(matrix.width(), 0);
    }
}
