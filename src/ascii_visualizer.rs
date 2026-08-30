//! Audio-reactive fullscreen ASCII visualization.

use serde::Serialize;

/// Maximum number of columns rendered for any frontend request.
pub const MAX_RENDER_WIDTH: usize = 320;

/// Maximum number of rows rendered for any frontend request.
pub const MAX_RENDER_HEIGHT: usize = 120;

/// One bounded snapshot of audio measurements published by the playback backend.
///
/// Values come from `FFmpeg`'s `astats` and `aspectralstats` filters inside the
/// already playing mpv process. No media is fetched or decoded a second time.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct AudioVisualizationSample {
    /// Root-mean-square signal level in decibels relative to full scale.
    pub rms_db: f32,
    /// Peak signal level in decibels relative to full scale.
    pub peak_db: f32,
    /// First-channel zero crossings divided by the number of samples.
    pub zero_crossing_rate: f32,
    /// Spectral center of mass in hertz.
    pub centroid_hz: f32,
    /// Spectral spread around the centroid in hertz.
    pub spread_hz: f32,
    /// Change in the spectrum since the preceding analysis window.
    pub flux: f32,
    /// Frequency below which most spectral energy falls.
    pub rolloff_hz: f32,
}

impl Default for AudioVisualizationSample {
    fn default() -> Self {
        Self {
            rms_db: -90.0,
            peak_db: -90.0,
            zero_crossing_rate: 0.0,
            centroid_hz: 0.0,
            spread_hz: 0.0,
            flux: 0.0,
            rolloff_hz: 0.0,
        }
    }
}

impl AudioVisualizationSample {
    /// Returns a stable audio-rich sample for renderer and frontend tests.
    #[cfg(test)]
    fn fixture() -> Self {
        Self {
            rms_db: -11.5,
            peak_db: -2.0,
            zero_crossing_rate: 0.18,
            centroid_hz: 2_400.0,
            spread_hz: 1_800.0,
            flux: 0.42,
            rolloff_hz: 7_800.0,
        }
    }

    /// Maps the signal level to a finite `0.0..=1.0` render intensity.
    fn level(self) -> f32 {
        db_unit(self.rms_db)
    }

    /// Maps the peak level to a finite `0.0..=1.0` render intensity.
    fn peak(self) -> f32 {
        db_unit(self.peak_db)
    }
}

/// The five stable fullscreen visual styles exposed to frontends.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub enum AsciiVisualizationMode {
    /// Audio-shaped vertical columns.
    #[default]
    Bars,
    /// Concentric audio pulses.
    Pulse,
    /// Falling audio-reactive glyphs.
    Rain,
    /// A spectral tunnel centered in the viewport.
    Tunnel,
    /// An audio-driven star field.
    Stars,
}

impl AsciiVisualizationMode {
    /// Every mode in left-to-right switching order.
    pub const ALL: [Self; 5] = [
        Self::Bars,
        Self::Pulse,
        Self::Rain,
        Self::Tunnel,
        Self::Stars,
    ];

    /// Returns the short name shown by each frontend.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bars => "Bars",
            Self::Pulse => "Pulse",
            Self::Rain => "Rain",
            Self::Tunnel => "Tunnel",
            Self::Stars => "Stars",
        }
    }

    /// Selects the style immediately to the left, wrapping at the beginning.
    #[must_use]
    pub const fn previous(self) -> Self {
        match self {
            Self::Bars => Self::Stars,
            Self::Pulse => Self::Bars,
            Self::Rain => Self::Pulse,
            Self::Tunnel => Self::Rain,
            Self::Stars => Self::Tunnel,
        }
    }

    /// Selects the style immediately to the right, wrapping at the end.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Bars => Self::Pulse,
            Self::Pulse => Self::Rain,
            Self::Rain => Self::Tunnel,
            Self::Tunnel => Self::Stars,
            Self::Stars => Self::Bars,
        }
    }
}

/// Produces a deterministic, bounded ASCII frame for a frontend viewport.
///
/// The frontend supplies only cell dimensions and a monotonically wrapping
/// frame counter. Every output line contains exactly the returned width in
/// single-column ASCII characters, including spaces.
#[must_use]
pub fn render_ascii_frame(
    mode: AsciiVisualizationMode,
    width: u16,
    height: u16,
    sample: AudioVisualizationSample,
    frame: u64,
) -> Vec<String> {
    let width = usize::from(width).min(MAX_RENDER_WIDTH);
    let height = usize::from(height).min(MAX_RENDER_HEIGHT);
    if width == 0 || height == 0 {
        return Vec::new();
    }
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| render_cell(mode, x, y, width, height, sample, frame))
                .collect()
        })
        .collect()
}

