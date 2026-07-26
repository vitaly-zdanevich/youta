//! Backend-independent waveform peak storage.
//!
//! The representation deliberately contains no decoder or terminal types. It
//! can therefore become a separate crate later without changing the playback
//! or TUI interfaces.

/// Minimum and maximum signed sample values for one time bucket.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Peak {
    /// Lowest sample in the bucket.
    pub minimum: i16,
    /// Highest sample in the bucket.
    pub maximum: i16,
}

impl Peak {
    fn merge(self, other: Self) -> Self {
        Self {
            minimum: self.minimum.min(other.minimum),
            maximum: self.maximum.max(other.maximum),
        }
    }
}

/// One resolution in a multiresolution waveform pyramid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeakLevel {
    /// Number of source frames represented by each peak.
    pub frames_per_peak: usize,
    /// Ordered peaks spanning the source.
    pub peaks: Vec<Peak>,
}

/// Multiresolution min/max peaks for fast terminal rendering.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeakPyramid {
    levels: Vec<PeakLevel>,
    total_frames: usize,
}

impl PeakPyramid {
    /// Builds a peak pyramid from interleaved signed 16-bit PCM samples.
    ///
    /// Channels are mixed by extrema rather than averaged so short transients
    /// remain visible. `base_frames_per_peak` controls the finest retained
    /// resolution.
    #[must_use]
    pub fn from_interleaved_i16(
        samples: &[i16],
        channels: usize,
        base_frames_per_peak: usize,
    ) -> Self {
        if channels == 0 || base_frames_per_peak == 0 {
            return Self::default();
        }

        let total_frames = samples.len() / channels;
        let mut finest = Vec::with_capacity(total_frames.div_ceil(base_frames_per_peak));
        for frame_bucket in samples
            .chunks_exact(channels)
            .collect::<Vec<_>>()
            .chunks(base_frames_per_peak)
        {
            let mut peak = Peak {
                minimum: i16::MAX,
                maximum: i16::MIN,
            };
            for frame in frame_bucket {
                for sample in *frame {
                    peak.minimum = peak.minimum.min(*sample);
                    peak.maximum = peak.maximum.max(*sample);
                }
            }
            finest.push(peak);
        }

        let mut levels = Vec::new();
        let mut frames_per_peak = base_frames_per_peak;
        let mut current = finest;
        loop {
            levels.push(PeakLevel {
                frames_per_peak,
                peaks: current.clone(),
            });
            if current.len() <= 1 {
                break;
            }
            current = current
                .chunks(2)
                .map(|pair| pair.get(1).map_or(pair[0], |second| pair[0].merge(*second)))
                .collect();
            frames_per_peak = frames_per_peak.saturating_mul(2);
        }

        Self {
            levels,
            total_frames,
        }
    }

    /// Returns the total number of decoded source frames.
    #[must_use]
    pub const fn total_frames(&self) -> usize {
        self.total_frames
    }

    /// Returns every available resolution, finest first.
    #[must_use]
    pub fn levels(&self) -> &[PeakLevel] {
        &self.levels
    }

    /// Chooses the finest level that renders at most roughly twice the requested
    /// terminal width.
    #[must_use]
    pub fn level_for_width(&self, columns: usize) -> Option<&PeakLevel> {
        if columns == 0 {
            return None;
        }
        self.levels
            .iter()
            .find(|level| level.peaks.len() <= columns.saturating_mul(2))
            .or_else(|| self.levels.last())
    }

    /// Maps a terminal column to a source frame, clamped to the media bounds.
    #[must_use]
    pub fn frame_for_column(&self, column: usize, columns: usize) -> usize {
        if self.total_frames == 0 || columns == 0 {
            return 0;
        }
        column
            .min(columns)
            .saturating_mul(self.total_frames)
            .checked_div(columns)
            .unwrap_or_default()
            .min(self.total_frames.saturating_sub(1))
    }
}

/// User-selected, half-open waveform range used for lossless cut requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaveformSelection {
    /// First selected source frame.
    pub start_frame: usize,
    /// Frame immediately after the selection.
    pub end_frame: usize,
}

impl WaveformSelection {
    /// Constructs a normalized range regardless of drag direction.
    #[must_use]
    pub fn normalized(first: usize, second: usize) -> Self {
        Self {
            start_frame: first.min(second),
            end_frame: first.max(second),
        }
    }

    /// Returns whether the selection contains at least one frame.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start_frame == self.end_frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pyramid_preserves_channel_extrema() {
        let samples = [
            -10, 20, // frame 0
            -30, 5, // frame 1
            7, 40, // frame 2
            -2, 8, // frame 3
        ];
        let pyramid = PeakPyramid::from_interleaved_i16(&samples, 2, 2);

        assert_eq!(
            pyramid.levels()[0].peaks,
            [
                Peak {
                    minimum: -30,
                    maximum: 20,
                },
                Peak {
                    minimum: -2,
                    maximum: 40,
                },
            ]
        );
        assert_eq!(
            pyramid.levels()[1].peaks,
            [Peak {
                minimum: -30,
                maximum: 40,
            }]
        );
    }

    #[test]
    fn invalid_shape_returns_empty_pyramid() {
        assert_eq!(
            PeakPyramid::from_interleaved_i16(&[1, 2], 0, 2),
            PeakPyramid::default()
        );
        assert_eq!(
            PeakPyramid::from_interleaved_i16(&[1, 2], 1, 0),
            PeakPyramid::default()
        );
    }

    #[test]
    fn terminal_column_maps_to_source_frame() {
        let samples = (0_i16..100).collect::<Vec<_>>();
        let pyramid = PeakPyramid::from_interleaved_i16(&samples, 1, 10);

        assert_eq!(pyramid.frame_for_column(0, 10), 0);
        assert_eq!(pyramid.frame_for_column(5, 10), 50);
        assert_eq!(pyramid.frame_for_column(10, 10), 99);
    }

    #[test]
    fn selection_is_normalized_after_reverse_drag() {
        let selection = WaveformSelection::normalized(80, 20);
        assert_eq!(selection.start_frame, 20);
        assert_eq!(selection.end_frame, 80);
        assert!(!selection.is_empty());
    }
}
