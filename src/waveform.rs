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
    /// Builds a peak pyramid from an already reduced finest level.
    ///
    /// `frames_per_peak` is the nominal number of source frames represented by
    /// each supplied peak. The final peak may represent fewer frames;
    /// `total_frames` retains the exact decoded length for timeline mapping.
    /// Empty or internally inconsistent spans return an empty pyramid.
    #[must_use]
    pub fn from_peaks(peaks: Vec<Peak>, frames_per_peak: usize, total_frames: usize) -> Self {
        if peaks.is_empty() || frames_per_peak == 0 || total_frames == 0 {
            return Self::default();
        }
        let minimum_frames = peaks
            .len()
            .saturating_sub(1)
            .saturating_mul(frames_per_peak)
            .saturating_add(1);
        let maximum_frames = peaks.len().saturating_mul(frames_per_peak);
        if !(minimum_frames..=maximum_frames).contains(&total_frames) {
            return Self::default();
        }

        let mut levels = vec![PeakLevel {
            frames_per_peak,
            peaks,
        }];
        while levels.last().is_some_and(|level| level.peaks.len() > 1) {
            let Some(previous) = levels.last() else {
                break;
            };
            let peaks = previous
                .peaks
                .chunks(2)
                .map(|pair| pair.get(1).map_or(pair[0], |second| pair[0].merge(*second)))
                .collect();
            levels.push(PeakLevel {
                frames_per_peak: previous.frames_per_peak.saturating_mul(2),
                peaks,
            });
        }

        Self {
            levels,
            total_frames,
        }
    }

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
        let mut peak = Peak {
            minimum: i16::MAX,
            maximum: i16::MIN,
        };
        let mut bucket_frames = 0;
        for frame in samples.chunks_exact(channels) {
            for sample in frame {
                peak.minimum = peak.minimum.min(*sample);
                peak.maximum = peak.maximum.max(*sample);
            }
            bucket_frames += 1;
            if bucket_frames == base_frames_per_peak {
                finest.push(peak);
                peak = Peak {
                    minimum: i16::MAX,
                    maximum: i16::MIN,
                };
                bucket_frames = 0;
            }
        }
        if bucket_frames > 0 {
            finest.push(peak);
        }

        Self::from_peaks(finest, base_frames_per_peak, total_frames)
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

    /// Estimates heap-backed peak storage retained by this pyramid.
    ///
    /// The value includes spare vector capacity so a cache cannot undercount a
    /// short waveform produced by a builder that reserved a larger peak limit.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.levels
            .iter()
            .map(|level| {
                level
                    .peaks
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Peak>())
            })
            .sum::<usize>()
            .saturating_add(
                self.levels
                    .capacity()
                    .saturating_mul(std::mem::size_of::<PeakLevel>()),
            )
            .saturating_add(std::mem::size_of::<Self>())
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

    /// Returns one time-weighted merged peak per requested terminal column.
    ///
    /// Columns divide the exact decoded frame count rather than the number of
    /// stored peaks. A partial final source bucket therefore occupies only its
    /// proportional timeline width. A stored peak can contribute to adjacent
    /// columns when a column boundary crosses its represented frame span; this
    /// preserves transients without stretching the final bucket.
    ///
    /// An allocation failure returns an empty waveform instead of aborting.
    #[must_use]
    pub fn reduced_for_width(&self, columns: usize) -> Vec<Peak> {
        let Some(level) = self.level_for_width(columns) else {
            return Vec::new();
        };
        let mut reduced = Vec::new();
        if reduced.try_reserve_exact(columns).is_err() {
            return reduced;
        }

        let total_frames = self.total_frames as u128;
        let columns = columns as u128;
        let final_peak_index = level.peaks.len().saturating_sub(1);
        for column in 0..columns {
            let start_frame = column.saturating_mul(total_frames) / columns;
            let end_frame = column
                .saturating_add(1)
                .saturating_mul(total_frames)
                .div_ceil(columns);
            let first_peak = usize::try_from(start_frame / level.frames_per_peak.max(1) as u128)
                .unwrap_or(final_peak_index)
                .min(final_peak_index);
            let end_peak =
                usize::try_from(end_frame.div_ceil(level.frames_per_peak.max(1) as u128))
                    .unwrap_or(level.peaks.len())
                    .clamp(first_peak.saturating_add(1), level.peaks.len());
            reduced.push(
                level.peaks[first_peak..end_peak]
                    .iter()
                    .copied()
                    .reduce(Peak::merge)
                    .unwrap_or_default(),
            );
        }
        reduced
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

/// Streaming, memory-bounded builder for a finest waveform level.
///
/// Callers push equal-size source buckets in chronological order. Whenever
/// the configured peak limit is reached, the builder merges adjacent buckets
/// and doubles the number of source frames represented by future stored
/// peaks. Only a partial final bucket may contain fewer source frames.
#[derive(Debug)]
pub struct PeakPyramidBuilder {
    base_frames_per_peak: usize,
    frames_per_peak: usize,
    maximum_peaks: usize,
    peaks: Vec<Peak>,
    pending_peak: Option<Peak>,
    pending_buckets: usize,
    buckets_per_peak: usize,
    total_frames: usize,
}

impl PeakPyramidBuilder {
    /// Creates a builder retaining no more than `maximum_peaks` plus one
    /// partially accumulated peak.
    ///
    /// A zero frame count, a peak limit below two, or a peak allocation that
    /// cannot be reserved safely returns `None`.
    #[must_use]
    pub fn new(base_frames_per_peak: usize, maximum_peaks: usize) -> Option<Self> {
        if base_frames_per_peak == 0 || maximum_peaks < 2 {
            return None;
        }
        let maximum_peaks = maximum_peaks - (maximum_peaks % 2);
        let mut peaks = Vec::new();
        peaks.try_reserve_exact(maximum_peaks).ok()?;
        Some(Self {
            base_frames_per_peak,
            frames_per_peak: base_frames_per_peak,
            maximum_peaks,
            peaks,
            pending_peak: None,
            pending_buckets: 0,
            buckets_per_peak: 1,
            total_frames: 0,
        })
    }

    /// Adds one chronological peak and its exact represented source frames.
    ///
    /// `represented_frames` may be smaller than the nominal base size only
    /// for the decoder's final bucket. Empty buckets are ignored.
    pub fn push(&mut self, peak: Peak, represented_frames: usize) {
        if represented_frames == 0 {
            return;
        }
        self.total_frames = self.total_frames.saturating_add(represented_frames);

        if self.pending_buckets == 0 && self.peaks.len() == self.maximum_peaks {
            self.compact();
        }
        self.pending_peak = Some(
            self.pending_peak
                .map_or(peak, |pending| pending.merge(peak)),
        );
        self.pending_buckets += 1;
        if self.pending_buckets == self.buckets_per_peak {
            if let Some(pending) = self.pending_peak.take() {
                self.peaks.push(pending);
            }
            self.pending_buckets = 0;
        }
    }

    /// Completes the bounded pyramid.
    #[must_use]
    pub fn finish(mut self) -> PeakPyramid {
        if let Some(pending) = self.pending_peak.take() {
            self.peaks.push(pending);
        }
        PeakPyramid::from_peaks(self.peaks, self.frames_per_peak, self.total_frames)
    }

    fn compact(&mut self) {
        debug_assert_eq!(self.pending_buckets, 0);
        debug_assert_eq!(self.peaks.len() % 2, 0);
        let compacted = self
            .peaks
            .chunks_exact(2)
            .map(|pair| pair[0].merge(pair[1]))
            .collect();
        self.peaks = compacted;
        self.buckets_per_peak = self.buckets_per_peak.saturating_mul(2);
        self.frames_per_peak = self
            .base_frames_per_peak
            .saturating_mul(self.buckets_per_peak);
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
        assert_eq!(
            PeakPyramid::from_peaks(vec![Peak::default(), Peak::default()], 4, 4),
            PeakPyramid::default(),
            "only the final supplied peak may be partial"
        );
        assert_eq!(
            PeakPyramid::from_peaks(vec![Peak::default(), Peak::default()], 4, 9),
            PeakPyramid::default(),
            "supplied peaks cannot represent more frames than their nominal spans"
        );
    }

    #[test]
    fn incomplete_interleaved_frame_is_ignored_without_allocating_frame_references() {
        let pyramid = PeakPyramid::from_interleaved_i16(&[-5, 7, 99], 2, 1);

        assert_eq!(pyramid.total_frames(), 1);
        assert_eq!(
            pyramid.levels()[0].peaks,
            [Peak {
                minimum: -5,
                maximum: 7,
            }]
        );
    }

    #[test]
    fn width_reduction_merges_instead_of_skipping_transients() {
        let pyramid = PeakPyramid::from_peaks(
            vec![
                Peak {
                    minimum: -1,
                    maximum: 1,
                },
                Peak {
                    minimum: -30_000,
                    maximum: 29_000,
                },
                Peak {
                    minimum: -2,
                    maximum: 2,
                },
                Peak {
                    minimum: -3,
                    maximum: 3,
                },
            ],
            1,
            4,
        );

        assert_eq!(
            pyramid.reduced_for_width(2),
            [
                Peak {
                    minimum: -30_000,
                    maximum: 29_000,
                },
                Peak {
                    minimum: -3,
                    maximum: 3,
                },
            ]
        );
    }

    #[test]
    fn width_reduction_weights_a_partial_final_bucket_by_exact_frames() {
        let full_bucket = Peak {
            minimum: -1,
            maximum: 1,
        };
        let one_frame_bucket = Peak {
            minimum: -30_000,
            maximum: 30_000,
        };
        let pyramid = PeakPyramid::from_peaks(vec![full_bucket, one_frame_bucket], 4_096, 4_097);

        let reduced = pyramid.reduced_for_width(80);

        assert_eq!(reduced.len(), 80);
        assert!(
            reduced[..79].iter().all(|peak| *peak == full_bucket),
            "the 4,096-frame bucket must occupy the first 79 columns"
        );
        assert_eq!(
            reduced[79],
            full_bucket.merge(one_frame_bucket),
            "the one-frame tail must affect only the final intersecting column"
        );
    }

    #[test]
    fn width_reduction_repeats_peaks_for_their_exact_frame_spans() {
        let three_frame_bucket = Peak {
            minimum: -3,
            maximum: 3,
        };
        let one_frame_bucket = Peak {
            minimum: -1,
            maximum: 1,
        };
        let pyramid = PeakPyramid::from_peaks(vec![three_frame_bucket, one_frame_bucket], 3, 4);

        assert_eq!(
            pyramid.reduced_for_width(4),
            [
                three_frame_bucket,
                three_frame_bucket,
                three_frame_bucket,
                one_frame_bucket,
            ]
        );
    }

    #[test]
    fn streaming_builder_compacts_without_exceeding_peak_limit() {
        let mut builder = PeakPyramidBuilder::new(4, 4).expect("valid builder");
        for value in 0_i16..10 {
            builder.push(
                Peak {
                    minimum: -value,
                    maximum: value,
                },
                4,
            );
        }
        let pyramid = builder.finish();

        assert_eq!(pyramid.total_frames(), 40);
        assert!(pyramid.levels()[0].peaks.len() <= 4);
        assert_eq!(pyramid.levels()[0].frames_per_peak, 16);
        assert_eq!(
            pyramid.levels()[0].peaks,
            [
                Peak {
                    minimum: -3,
                    maximum: 3,
                },
                Peak {
                    minimum: -7,
                    maximum: 7,
                },
                Peak {
                    minimum: -9,
                    maximum: 9,
                },
            ]
        );
    }

    #[test]
    fn streaming_builder_rejects_an_impossible_peak_reservation_without_panicking() {
        assert!(PeakPyramidBuilder::new(1, usize::MAX).is_none());
    }

    #[test]
    fn retained_bytes_include_spare_peak_capacity() {
        let mut peaks = Vec::with_capacity(1_024);
        peaks.push(Peak {
            minimum: -1,
            maximum: 1,
        });
        let pyramid = PeakPyramid::from_peaks(peaks, 1, 1);

        assert!(
            pyramid.retained_bytes() >= 1_024_usize.saturating_mul(std::mem::size_of::<Peak>())
        );
    }

    #[test]
    fn streaming_builder_keeps_exact_partial_final_frame_count() {
        let mut builder = PeakPyramidBuilder::new(4, 4).expect("valid builder");
        builder.push(
            Peak {
                minimum: -10,
                maximum: 10,
            },
            4,
        );
        builder.push(
            Peak {
                minimum: -20,
                maximum: 20,
            },
            2,
        );

        let pyramid = builder.finish();
        assert_eq!(pyramid.total_frames(), 6);
        assert_eq!(pyramid.levels()[0].peaks.len(), 2);
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