fn render_cell(
    mode: AsciiVisualizationMode,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    sample: AudioVisualizationSample,
    frame: u64,
) -> char {
    match mode {
        AsciiVisualizationMode::Bars => bars_cell(x, y, width, height, sample, frame),
        AsciiVisualizationMode::Pulse => pulse_cell(x, y, width, height, sample, frame),
        AsciiVisualizationMode::Rain => rain_cell(x, y, width, height, sample, frame),
        AsciiVisualizationMode::Tunnel => tunnel_cell(x, y, width, height, sample, frame),
        AsciiVisualizationMode::Stars => stars_cell(x, y, width, height, sample, frame),
    }
}

fn bars_cell(
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    sample: AudioVisualizationSample,
    frame: u64,
) -> char {
    let level = sample.level();
    let horizontal = ratio(x, width);
    let centroid = frequency_unit(sample.centroid_hz);
    let spread = (frequency_unit(sample.spread_hz) * 0.75 + 0.08).clamp(0.08, 0.8);
    let distance = (horizontal - centroid).abs() / spread;
    let spectral_shape = (-2.4 * distance * distance).exp();
    let ripple = ((horizontal * 19.0 + frame_phase(frame) * 0.16).sin() + 1.0) * 0.12;
    let energy = (level * (0.24 + spectral_shape * 0.82 + ripple)
        + finite_unit(sample.flux) * ripple)
        .clamp(0.0, 1.0);
    let from_bottom = 1.0 - ratio(y, height);
    if from_bottom > energy {
        return if from_bottom - energy < 0.025 && sample.peak() > 0.25 {
            '.'
        } else {
            ' '
        };
    }
    intensity_glyph(from_bottom / energy.max(0.01))
}

fn pulse_cell(
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    sample: AudioVisualizationSample,
    frame: u64,
) -> char {
    let (horizontal, vertical) = centered(x, y, width, height);
    let radius = (horizontal * horizontal + vertical * vertical).sqrt();
    let phase = frame_phase(frame) * (0.10 + finite_unit(sample.zero_crossing_rate) * 0.35);
    let rings = (radius * (9.0 + frequency_unit(sample.centroid_hz) * 13.0) - phase).sin();
    let width = 0.84 - sample.level() * 0.58;
    let strength =
        (1.0 - rings.abs() / width.max(0.12)).clamp(0.0, 1.0) * (0.2 + sample.level() * 0.8);
    if strength <= 0.08 {
        ' '
    } else {
        intensity_glyph(strength)
    }
}

fn rain_cell(
    x: usize,
    y: usize,
    _width: usize,
    _height: usize,
    sample: AudioVisualizationSample,
    frame: u64,
) -> char {
    let peak = sample.peak();
    let speed = 1
        + u64::from(peak >= 0.1)
        + u64::from(peak >= 0.3)
        + u64::from(peak >= 0.5)
        + u64::from(peak >= 0.7)
        + u64::from(peak >= 0.9);
    let offset = usize::try_from(frame.wrapping_mul(speed) % 127).unwrap_or_default();
    let shifted_y = y.wrapping_add(offset);
    let cell_seed = cell_hash(x, shifted_y / 3, 0);
    let density = 0.025 + sample.level() * 0.18 + finite_unit(sample.flux) * 0.06;
    if hash_unit(cell_seed) > density {
        return ' ';
    }
    match shifted_y % 3 {
        0 => '#',
        1 => '|',
        _ => '.',
    }
}

fn tunnel_cell(
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    sample: AudioVisualizationSample,
    frame: u64,
) -> char {
    let (horizontal, vertical) = centered(x, y, width, height);
    let radius = (horizontal * horizontal + vertical * vertical).sqrt();
    let angle = vertical.atan2(horizontal);
    let animation_frame = frame_phase(frame);
    let rotation = animation_frame * 0.035 * (1.0 + finite_unit(sample.flux) * 3.0);
    let spokes = (angle * (4.0 + frequency_unit(sample.rolloff_hz) * 8.0) + rotation)
        .sin()
        .abs();
    let rings = (radius * 18.0 - animation_frame * (0.08 + sample.level() * 0.25))
        .sin()
        .abs();
    let strength = ((1.0 - spokes.min(rings)) * (0.18 + sample.level() * 0.82)).clamp(0.0, 1.0);
    if strength < 0.3 {
        ' '
    } else {
        intensity_glyph(strength)
    }
}

fn stars_cell(
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    sample: AudioVisualizationSample,
    frame: u64,
) -> char {
    let depth = frame / 2;
    let seed = cell_hash(x, y, depth);
    let (horizontal, vertical) = centered(x, y, width, height);
    let radius = (horizontal * horizontal + vertical * vertical)
        .sqrt()
        .min(1.5);
    let density = 0.015 + sample.level() * 0.09 + radius * finite_unit(sample.flux) * 0.04;
    if hash_unit(seed) > density {
        return ' ';
    }
    let brightness = (hash_unit(seed.rotate_left(17)) * 0.45 + sample.peak() * 0.4 + radius * 0.25)
        .clamp(0.0, 1.0);
    intensity_glyph(brightness)
}

fn centered(x: usize, y: usize, width: usize, height: usize) -> (f32, f32) {
    let horizontal = ratio(x, width) * 2.0 - 1.0;
    let vertical = (ratio(y, height) * 2.0 - 1.0)
        * (bounded_coordinate(height) / bounded_coordinate(width))
        * 2.0;
    (horizontal, vertical)
}

fn ratio(position: usize, extent: usize) -> f32 {
    if extent <= 1 {
        0.0
    } else {
        bounded_coordinate(position) / bounded_coordinate(extent - 1)
    }
}

fn db_unit(value: f32) -> f32 {
    if value.is_finite() {
        ((value + 70.0) / 70.0).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn frequency_unit(value: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        (value.ln_1p() / 22_001.0_f32.ln()).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn finite_unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn frame_phase(frame: u64) -> f32 {
    let bounded = u16::try_from(frame % u64::from(u16::MAX)).unwrap_or_default();
    f32::from(bounded)
}

fn bounded_coordinate(value: usize) -> f32 {
    f32::from(u16::try_from(value).unwrap_or(u16::MAX))
}

fn intensity_glyph(value: f32) -> char {
    if value < 0.2 {
        '.'
    } else if value < 0.4 {
        ':'
    } else if value < 0.62 {
        '*'
    } else if value < 0.82 {
        'O'
    } else {
        '#'
    }
}

fn cell_hash(x: usize, y: usize, frame: u64) -> u64 {
    let mut value = (x as u64).wrapping_mul(0x9E37_79B1_85EB_CA87)
        ^ (y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ frame.wrapping_mul(0x1656_67B1_9E37_79F9);
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value.wrapping_mul(0x94D0_49BB_1331_11EB) ^ (value >> 31)
}

fn hash_unit(value: u64) -> f32 {
    let low_bits = u16::try_from(value & 0xffff).unwrap_or_default();
    f32::from(low_bits) / f32::from(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_exactly_five_stable_visualization_modes() {
        assert_eq!(AsciiVisualizationMode::ALL.len(), 5);
        assert_eq!(
            AsciiVisualizationMode::ALL.map(AsciiVisualizationMode::label),
            ["Bars", "Pulse", "Rain", "Tunnel", "Stars"],
        );
    }

    #[test]
    fn every_mode_produces_one_bounded_ascii_cell_per_requested_position() {
        let sample = AudioVisualizationSample::fixture();
        for mode in AsciiVisualizationMode::ALL {
            let frame = render_ascii_frame(mode, 47, 13, sample, 9);
            assert_eq!(frame.len(), 13);
            assert!(frame.iter().all(|line| line.chars().count() == 47));
            assert!(
                frame
                    .iter()
                    .flat_map(|line| line.chars())
                    .all(|character| character.is_ascii())
            );
        }
    }

    #[test]
    fn live_audio_metrics_change_the_rendered_frame() {
        let quiet = AudioVisualizationSample::default();
        let loud = AudioVisualizationSample::fixture();
        for mode in AsciiVisualizationMode::ALL {
            assert_ne!(
                render_ascii_frame(mode, 40, 12, quiet, 4),
                render_ascii_frame(mode, 40, 12, loud, 4),
                "{mode:?} must react to audio instead of elapsed time alone",
            );
        }
    }

    #[test]
    fn renderer_caps_untrusted_frontend_dimensions() {
        let frame = render_ascii_frame(
            AsciiVisualizationMode::Bars,
            u16::MAX,
            u16::MAX,
            AudioVisualizationSample::fixture(),
            0,
        );
        assert_eq!(frame.len(), MAX_RENDER_HEIGHT);
        assert!(frame.iter().all(|line| line.len() == MAX_RENDER_WIDTH));
    }
}
