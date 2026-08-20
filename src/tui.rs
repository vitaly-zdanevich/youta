//! Ratatui user interface and input mapping.
//!
//! This module renders Youta's own controls. An external player backend never
//! writes to the terminal and does not create a second user interface.

use std::io::{self, IsTerminal, Stdout};
use std::path::PathBuf;
use std::time::Duration;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::style::{Attribute, Colored, ResetColor, SetAttribute};
use crossterm::terminal::{
    Clear as ClearTerminal, ClearType, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
    disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::CellDiffOption;
#[cfg(test)]
use ratatui::layout::Size;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Gauge, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use ratatui::{Terminal, TerminalOptions, Viewport};
#[cfg(feature = "images")]
use ratatui_image::StatefulImage as TerminalImage;
use unicode_segmentation::UnicodeSegmentation;

use crate::config::{
    DEFAULT_THUMBNAIL_HEIGHT, MIN_THUMBNAIL_HEIGHT, SubscriptionsLayout, ThumbnailMode,
};
use crate::domain::{Chapter, MediaId, SourceKind, decode_url_path_segment_once};
#[cfg(all(feature = "gpm", target_os = "linux"))]
use crate::gpm::LinuxConsoleInput;
use crate::links::{chapter_title_for_display, is_advertisement_chapter_title};
use crate::playback::PlaybackStatus;
#[cfg(feature = "qr")]
use crate::qr_code::QrMatrix;
use crate::report_actions::{SystemReportActions, system_url_opener_name};
use crate::subscriptions::SubscriptionKind;
use crate::terminal_environment::{TerminalAttachment, openrc_manages_system};
#[cfg(feature = "local-browser")]
use crate::text_file_open::{
    TextFileOpenLifecycle, TextFileOpenPlan, spawn_detached_text_file_open,
};
#[cfg(feature = "images")]
use crate::thumbnails::{ThumbnailCapability, ThumbnailManager, ThumbnailProtocol, ThumbnailState};
use crate::waveform::Peak;

pub use crate::view::*;

use crate::keymap::{Key, KeyPress, PopupGeometry, ScrollGeometry};

/// Fixed-width marker rendered for the same actively playing Commons value.
const WIKIDATA_MEDIA_PAUSE_SYMBOL: &str = "⏸";

/// Marker shown while the current media is playing.
const PLAYBACK_PLAYING_SYMBOL: &str = "▶";

/// Marker shown while the current media is paused.
const PLAYBACK_PAUSED_SYMBOL: &str = "⏸";

/// Most chapter-label rows Youta may reserve above the seek track.
const MAX_CHAPTER_LABEL_ROWS: u16 = 4;

/// Body rows retained even when a dense chapter timeline requests more space.
const MIN_BODY_ROWS: u16 = 8;

/// Maximum number of terminal rows used by the expanded local waveform.
const WAVEFORM_ROWS: u16 = 4;

/// Player rows reserved for the waveform plus its one-line playback status.
const WAVEFORM_PLAYER_ROWS: u16 = WAVEFORM_ROWS + 1;

/// Seek-bar visual style.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SeekBarStyle {
    /// A compact terminal gauge.
    #[default]
    Line,
    /// A small animated cat label on the progress marker.
    NyanCat,
}

/// UI color and visibility preferences.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiSettings {
    /// Display shortcut labels inside buttons.
    pub show_hotkeys: bool,
    /// Use the humorous DOS-RPG-inspired border palette.
    pub funny_mode: bool,
    /// Seek-bar rendering mode.
    pub seek_bar_style: SeekBarStyle,
    /// Whether supported terminals may fetch and render thumbnails.
    pub thumbnails: ThumbnailMode,
    /// Maximum thumbnail height in terminal rows.
    pub thumbnail_height: u16,
    /// Prefetch artwork for all currently loaded global Search rows.
    pub prefetch_search_thumbnails: bool,
    /// Persistent thumbnail byte cache selected by the loaded configuration.
    pub thumbnail_cache_dir: Option<PathBuf>,
    /// `FFmpeg` used to extract the one frame a local video is previewed by.
    pub ffmpeg_executable: PathBuf,
    /// Redraw period while playback is active.
    pub playing_tick: Duration,
    /// Redraw period while idle or paused.
    pub idle_tick: Duration,
}

/// Maximum event-loop sleep while one foreground Local listing is pending.
///
/// The tighter interval applies only for the short lifetime of a directory
/// request. Idle browsing retains [`UiSettings::idle_tick`], so an open Local
/// tab does not continuously poll or waste battery.
const LOCAL_BROWSE_RESPONSE_POLL_INTERVAL: Duration = Duration::from_millis(25);

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            show_hotkeys: true,
            funny_mode: false,
            seek_bar_style: SeekBarStyle::Line,
            thumbnails: ThumbnailMode::Auto,
            thumbnail_height: DEFAULT_THUMBNAIL_HEIGHT,
            prefetch_search_thumbnails: true,
            thumbnail_cache_dir: None,
            ffmpeg_executable: PathBuf::from("ffmpeg"),
            playing_tick: Duration::from_millis(250),
            idle_tick: Duration::from_secs(1),
        }
    }
}

trait ThumbnailRenderer {
    /// Applies the current physical-TTY artwork preference.
    ///
    /// Renderers for graphical terminals and no-image builds ignore this
    /// policy because it only governs the Linux-console half-block fallback.
    fn set_tty_images_enabled(&mut self, _enabled: bool) -> bool {
        false
    }
    fn poll(&mut self) -> bool;
    fn is_enabled(&self) -> bool;
    fn is_pending(&self) -> bool {
        false
    }
    fn needs_immediate_redraw(&self) -> bool {
        false
    }
    /// Reports whether the current frame can render actual artwork pixels.
    ///
    /// Loading and error placeholders deliberately return `false` so the TUI
    /// never exposes an image-expansion hit target where no image is visible.
    fn has_rendered_artwork(&self) -> bool {
        false
    }
    /// Returns the ready artwork area prepared by the background worker.
    ///
    /// `None` retains the full requested area for loading and failure
    /// placeholders, whose text deliberately owns the complete area.
    fn prepared_artwork_area(&self, _available: Rect) -> Option<Rect> {
        None
    }
    fn synchronize(&mut self, source: Option<&url::Url>, area: Rect) -> bool;
    /// Synchronizes a selected local video with its midpoint-frame target.
    fn synchronize_local_video(&mut self, _source: &LocalVideoThumbnailView, _area: Rect) -> bool {
        false
    }
    /// Replaces the cache-only backlog for artwork selected by the TUI.
    fn synchronize_prefetch(&mut self, _sources: &[url::Url]) -> bool {
        false
    }
    /// Temporarily hides artwork behind a modal without invalidating its work.
    ///
    /// Implementations retain the selected target, ready protocol, and
    /// in-flight generation so closing a popup never starts the same load
    /// again.
    fn obscure(&mut self) -> bool {
        false
    }
    fn clear(&mut self) -> bool;
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme);
}

#[cfg(feature = "images")]
struct TerminalThumbnailRenderer {
    manager: ThumbnailManager,
    mode: ThumbnailMode,
    cache_directory: Option<PathBuf>,
    ffmpeg_executable: PathBuf,
    tty_images_enabled: bool,
    tty_image_policy_applies: bool,
    suspended_tty_manager: Option<ThumbnailManager>,
    clear_before_ready: bool,
    followup_frame_pending: bool,
    visible_source: Option<url::Url>,
    prefetched_visible_source: Option<url::Url>,
    prefetch_sources: Vec<url::Url>,
}

#[cfg(feature = "images")]
impl TerminalThumbnailRenderer {
    /// Wraps the asynchronous manager with terminal-frame transition state.
    #[cfg(test)]
    fn new(manager: ThumbnailManager) -> Self {
        let tty_image_policy_applies = matches!(
            manager.capability(),
            ThumbnailCapability::Supported(ThumbnailProtocol::Halfblocks)
        );
        Self::new_with_runtime_policy(
            manager,
            ThumbnailMode::Auto,
            None,
            PathBuf::from("ffmpeg"),
            true,
            tty_image_policy_applies,
        )
    }

    /// Wraps a manager with the configuration needed for live TTY toggling.
    fn new_with_runtime_policy(
        manager: ThumbnailManager,
        mode: ThumbnailMode,
        cache_directory: Option<PathBuf>,
        ffmpeg_executable: PathBuf,
        tty_images_enabled: bool,
        tty_image_policy_applies: bool,
    ) -> Self {
        Self {
            manager,
            mode,
            cache_directory,
            ffmpeg_executable,
            tty_images_enabled,
            tty_image_policy_applies,
            suspended_tty_manager: None,
            clear_before_ready: false,
            followup_frame_pending: false,
            visible_source: None,
            prefetched_visible_source: None,
            prefetch_sources: Vec::new(),
        }
    }
}

#[cfg(feature = "images")]
impl ThumbnailRenderer for TerminalThumbnailRenderer {
    fn set_tty_images_enabled(&mut self, enabled: bool) -> bool {
        if !self.tty_image_policy_applies || self.tty_images_enabled == enabled {
            return false;
        }
        self.tty_images_enabled = enabled;
        if enabled {
            self.manager = self.suspended_tty_manager.take().unwrap_or_else(|| {
                self.cache_directory
                    .as_ref()
                    .map_or_else(
                        || ThumbnailManager::from_current_terminal_with_tty_images(self.mode, true),
                        |cache_directory| {
                            ThumbnailManager::from_current_terminal_with_cache_and_tty_images(
                                self.mode,
                                cache_directory.clone(),
                                true,
                            )
                        },
                    )
                    .with_video_frame_program(self.ffmpeg_executable.clone())
            });
        } else {
            let disabled = ThumbnailManager::from_current_terminal(ThumbnailMode::Off);
            self.suspended_tty_manager = Some(std::mem::replace(&mut self.manager, disabled));
        }
        self.clear_before_ready = false;
        self.followup_frame_pending = false;
        self.visible_source = None;
        self.prefetched_visible_source = None;
        self.prefetch_sources.clear();
        true
    }

    fn poll(&mut self) -> bool {
        let changed = self.manager.poll();
        if changed {
            self.clear_before_ready = self.manager.state() == &ThumbnailState::Ready;
            self.followup_frame_pending = false;
        }
        changed
    }

    fn is_enabled(&self) -> bool {
        self.manager.is_enabled()
    }

    fn is_pending(&self) -> bool {
        self.manager.state() == &ThumbnailState::Loading
    }

    fn needs_immediate_redraw(&self) -> bool {
        self.followup_frame_pending
    }

    fn has_rendered_artwork(&self) -> bool {
        self.manager.state() == &ThumbnailState::Ready
            && !self.clear_before_ready
            && self.manager.protocol().is_some()
    }

    fn prepared_artwork_area(&self, available: Rect) -> Option<Rect> {
        self.manager.render_size().map(|render_size| {
            Rect::new(
                available.x,
                available.y,
                render_size.width.min(available.width),
                render_size.height.min(available.height),
            )
        })
    }

    fn synchronize(&mut self, source: Option<&url::Url>, area: Rect) -> bool {
        self.visible_source = source.cloned();
        let changed = self.manager.synchronize(source, area);
        if changed {
            self.clear_before_ready = false;
            self.followup_frame_pending = false;
        }
        changed
    }

    fn synchronize_local_video(&mut self, source: &LocalVideoThumbnailView, area: Rect) -> bool {
        self.visible_source = None;
        let changed =
            self.manager
                .synchronize_local_video(&source.path, source.midpoint_millis, area);
        if changed {
            self.clear_before_ready = false;
            self.followup_frame_pending = false;
        }
        changed
    }

    fn synchronize_prefetch(&mut self, sources: &[url::Url]) -> bool {
        let sources_unchanged = self.prefetch_sources == sources;
        if sources_unchanged && self.prefetched_visible_source == self.visible_source {
            return false;
        }
        self.prefetch_sources.clear();
        self.prefetch_sources.extend_from_slice(sources);
        self.prefetched_visible_source
            .clone_from(&self.visible_source);
        self.manager.synchronize_prefetch(&self.prefetch_sources)
    }

    fn obscure(&mut self) -> bool {
        // The popup is rendered after the body and overwrites its terminal
        // cells. Keeping the manager intact lets a ready or in-flight image
        // resume on the first frame after the popup closes.
        false
    }

    fn clear(&mut self) -> bool {
        self.visible_source = None;
        self.clear_before_ready = false;
        self.followup_frame_pending = false;
        self.manager.clear()
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        match self.manager.state().clone() {
            ThumbnailState::Loading => {
                frame.render_widget(
                    Paragraph::new("Loading thumbnail…")
                        .style(theme.muted)
                        .alignment(Alignment::Center),
                    area,
                );
            }
            ThumbnailState::Ready => {
                if self.clear_before_ready {
                    // Image protocols mark their cells as skipped so ratatui does
                    // not overwrite the pixels. A dedicated ordinary frame first
                    // erases the loading label that would otherwise remain in the
                    // terminal's previous buffer underneath those skipped cells.
                    self.clear_before_ready = false;
                    self.followup_frame_pending = true;
                } else if let Some(protocol) = self.manager.protocol_mut() {
                    frame.render_stateful_widget(TerminalImage::default(), area, protocol);
                    if matches!(
                        self.manager.capability(),
                        ThumbnailCapability::Supported(ThumbnailProtocol::Halfblocks)
                    ) {
                        quantize_linux_console_thumbnail(frame.buffer_mut(), area);
                    }
                    self.followup_frame_pending = false;
                }
            }
            ThumbnailState::Failed(error) => {
                self.followup_frame_pending = false;
                frame.render_widget(
                    Paragraph::new(format!("Thumbnail unavailable: {error}"))
                        .style(theme.muted)
                        .alignment(Alignment::Center)
                        .wrap(Wrap { trim: true }),
                    area,
                );
            }
            ThumbnailState::Disabled | ThumbnailState::Unsupported | ThumbnailState::Idle => {
                self.followup_frame_pending = false;
            }
        }
    }
}

/// Converts half-block image cells to the Linux VT's dependable 16-color set.
///
/// `ratatui-image` emits true-color cells for its half-block protocol. A Linux
/// virtual console does not reliably implement 24-bit SGR, so emitting those
/// cells verbatim can produce incorrect colors or escape-sequence artifacts.
#[cfg(feature = "images")]
fn quantize_linux_console_thumbnail(buffer: &mut ratatui::buffer::Buffer, area: Rect) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let Some(cell) = buffer.cell_mut((x, y)) else {
                continue;
            };
            cell.fg = nearest_ansi16_color(cell.fg);
            cell.bg = nearest_ansi16_color(cell.bg);
            // Half-block pixels do not carry text semantics. Removing any
            // modifier left in a reused cell prevents the Linux console from
            // simulating italic, bold, or dim attributes by changing colors.
            cell.modifier = Modifier::empty();
        }
    }
}

/// Makes every Linux-VT color transition explicit to Ratatui's modifier diff.
///
/// Linux virtual consoles implement bright foreground colors and indexed/RGB
/// foregrounds by mutating the same intensity state as SGR bold. Resetting
/// only the foreground color does not restore normal intensity, while
/// Crossterm models color and bold as independent state. Representing bright
/// colors as bright ANSI colors paired with [`Modifier::BOLD`] keeps both
/// models in sync and prevents isolated cells from retaining gray or bright
/// glyphs after a partial redraw. A base color carrying logical bold is
/// promoted to its bright variant because Crossterm emits modifiers before
/// colors and the later Linux-VT color sequence also changes intensity.
fn normalize_linux_console_buffer(buffer: &mut ratatui::buffer::Buffer) {
    normalize_linux_console_buffer_for_color_output(
        buffer,
        !Colored::ansi_color_disabled_memoized(),
    );
}

/// Applies the Linux-console invariant for the active Crossterm color policy.
fn normalize_linux_console_buffer_for_color_output(
    buffer: &mut ratatui::buffer::Buffer,
    color_output_enabled: bool,
) {
    for cell in &mut buffer.content {
        cell.modifier.remove(Modifier::DIM | Modifier::ITALIC);
        let (foreground, bright) = linux_console_foreground(
            nearest_linux_console_ansi16_color(cell.fg),
            cell.modifier.contains(Modifier::BOLD),
        );
        if bright {
            cell.modifier.insert(Modifier::BOLD);
        }
        if !color_output_enabled {
            cell.fg = Color::Reset;
            cell.bg = Color::Reset;
            continue;
        }
        cell.fg = foreground;
        cell.bg = linux_console_background(nearest_linux_console_ansi16_color(cell.bg));
    }
}

/// Reapplies console normalization after cursor overlays mutate the frame.
fn normalize_physical_linux_console_frame(frame: &mut Frame<'_>, view: &ViewModel) {
    if view.physical_linux_console {
        normalize_linux_console_buffer(frame.buffer_mut());
    }
}

/// Conventional RGB values of the Linux virtual console's named ANSI colors.
const LINUX_CONSOLE_ANSI16: [(Color, [u8; 3]); 16] = [
    (Color::Black, [0, 0, 0]),
    (Color::Red, [170, 0, 0]),
    (Color::Green, [0, 170, 0]),
    (Color::Yellow, [170, 85, 0]),
    (Color::Blue, [0, 0, 170]),
    (Color::Magenta, [170, 0, 170]),
    (Color::Cyan, [0, 170, 170]),
    (Color::Gray, [170, 170, 170]),
    (Color::DarkGray, [85, 85, 85]),
    (Color::LightRed, [255, 85, 85]),
    (Color::LightGreen, [85, 255, 85]),
    (Color::LightYellow, [255, 255, 85]),
    (Color::LightBlue, [85, 85, 255]),
    (Color::LightMagenta, [255, 85, 255]),
    (Color::LightCyan, [85, 255, 255]),
    (Color::White, [255, 255, 255]),
];

/// Maps an indexed or RGB color to the nearest Linux-console ANSI color.
fn nearest_linux_console_ansi16_color(color: Color) -> Color {
    let rgb = match color {
        Color::Rgb(red, green, blue) => [red, green, blue],
        Color::Indexed(index) => linux_console_indexed_rgb(index),
        _ => return color,
    };
    LINUX_CONSOLE_ANSI16
        .into_iter()
        .min_by_key(|(_, candidate)| {
            let red_delta = i32::from(rgb[0]) - i32::from(candidate[0]);
            let green_delta = i32::from(rgb[1]) - i32::from(candidate[1]);
            let blue_delta = i32::from(rgb[2]) - i32::from(candidate[2]);
            red_delta * red_delta + green_delta * green_delta + blue_delta * blue_delta
        })
        .map_or(Color::Reset, |(color, _)| color)
}

/// Reproduces the Linux VT's conversion of an indexed color to RGB.
fn linux_console_indexed_rgb(index: u8) -> [u8; 3] {
    if index < 8 {
        return [
            if index & 1 != 0 { 0xaa } else { 0x00 },
            if index & 2 != 0 { 0xaa } else { 0x00 },
            if index & 4 != 0 { 0xaa } else { 0x00 },
        ];
    }
    if index < 16 {
        return [
            if index & 1 != 0 { 0xff } else { 0x55 },
            if index & 2 != 0 { 0xff } else { 0x55 },
            if index & 4 != 0 { 0xff } else { 0x55 },
        ];
    }
    if index < 232 {
        let mut cube = u16::from(index) - 16;
        let blue = (cube % 6) * 255 / 6;
        cube /= 6;
        let green = (cube % 6) * 255 / 6;
        let red = (cube / 6) * 255 / 6;
        return [red as u8, green as u8, blue as u8];
    }
    let gray = u16::from(index) * 10 - 2312;
    [gray as u8; 3]
}

/// Keeps foreground color and tracked Linux-VT intensity in agreement.
fn linux_console_foreground(color: Color, bold: bool) -> (Color, bool) {
    match color {
        Color::DarkGray
        | Color::LightRed
        | Color::LightGreen
        | Color::LightYellow
        | Color::LightBlue
        | Color::LightMagenta
        | Color::LightCyan
        | Color::White => (color, true),
        Color::Black if bold => (Color::DarkGray, true),
        Color::Red if bold => (Color::LightRed, true),
        Color::Green if bold => (Color::LightGreen, true),
        Color::Yellow if bold => (Color::LightYellow, true),
        Color::Blue if bold => (Color::LightBlue, true),
        Color::Magenta if bold => (Color::LightMagenta, true),
        Color::Cyan if bold => (Color::LightCyan, true),
        Color::Gray if bold => (Color::White, true),
        color => (color, false),
    }
}

/// Collapses unsupported bright backgrounds to their base Linux-VT hues.
fn linux_console_background(color: Color) -> Color {
    match color {
        Color::DarkGray => Color::Black,
        Color::LightRed => Color::Red,
        Color::LightGreen => Color::Green,
        Color::LightYellow => Color::Yellow,
        Color::LightBlue => Color::Blue,
        Color::LightMagenta => Color::Magenta,
        Color::LightCyan => Color::Cyan,
        Color::White => Color::Gray,
        color => color,
    }
}

/// Maps an RGB color to the nearest conventional Linux-console ANSI color.
#[cfg(feature = "images")]
fn nearest_ansi16_color(color: Color) -> Color {
    if !matches!(color, Color::Rgb(..)) {
        return color;
    }
    nearest_linux_console_ansi16_color(color)
}

#[cfg(feature = "images")]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the no-thumbnails build returns None through the same interface"
)]
fn create_thumbnail_renderer(
    settings: &UiSettings,
    show_images_in_tty: bool,
) -> Option<Box<dyn ThumbnailRenderer>> {
    let manager = settings
        .thumbnail_cache_dir
        .as_ref()
        .map_or_else(
            || {
                ThumbnailManager::from_current_terminal_with_tty_images(
                    settings.thumbnails,
                    show_images_in_tty,
                )
            },
            |cache_dir| {
                ThumbnailManager::from_current_terminal_with_cache_and_tty_images(
                    settings.thumbnails,
                    cache_dir.clone(),
                    show_images_in_tty,
                )
            },
        )
        .with_video_frame_program(settings.ffmpeg_executable.clone());
    Some(Box::new(
        TerminalThumbnailRenderer::new_with_runtime_policy(
            manager,
            settings.thumbnails,
            settings.thumbnail_cache_dir.clone(),
            settings.ffmpeg_executable.clone(),
            show_images_in_tty,
            current_terminal_attachment().is_physical_linux_virtual_console(),
        ),
    ))
}

#[cfg(not(feature = "images"))]
fn create_thumbnail_renderer(
    _settings: &UiSettings,
    _show_images_in_tty: bool,
) -> Option<Box<dyn ThumbnailRenderer>> {
    None
}

/// Captures the bounded terminal facts used to decide whether external-opener
/// controls apply. The result does not depend on terminal-image support.
fn current_terminal_attachment() -> TerminalAttachment {
    let term = std::env::var("TERM").ok();
    let term_program = std::env::var("TERM_PROGRAM").ok();
    let tmux = std::env::var_os("TMUX").is_some()
        || term
            .as_deref()
            .is_some_and(|value| value.starts_with("tmux"))
        || term_program.as_deref() == Some("tmux");
    let ssh = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .into_iter()
        .any(|name| std::env::var_os(name).is_some());
    TerminalAttachment {
        linux: cfg!(target_os = "linux"),
        stdin_is_terminal: io::stdin().is_terminal(),
        stdout_is_terminal: io::stdout().is_terminal(),
        term,
        ssh,
        tmux,
        output_device: std::fs::read_link("/proc/self/fd/1").ok(),
    }
}

/// Cell-grid and pixel dimensions reported for the current terminal window.
///
/// These values come from the terminal device attached to Youta, not from a
/// desktop monitor or compositor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TerminalWindowMetrics {
    columns: u16,
    rows: u16,
    width_pixels: u16,
    height_pixels: u16,
}

impl TerminalWindowMetrics {
    /// Rejects incomplete terminal reports because they cannot preserve an
    /// image aspect ratio reliably.
    fn new(columns: u16, rows: u16, width_pixels: u16, height_pixels: u16) -> Option<Self> {
        (columns > 0 && rows > 0 && width_pixels > 0 && height_pixels > 0).then_some(Self {
            columns,
            rows,
            width_pixels,
            height_pixels,
        })
    }
}

/// Reads the current terminal window's grid and pixel dimensions.
///
/// Some PTYs and Windows terminals report zero pixel dimensions. Returning
/// `None` keeps Youta's configured thumbnail height in those environments.
fn current_terminal_window_metrics() -> Option<TerminalWindowMetrics> {
    let window = crossterm::terminal::window_size().ok()?;
    TerminalWindowMetrics::new(window.columns, window.rows, window.width, window.height)
}

/// Reads independently usable pixel dimensions for source artwork policies.
///
/// Some terminals report only one pixel dimension. Keeping each value
/// independently optional preserves YouTube's width-only policy while Yandex
/// artwork can require both dimensions.
fn current_terminal_window_pixels() -> (Option<u16>, Option<u16>) {
    crossterm::terminal::window_size().map_or((None, None), |window| {
        nonzero_terminal_window_pixels(window.width, window.height)
    })
}

/// Validates independently reported terminal-window pixel dimensions.
const fn nonzero_terminal_window_pixels(width: u16, height: u16) -> (Option<u16>, Option<u16>) {
    (
        if width == 0 { None } else { Some(width) },
        if height == 0 { None } else { Some(height) },
    )
}

/// Number of wrapped description rows below which YouTube artwork expands.
const SHORT_YOUTUBE_DESCRIPTION_LINE_LIMIT: usize = 15;

/// Reports whether one YouTube description is short at its rendered width.
///
/// The Details scrollbar owns one column whenever the pane is wider than one
/// cell, so this uses the same text width as the description renderer. Injected
/// video-link actions are included because they also occupy rendered cells.
fn youtube_description_is_short(details: &DetailView, pane_width: u16) -> bool {
    if details.source != "YouTube" {
        return false;
    }
    let description_width = pane_width.saturating_sub(1).max(1);
    let rendered_lines = wrap_description_source(
        &details.description,
        usize::from(description_width),
        &details.video_links,
    );
    rendered_lines.len() < SHORT_YOUTUBE_DESCRIPTION_LINE_LIMIT
}

/// Chooses a YouTube thumbnail height for one right-hand terminal pane.
///
/// At 1080 terminal-window pixels and above, or when a YouTube description
/// occupies fewer than 15 rendered rows, the returned row count maps the pane's
/// complete cell width to the selected thumbnail's source aspect ratio. A
/// short description can use the same 10×20 fallback cell geometry as the
/// image backend when a terminal omits pixel dimensions; the 1080-pixel rule
/// itself requires a complete window report.
fn youtube_thumbnail_height(
    configured_height: u16,
    pane_width: u16,
    youtube_video: bool,
    short_description: bool,
    thumbnail_dimensions: Option<(u32, u32)>,
    terminal_window: Option<TerminalWindowMetrics>,
) -> u16 {
    const LARGE_TERMINAL_HEIGHT_PIXELS: u16 = 1080;
    const FALLBACK_CELL_PIXELS: (u16, u16) = (10, 20);

    if !youtube_video || pane_width == 0 {
        return configured_height;
    }
    let (source_width, source_height) = thumbnail_dimensions.unwrap_or((16, 9));
    if source_width == 0 || source_height == 0 {
        return configured_height;
    }
    let (numerator, denominator) = match terminal_window {
        Some(window)
            if short_description || window.height_pixels >= LARGE_TERMINAL_HEIGHT_PIXELS =>
        {
            (
                u64::from(pane_width)
                    .saturating_mul(u64::from(window.width_pixels))
                    .saturating_mul(u64::from(source_height))
                    .saturating_mul(u64::from(window.rows)),
                u64::from(window.columns)
                    .saturating_mul(u64::from(source_width))
                    .saturating_mul(u64::from(window.height_pixels)),
            )
        }
        None if short_description => (
            u64::from(pane_width)
                .saturating_mul(u64::from(FALLBACK_CELL_PIXELS.0))
                .saturating_mul(u64::from(source_height)),
            u64::from(FALLBACK_CELL_PIXELS.1).saturating_mul(u64::from(source_width)),
        ),
        Some(_) | None => return configured_height,
    };
    let height = numerator
        .saturating_add(denominator / 2)
        .checked_div(denominator)
        .unwrap_or_default();
    u16::try_from(height)
        .unwrap_or(u16::MAX)
        .max(MIN_THUMBNAIL_HEIGHT)
}

/// Chooses a Yandex artwork height that displays the selected square source
/// at its useful pixel size without upscaling it beyond that source.
///
/// The ordinary thumbnail-height preference remains the minimum requested
/// block. Complete terminal metrics convert source pixels into terminal rows;
/// the same conservative 10×20 cell geometry used by the image backend is the
/// fallback when a terminal omits pixel dimensions.
fn yandex_music_thumbnail_height(
    configured_height: u16,
    pane_width: u16,
    thumbnail_dimensions: Option<(u32, u32)>,
    terminal_window: Option<TerminalWindowMetrics>,
) -> u16 {
    const FALLBACK_CELL_PIXELS: (u16, u16) = (10, 20);

    let Some((source_width, source_height)) = thumbnail_dimensions else {
        return configured_height;
    };
    if pane_width == 0 || source_width == 0 || source_height == 0 {
        return configured_height;
    }
    let (available_width_pixels, row_numerator, row_denominator) = terminal_window.map_or_else(
        || {
            (
                u64::from(pane_width).saturating_mul(u64::from(FALLBACK_CELL_PIXELS.0)),
                1_u64,
                u64::from(FALLBACK_CELL_PIXELS.1),
            )
        },
        |window| {
            (
                u64::from(pane_width)
                    .saturating_mul(u64::from(window.width_pixels))
                    .checked_div(u64::from(window.columns))
                    .unwrap_or_default(),
                u64::from(window.rows),
                u64::from(window.height_pixels),
            )
        },
    );
    let rendered_width_pixels = available_width_pixels.min(u64::from(source_width));
    let numerator = rendered_width_pixels
        .saturating_mul(u64::from(source_height))
        .saturating_mul(row_numerator);
    let denominator = u64::from(source_width).saturating_mul(row_denominator);
    let height = numerator
        .saturating_add(denominator / 2)
        .checked_div(denominator)
        .unwrap_or_default();
    u16::try_from(height)
        .unwrap_or(u16::MAX)
        .max(configured_height)
        .max(MIN_THUMBNAIL_HEIGHT)
}

/// Runs Youta in the current terminal until the controller requests shutdown.
pub fn run(controller: &mut impl UiController, settings: &UiSettings) -> io::Result<()> {
    if !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "Youta's interactive UI requires a terminal",
        ));
    }

    let terminal_attachment = current_terminal_attachment();
    let physical_linux_console = terminal_attachment.is_physical_linux_virtual_console();
    let openrc_managed = physical_linux_console && openrc_manages_system();
    controller.dispatch(UiAction::SetExternalOpenerAvailable(
        terminal_attachment.external_opener_available(),
    ));
    let mut session = TerminalSession::enter()?;
    let (width, height) = current_terminal_window_pixels();
    controller.dispatch(UiAction::SetTerminalWindowPixels { width, height });
    let mut input = TerminalInput::new();
    let mut thumbnail_renderer =
        create_thumbnail_renderer(settings, controller.view().show_images_in_tty);
    let mut hit_map = HitMap::default();
    let mut virtual_cursor = VirtualCursor::default();
    // Discovery reads filesystem metadata once and runs nothing, so the
    // terminal owns its clipboard transport for the whole session.
    let clipboard = SystemReportActions::new();
    loop {
        let mut renderer = thumbnail_renderer.take();
        if let Some(renderer) = renderer.as_mut() {
            synchronize_tty_image_preference(controller.view(), renderer.as_mut());
            renderer.poll();
            session.terminal.draw(|frame| {
                render_frame(
                    frame,
                    controller.view(),
                    settings,
                    &mut hit_map,
                    Some(renderer.as_mut()),
                );
                render_local_rename_cursor(frame, controller.view(), !virtual_cursor.active);
                render_virtual_cursor_overlay(frame, controller.view(), &mut virtual_cursor);
                normalize_physical_linux_console_frame(frame, controller.view());
            })?;
        } else {
            session.terminal.draw(|frame| {
                render_frame(frame, controller.view(), settings, &mut hit_map, None);
                render_local_rename_cursor(frame, controller.view(), !virtual_cursor.active);
                render_virtual_cursor_overlay(frame, controller.view(), &mut virtual_cursor);
                normalize_physical_linux_console_frame(frame, controller.view());
            })?;
        }
        if let Some(renderer) = renderer.as_deref_mut() {
            synchronize_thumbnail_prefetch(controller.view(), settings, renderer);
        }
        if controller.view().quitting {
            break;
        }

        let wait = event_wait(controller.view(), settings);
        let wait_outcome = wait_for_event_or_thumbnail(wait, renderer.as_deref_mut(), |timeout| {
            input.poll(timeout)
        })?;
        if wait_outcome == WaitOutcome::TerminalEvent {
            match input.read()? {
                Event::Key(key) => {
                    let cursor_was_active = virtual_cursor.active;
                    let f8_pressed = key.kind == KeyEventKind::Press && key.code == KeyCode::F(8);
                    match virtual_cursor.handle_key(key) {
                        VirtualCursorKey::PassThrough => {
                            if let Some(action) = key_action_with_page_rows(
                                key,
                                controller.view(),
                                visible_main_list_page_rows(&hit_map),
                                Some(&hit_map),
                            ) {
                                controller.dispatch(action);
                            }
                        }
                        VirtualCursorKey::Click(mouse) => {
                            if let Some(action) = mouse_action(mouse, &hit_map, controller.view()) {
                                controller.dispatch(action);
                            }
                        }
                        VirtualCursorKey::Consumed => {}
                    }
                    input.retry_gpm_on_f8_press(f8_pressed, physical_linux_console);
                    if let Some(notice) = virtual_cursor.take_gpm_unavailable_notice(
                        cursor_was_active,
                        ConsolePointerAvailability {
                            physical_linux_console,
                            gpm_supported: TerminalInput::gpm_supported(),
                            gpm_connected: input.gpm_connected(),
                            openrc_managed,
                        },
                    ) {
                        controller.dispatch(UiAction::ReportGpmUnavailable {
                            gpm_supported: notice.gpm_supported,
                            openrc_managed: notice.openrc_managed,
                        });
                    }
                }
                Event::Mouse(mouse) => {
                    virtual_cursor.follow_mouse(&mouse);
                    if let Some(action) = mouse_action(mouse, &hit_map, controller.view()) {
                        controller.dispatch(action);
                    }
                }
                Event::Resize(_, _) => {
                    let (width, height) = current_terminal_window_pixels();
                    controller.dispatch(UiAction::SetTerminalWindowPixels { width, height });
                }
                _ => {}
            }
        }
        // A terminal copies through a native helper, or through an OSC 52
        // escape written to its own tty when there is no helper — which is why
        // the copy happens here and not in the controller: neither transport
        // exists in a window, and the escape one would be written into nothing.
        if let Some(request) = controller.take_clipboard_request() {
            controller.report_clipboard_result(
                clipboard
                    .copy_report(&request.text)
                    .map_err(|error| error.to_string()),
            );
        }
        #[cfg(feature = "local-browser")]
        if let Some(plan) = controller.take_text_file_open_plan() {
            if let Some(renderer) = renderer.as_deref_mut() {
                renderer.clear();
            }
            let result = execute_text_file_open_plan(&mut session, plan);
            controller.report_text_file_open_result(result);
        }
        controller.tick();
        thumbnail_renderer = renderer;
    }
    Ok(())
}

/// Applies the controller's live physical-TTY artwork preference.
fn synchronize_tty_image_preference(
    view: &ViewModel,
    renderer: &mut dyn ThumbnailRenderer,
) -> bool {
    renderer.set_tty_images_enabled(view.show_images_in_tty)
}

/// Input source that opportunistically adds GPM without changing PTY behavior.
struct TerminalInput {
    #[cfg(all(feature = "gpm", target_os = "linux"))]
    linux_console: Option<LinuxConsoleInput>,
}

impl TerminalInput {
    fn new() -> Self {
        Self {
            #[cfg(all(feature = "gpm", target_os = "linux"))]
            linux_console: LinuxConsoleInput::try_current(),
        }
    }

    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        #[cfg(all(feature = "gpm", target_os = "linux"))]
        if let Some(input) = self.linux_console.as_mut() {
            let Ok(ready) = input.poll(timeout) else {
                // GPM is optional and may stop while Youta is running. Drop
                // the failed socket and retain keyboard input. The next F8
                // press explicitly retries it.
                self.linux_console = None;
                return event::poll(Duration::ZERO);
            };
            return Ok(ready);
        }
        event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<Event> {
        #[cfg(all(feature = "gpm", target_os = "linux"))]
        if let Some(input) = self.linux_console.as_mut() {
            return input.read();
        }
        event::read()
    }

    /// Reports whether this build includes the Linux GPM input adapter.
    const fn gpm_supported() -> bool {
        cfg!(all(feature = "gpm", target_os = "linux"))
    }

    /// Reports whether the live GPM control socket can currently supply input.
    fn gpm_connected(&self) -> bool {
        #[cfg(all(feature = "gpm", target_os = "linux"))]
        {
            return self.linux_console.is_some();
        }
        #[cfg(not(all(feature = "gpm", target_os = "linux")))]
        false
    }

    /// Retries GPM only for an explicit F8 press on a physical console.
    ///
    /// Startup retains its existing opportunistic attempt. Restricting later
    /// attempts to F8 avoids background filesystem or socket probes.
    fn retry_gpm_on_f8_press(&mut self, f8_pressed: bool, physical_linux_console: bool) -> bool {
        if !gpm_reconnect_needed(
            f8_pressed,
            ConsolePointerAvailability {
                physical_linux_console,
                gpm_supported: Self::gpm_supported(),
                gpm_connected: self.gpm_connected(),
                openrc_managed: false,
            },
        ) {
            return false;
        }
        #[cfg(all(feature = "gpm", target_os = "linux"))]
        {
            return retry_optional_input_with(
                &mut self.linux_console,
                LinuxConsoleInput::try_current,
            );
        }
        #[cfg(not(all(feature = "gpm", target_os = "linux")))]
        false
    }
}

/// Replaces a disconnected optional input through an injected factory.
///
/// An existing connection is retained without invoking the factory. The
/// return value reports whether this call installed a new input.
#[cfg(any(all(feature = "gpm", target_os = "linux"), test))]
fn retry_optional_input_with<T>(
    input: &mut Option<T>,
    factory: impl FnOnce() -> Option<T>,
) -> bool {
    if input.is_some() {
        return false;
    }
    *input = factory();
    input.is_some()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VirtualCursorKey {
    PassThrough,
    Consumed,
    Click(MouseEvent),
}

/// Runtime facts governing the physical-console GPM fallback notice.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ConsolePointerAvailability {
    /// Whether Youta is attached directly to a Linux virtual console.
    physical_linux_console: bool,
    /// Whether this binary contains the GPM input adapter.
    gpm_supported: bool,
    /// Whether the live GPM socket is connected.
    gpm_connected: bool,
    /// Whether OpenRC manages this system.
    openrc_managed: bool,
}

/// Returns whether an explicit F8 press can retry the GPM control socket.
fn gpm_reconnect_needed(f8_pressed: bool, availability: ConsolePointerAvailability) -> bool {
    f8_pressed
        && availability.physical_linux_console
        && availability.gpm_supported
        && !availability.gpm_connected
}

/// Facts retained for one semantic unavailable-GPM controller action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GpmUnavailableNotice {
    /// Whether starting a daemon can make this compiled adapter connect.
    gpm_supported: bool,
    /// Whether `rc-service` is an applicable service command.
    openrc_managed: bool,
}

/// Keyboard-controlled pointer used when no physical mouse input is available.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct VirtualCursor {
    active: bool,
    column: u16,
    row: u16,
    bounds: Rect,
    gpm_unavailable_notice_shown: bool,
}

impl VirtualCursor {
    /// Moves an active keyboard pointer to the latest physical mouse cell.
    ///
    /// This lets GPM drive the same visible square on a Linux virtual console
    /// without making ordinary terminal mouse movement activate the fallback.
    fn follow_mouse(&mut self, mouse: &MouseEvent) {
        if !self.active || self.bounds.is_empty() {
            return;
        }
        self.column = mouse
            .column
            .clamp(self.bounds.x, self.bounds.right().saturating_sub(1));
        self.row = mouse
            .row
            .clamp(self.bounds.y, self.bounds.bottom().saturating_sub(1));
    }

    fn handle_key(&mut self, key: KeyEvent) -> VirtualCursorKey {
        if key.code == KeyCode::F(8) {
            if key.kind == KeyEventKind::Press {
                self.active = !self.active;
                if self.active {
                    self.column = self
                        .bounds
                        .x
                        .saturating_add(self.bounds.width.saturating_sub(1) / 2);
                    self.row = self
                        .bounds
                        .y
                        .saturating_add(self.bounds.height.saturating_sub(1) / 2);
                }
            }
            return VirtualCursorKey::Consumed;
        }
        if !self.active {
            return VirtualCursorKey::PassThrough;
        }
        if key.kind == KeyEventKind::Release
            && matches!(
                key.code,
                KeyCode::Esc
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Enter
            )
        {
            return VirtualCursorKey::Consumed;
        }
        match key.code {
            KeyCode::Esc => {
                self.active = false;
                VirtualCursorKey::Consumed
            }
            KeyCode::Left => {
                self.column = self.column.saturating_sub(1).max(self.bounds.x);
                VirtualCursorKey::Consumed
            }
            KeyCode::Right => {
                self.column = self
                    .column
                    .saturating_add(1)
                    .min(self.bounds.right().saturating_sub(1));
                VirtualCursorKey::Consumed
            }
            KeyCode::Up => {
                self.row = self.row.saturating_sub(1).max(self.bounds.y);
                VirtualCursorKey::Consumed
            }
            KeyCode::Down => {
                self.row = self
                    .row
                    .saturating_add(1)
                    .min(self.bounds.bottom().saturating_sub(1));
                VirtualCursorKey::Consumed
            }
            KeyCode::Enter => VirtualCursorKey::Click(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: self.column,
                row: self.row,
                modifiers: KeyModifiers::NONE,
            }),
            _ => VirtualCursorKey::PassThrough,
        }
    }

    /// Returns one actionable notice when F8 activates the pointer without GPM.
    ///
    /// The notice is confined to a confirmed Linux virtual console and is
    /// emitted once per Youta run. An OpenRC command is included only when the
    /// active system exposes non-empty OpenRC runtime state.
    fn take_gpm_unavailable_notice(
        &mut self,
        was_active: bool,
        availability: ConsolePointerAvailability,
    ) -> Option<GpmUnavailableNotice> {
        if was_active
            || !self.active
            || self.gpm_unavailable_notice_shown
            || !availability.physical_linux_console
            || availability.gpm_connected
        {
            return None;
        }
        self.gpm_unavailable_notice_shown = true;
        Some(GpmUnavailableNotice {
            gpm_supported: availability.gpm_supported,
            openrc_managed: availability.gpm_supported && availability.openrc_managed,
        })
    }

    fn render(&mut self, frame: &mut Frame<'_>) {
        self.synchronize_bounds(frame.area());
        if self.bounds.is_empty() {
            return;
        }
        if !self.active {
            return;
        }
        let cell = &mut frame.buffer_mut()[(self.column, self.row)];
        if cell.symbol().trim().is_empty() {
            cell.set_symbol("■");
        }
        cell.set_style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::REVERSED),
        );
    }

    /// Updates the pointer bounds without necessarily drawing its overlay.
    fn synchronize_bounds(&mut self, bounds: Rect) {
        self.bounds = bounds;
        if self.bounds.is_empty() {
            self.active = false;
            return;
        }
        self.column = self
            .column
            .clamp(self.bounds.x, self.bounds.right().saturating_sub(1));
        self.row = self
            .row
            .clamp(self.bounds.y, self.bounds.bottom().saturating_sub(1));
    }
}

/// Renders the virtual pointer unless it could corrupt a scanner-sensitive QR symbol.
fn render_virtual_cursor_overlay(
    frame: &mut Frame<'_>,
    view: &ViewModel,
    virtual_cursor: &mut VirtualCursor,
) {
    #[cfg(feature = "qr")]
    if view.video_qr_popup.is_some() && view.error_popup.is_none() {
        virtual_cursor.synchronize_bounds(frame.area());
        return;
    }
    #[cfg(not(feature = "qr"))]
    let _ = view;
    virtual_cursor.render(frame);
}

/// Synchronizes cache-only artwork work with the visible screen.
///
/// Global search warming follows its explicit preference. Subscription source
/// artwork is restored only from previously cached channel metadata and is
/// always warmed so moving between known channels does not wait for a network
/// request.
fn synchronize_thumbnail_prefetch(
    view: &ViewModel,
    settings: &UiSettings,
    renderer: &mut dyn ThumbnailRenderer,
) -> bool {
    let mut sources = Vec::new();
    if let Some(source) = view
        .details
        .as_ref()
        .and_then(|details| details.expanded_thumbnail_url.as_ref())
    {
        sources.push(source.clone());
    }
    let rows = match view.screen {
        Screen::Search | Screen::YouTubeMusic if settings.prefetch_search_thumbnails => {
            view.rows.as_slice()
        }
        Screen::Subscriptions => view.subscriptions.sources.as_slice(),
        _ => &[],
    };
    for source in rows.iter().filter_map(|row| row.thumbnail_url.as_ref()) {
        if !sources.contains(source) {
            sources.push(source.clone());
        }
    }
    renderer.synchronize_prefetch(&sources)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitOutcome {
    TerminalEvent,
    ThumbnailRedraw,
    Timeout,
}

/// Waits for terminal input while probing only an in-flight thumbnail worker.
///
/// Cached thumbnails receive two quick probes, followed by progressively
/// slower checks. Idle operation without thumbnail work still uses one full
/// blocking terminal wait, and long network loads settle at four probes per
/// second.
fn wait_for_event_or_thumbnail(
    wait: Duration,
    mut thumbnail_renderer: Option<&mut (dyn ThumbnailRenderer + '_)>,
    mut terminal_event_ready: impl FnMut(Duration) -> io::Result<bool>,
) -> io::Result<WaitOutcome> {
    let Some(renderer) = thumbnail_renderer.as_mut() else {
        return terminal_event_ready(wait).map(|ready| {
            if ready {
                WaitOutcome::TerminalEvent
            } else {
                WaitOutcome::Timeout
            }
        });
    };
    if renderer.needs_immediate_redraw() {
        return Ok(WaitOutcome::ThumbnailRedraw);
    }
    if !renderer.is_pending() {
        return terminal_event_ready(wait).map(|ready| {
            if ready {
                WaitOutcome::TerminalEvent
            } else {
                WaitOutcome::Timeout
            }
        });
    }
    if renderer.poll() || !renderer.is_pending() {
        return Ok(WaitOutcome::ThumbnailRedraw);
    }

    let mut remaining = wait;
    let mut waited = Duration::ZERO;
    while !remaining.is_zero() {
        let probe = thumbnail_probe_interval(waited).min(remaining);
        if terminal_event_ready(probe)? {
            return Ok(WaitOutcome::TerminalEvent);
        }
        remaining = remaining.saturating_sub(probe);
        waited = waited.saturating_add(probe);
        if renderer.poll() || !renderer.is_pending() {
            return Ok(WaitOutcome::ThumbnailRedraw);
        }
    }
    Ok(WaitOutcome::Timeout)
}

/// Chooses an early cache-friendly probe followed by low-power network checks.
fn thumbnail_probe_interval(waited: Duration) -> Duration {
    if waited < Duration::from_millis(50) {
        Duration::from_millis(25)
    } else if waited < Duration::from_millis(100) {
        Duration::from_millis(50)
    } else if waited < Duration::from_millis(200) {
        Duration::from_millis(100)
    } else {
        Duration::from_millis(250)
    }
}

/// Stable-width ASCII frames shared by background activity indicators.
const ASCII_ACTIVITY_FRAMES: [char; 4] = ['|', '/', '-', '\\'];

/// Chooses a bounded redraw wait without introducing another timer source.
fn event_wait(view: &ViewModel, settings: &UiSettings) -> Duration {
    let playback_wait = if view.playback.paused {
        settings.idle_tick
    } else {
        settings.playing_tick
    };
    let wait = if view.local_browse_pending
        || view.local_artwork_pending
        || matches!(view.waveform, WaveformView::Loading { .. })
    {
        playback_wait.min(LOCAL_BROWSE_RESPONSE_POLL_INTERVAL)
    } else if view.search_activity.is_some() || view.subscriptions.loading || view.playback_starting
    {
        playback_wait.min(settings.playing_tick)
    } else {
        playback_wait
    };
    wait.max(Duration::from_millis(1))
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut setup_guard = TerminalSetupGuard { active: true };
        let mut stdout = io::stdout();
        write_terminal_startup(&mut stdout)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )?;
        setup_guard.active = false;
        Ok(Self { terminal })
    }

    /// Restores the ordinary terminal before a foreground console editor runs.
    #[cfg(feature = "local-browser")]
    fn suspend(&mut self) -> io::Result<()> {
        disable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            SetAttribute(Attribute::Reset),
            ResetColor,
            Show,
            DisableMouseCapture,
            LeaveAlternateScreen
        )?;
        self.terminal.show_cursor()
    }

    /// Re-enters Youta's clean full-screen terminal after an editor exits.
    #[cfg(feature = "local-browser")]
    fn resume(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        write_terminal_startup(self.terminal.backend_mut())?;
        self.terminal.clear()
    }
}

/// Executes one shell-free text-file plan with the required terminal lifecycle.
#[cfg(feature = "local-browser")]
fn execute_text_file_open_plan(
    session: &mut TerminalSession,
    plan: TextFileOpenPlan,
) -> Result<TextFileOpenLifecycle, String> {
    use std::process::Command;

    let lifecycle = plan.lifecycle;
    let mut command = Command::new(&plan.executable);
    command.args(&plan.arguments);
    match lifecycle {
        // Shared with the window, which reaches the same graphical opener and
        // must not grow a second copy of the spawn-and-reap dance.
        TextFileOpenLifecycle::Detached => spawn_detached_text_file_open(&plan).map(|()| lifecycle),
        TextFileOpenLifecycle::SuspendTuiAndWait => {
            session
                .suspend()
                .map_err(|error| format!("cannot suspend the terminal UI: {error}"))?;
            let editor_result = command.status();
            let resume_result = session.resume();
            if let Err(error) = resume_result {
                return Err(format!("cannot restore the terminal UI: {error}"));
            }
            let status = editor_result
                .map_err(|error| format!("cannot start {}: {error}", plan.executable.display()))?;
            if status.success() {
                Ok(lifecycle)
            } else {
                Err(format!(
                    "{} exited with {status}",
                    plan.executable.display()
                ))
            }
        }
    }
}

/// Writes the escape commands that initialize Youta's terminal session.
///
/// Clearing after requesting the alternate screen preserves a terminal
/// emulator's primary buffer while also giving Linux virtual consoles, which
/// may ignore the alternate-screen request, a clean full-screen canvas.
fn write_terminal_startup(writer: &mut impl io::Write) -> io::Result<()> {
    execute!(
        writer,
        SetTitle("Youta"),
        EnterAlternateScreen,
        SetAttribute(Attribute::Reset),
        ResetColor,
        ClearTerminal(ClearType::All),
        MoveTo(0, 0),
        EnableMouseCapture,
        Hide
    )
}

struct TerminalSetupGuard {
    active: bool,
}

impl Drop for TerminalSetupGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            Show,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            Show,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

#[derive(Clone, Debug, Default)]
struct HitMap {
    tabs: Vec<(Screen, Rect)>,
    rows: Rect,
    /// First model row represented by the visible main-list rectangle.
    rows_first_index: usize,
    /// Physical terminal rows occupied by one main-list model row.
    rows_row_height: u16,
    subscription_source_rows: Rect,
    /// First model source represented by the visible source-list rectangle.
    subscription_source_first_index: usize,
    subscription_item_rows: Rect,
    /// First model item represented by the visible subscription-item rectangle.
    subscription_item_first_index: usize,
    subscription_source_buttons: Vec<(UiAction, Rect)>,
    details_panel: Rect,
    /// Last information-panel owner rendered into the terminal pane.
    information_panel_identity: Option<InformationPanelIdentity>,
    /// Cells occupied by actual, ready artwork in the Details panel.
    thumbnail_area: Option<Rect>,
    /// Full-terminal click target used to close an expanded artwork overlay.
    thumbnail_overlay_area: Option<Rect>,
    /// Actual wrapped-line offset rendered in the Details description.
    details_scroll_offset: usize,
    /// Largest wrapped-line offset that can change the visible description.
    details_scroll_maximum: usize,
    detail_links: Vec<(usize, Rect)>,
    /// Exact one-cell actions injected after `YouTube` video URLs.
    description_video_actions: Vec<(UiAction, Rect)>,
    detail_buttons: Vec<(UiAction, Rect)>,
    detail_text_rows: Vec<SelectableDetailsRow>,
    seek_bar: Rect,
    seek_markers: Vec<(UiAction, Rect)>,
    /// Owner-bearing waveform click target; stale frames clear it before render.
    waveform_seek: Option<WaveformSeekTarget>,
    buttons: Vec<(UiAction, Rect)>,
    now_playing: Option<Rect>,
    error_buttons: Vec<(UiAction, Rect)>,
    /// Buttons rendered inside the public-comments popup.
    video_comments_buttons: Vec<(UiAction, Rect)>,
    /// Close control rendered inside the selected-video QR popup.
    #[cfg(feature = "qr")]
    video_qr_buttons: Vec<(UiAction, Rect)>,
    /// Wrapped text viewport inside the public-comments popup.
    video_comments_text_area: Rect,
    /// Actual first wrapped comment line rendered in the viewport.
    video_comments_scroll_offset: usize,
    /// Largest wrapped-line offset that changes the comments viewport.
    video_comments_scroll_maximum: usize,
    /// Number of wrapped comment lines visible on one page.
    video_comments_page_lines: usize,
    /// Close control rendered inside the recent-project-history popup.
    project_history_buttons: Vec<(UiAction, Rect)>,
    /// Wrapped commit/provenance viewport inside the project-history popup.
    project_history_text_area: Rect,
    /// Actual first wrapped project-history line rendered in the viewport.
    project_history_scroll_offset: usize,
    /// Largest wrapped-line offset that changes the project-history viewport.
    project_history_scroll_maximum: usize,
    /// Number of project-history lines visible on one page.
    project_history_page_lines: usize,
    youtube_setup_fields: Vec<(YouTubeSetupField, Rect)>,
    youtube_setup_buttons: Vec<(UiAction, Rect)>,
    yandex_music_setup_field: Option<Rect>,
    yandex_music_setup_buttons: Vec<(UiAction, Rect)>,
    rss_subscription_field: Option<Rect>,
    rss_subscription_buttons: Vec<(UiAction, Rect)>,
    preferences_buttons: Vec<(UiAction, Rect)>,
    /// Visible playlist membership rows inside the chooser popup.
    playlist_popup_rows: Rect,
    /// First playlist model row represented by [`Self::playlist_popup_rows`].
    playlist_popup_first_index: usize,
    playlist_popup_fields: Vec<(PlaylistEditorField, Rect)>,
    playlist_popup_buttons: Vec<(UiAction, Rect)>,
    /// Visible queue rows inside the queue popup.
    queue_popup_rows: Rect,
    /// First queue model row represented by [`Self::queue_popup_rows`].
    queue_popup_first_index: usize,
    queue_popup_buttons: Vec<(UiAction, Rect)>,
    private_note_buttons: Vec<(UiAction, Rect)>,
    /// Visible wrapped-text cells inside the private-note editor.
    private_note_text_area: Rect,
    /// Actual first wrapped visual line rendered by the private-note editor.
    private_note_scroll_offset: usize,
    /// Largest wrapped-line offset that can change the private-note viewport.
    private_note_scroll_maximum: usize,
    local_file_buttons: Vec<(UiAction, Rect)>,
    /// Visible destination rows inside the Local Move popup.
    local_move_rows: Rect,
    /// First destination model row represented by [`Self::local_move_rows`].
    local_move_first_index: usize,
}

/// Exact local media and duration represented by one waveform rectangle.
#[derive(Clone, Debug, Eq, PartialEq)]
struct WaveformSeekTarget {
    area: Rect,
    media_id: MediaId,
    generation: u64,
    duration: Duration,
}

/// Exact terminal cells belonging to one visible, selectable Details row.
#[derive(Clone, Debug, Default)]
struct SelectableDetailsRow {
    x: u16,
    y: u16,
    cells: Vec<String>,
}

/// Stable owner and layout of the currently rendered information panel.
#[derive(Clone, Debug, Eq, PartialEq)]
struct InformationPanelIdentity {
    /// Physical pane requiring invalidation when it moves or resizes.
    area: Rect,
    /// Source-specific field layout rendered for the owner.
    kind: InformationPanelKind,
    /// Heading text distinguishing Details and source-only panels.
    title: String,
    /// Whether this panel includes selectable Details controls.
    show_text_selection: bool,
    /// Stable selected item represented by the panel.
    owner: InformationPanelOwner,
}

/// Stable identity of the item that owns one information panel.
#[derive(Clone, Debug, Eq, PartialEq)]
enum InformationPanelOwner {
    /// No selected item currently owns the panel.
    Empty,
    /// Provider channel identity used by channel panels.
    Channel(String),
    /// Stable playable-media identity used by Details panels.
    Media(MediaId),
    /// Display metadata fallback for providers without a stable identifier.
    Display { title: String, source: String },
}

#[cfg(test)]
fn render(frame: &mut Frame<'_>, view: &ViewModel, settings: &UiSettings, hit_map: &mut HitMap) {
    render_frame(frame, view, settings, hit_map, None);
    render_local_rename_cursor(frame, view, true);
    normalize_physical_linux_console_frame(frame, view);
}

/// Chooses a bounded chapter-label height without taking space from the
/// minimum usable body, seek track, playback status, or controls.
fn chapter_label_row_count(
    view: &ViewModel,
    terminal_width: u16,
    terminal_height: u16,
    has_download: bool,
) -> u16 {
    if view.playback_chapters.is_empty() {
        return 0;
    }

    let fixed_rows = 2_u16
        .saturating_add(MIN_BODY_ROWS)
        .saturating_add(if has_download { 2 } else { 0 })
        .saturating_add(2)
        .saturating_add(2);
    let available_rows = terminal_height
        .saturating_sub(fixed_rows)
        .min(MAX_CHAPTER_LABEL_ROWS);
    if available_rows == 0 {
        return 0;
    }

    let duration = view.playback.duration.unwrap_or(Duration::ZERO);
    if duration.is_zero() {
        return 1;
    }
    let duration_seconds = duration.as_secs();
    let visible_chapters = view
        .playback_chapters
        .iter()
        .filter(|chapter| {
            chapter.start_seconds < duration_seconds
                && (!view.skip_advertisement_chapters
                    || !is_advertisement_chapter_title(&chapter.title))
        })
        .count();
    if visible_chapters == 0 {
        return 0;
    }
    if visible_chapters == 1 {
        return 1;
    }

    required_chapter_label_rows(view, duration, terminal_width)
        .min(available_rows)
        .min(MAX_CHAPTER_LABEL_ROWS)
}

/// Simulates the renderer's interval placement to reserve enough label rows.
///
/// Using the real truncated label widths keeps timestamped and names-only
/// layouts consistent with the number of rows they receive.
fn required_chapter_label_rows(view: &ViewModel, duration: Duration, width: u16) -> u16 {
    if duration.is_zero() || width == 0 {
        return u16::from(!view.playback_chapters.is_empty() && width > 0);
    }
    let layout = chapter_label_layout(view, duration, width, MAX_CHAPTER_LABEL_ROWS);
    if layout.saturated {
        MAX_CHAPTER_LABEL_ROWS
    } else {
        layout
            .placements
            .iter()
            .map(|placement| placement.row.saturating_add(1))
            .max()
            .unwrap_or_default()
    }
}

/// One packed chapter label whose marker remains independently time-aligned.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ChapterLabelPlacement {
    index: usize,
    row: u16,
    start: u16,
    width: u16,
    text: String,
}

/// Result of bounded chapter-label packing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ChapterLabelLayout {
    placements: Vec<ChapterLabelPlacement>,
    saturated: bool,
}

/// Packs chapter labels into the nearest available horizontal intervals.
///
/// Exact marker-aligned positions are preferred across all rows. When those
/// collide, the closest free interval is used, preferring the right side on an
/// equal-distance tie. The current, first, and last visible chapters receive
/// priority so a dense early cluster cannot consume every label row.
fn chapter_label_layout(
    view: &ViewModel,
    duration: Duration,
    width: u16,
    row_limit: u16,
) -> ChapterLabelLayout {
    if duration.is_zero() || width == 0 || row_limit == 0 {
        return ChapterLabelLayout::default();
    }
    let visible = visible_chapter_indices(view, duration);
    let current = current_visible_chapter_index(view, &visible);
    let mut order = Vec::with_capacity(visible.len());
    for candidate in current
        .into_iter()
        .chain(visible.first().copied())
        .chain(visible.last().copied())
        .chain(visible.iter().copied())
    {
        if !order.contains(&candidate) {
            order.push(candidate);
        }
    }

    let mut occupied = vec![Vec::<(u16, u16)>::new(); usize::from(row_limit)];
    let mut layout = ChapterLabelLayout::default();
    let show_hours = duration.as_secs() >= 60 * 60;
    for index in order {
        let chapter = &view.playback_chapters[index];
        let marker = rounded_duration_column(
            Duration::from_secs(chapter.start_seconds),
            duration,
            width.saturating_sub(1),
        );
        let maximum_width = usize::from(width).min(if Some(index) == current { 36 } else { 20 });
        let text = truncate_terminal_text(
            &chapter_timeline_label(
                chapter,
                Some(index) == current,
                view.show_chapter_timestamps,
                show_hours,
            ),
            maximum_width,
        );
        let label_width = terminal_text_width(&text).min(width);
        if label_width == 0 {
            continue;
        }
        let preferred = marker.min(width.saturating_sub(label_width));
        let exact_row = occupied.iter().position(|ranges| {
            interval_is_free(ranges, preferred, preferred.saturating_add(label_width))
        });
        let (row, start) = if let Some(row) = exact_row {
            (row, preferred)
        } else if let Some(candidate) =
            nearest_free_label_slot(&occupied, width, label_width, preferred)
        {
            candidate
        } else {
            layout.saturated = true;
            continue;
        };
        occupied[row].push((start, start.saturating_add(label_width)));
        occupied[row].sort_unstable_by_key(|range| range.0);
        layout.placements.push(ChapterLabelPlacement {
            index,
            row: u16::try_from(row).unwrap_or(row_limit.saturating_sub(1)),
            start,
            width: label_width,
            text,
        });
    }
    expand_chapter_labels_into_free_space(&mut layout.placements, view, current, show_hours, width);
    layout
}

/// Expands packed labels to the next occupied interval on the same row.
///
/// Conservative widths still decide collision-free placement, including the
/// active chapter's priority. Once those positions are stable, labels may use
/// otherwise blank cells to their right without moving or overlapping a
/// neighbour.
fn expand_chapter_labels_into_free_space(
    placements: &mut [ChapterLabelPlacement],
    view: &ViewModel,
    current: Option<usize>,
    show_hours: bool,
    width: u16,
) {
    let row_count = placements
        .iter()
        .map(|placement| usize::from(placement.row).saturating_add(1))
        .max()
        .unwrap_or_default();
    let mut row_starts = vec![Vec::new(); row_count];
    for placement in placements.iter() {
        row_starts[usize::from(placement.row)].push(placement.start);
    }
    for starts in &mut row_starts {
        starts.sort_unstable();
    }

    for placement in placements {
        let starts = &row_starts[usize::from(placement.row)];
        let next_index = starts.partition_point(|start| *start <= placement.start);
        let free_end = starts.get(next_index).copied().unwrap_or(width);
        let available_width = free_end.saturating_sub(placement.start);
        let text = truncate_terminal_text(
            &chapter_timeline_label(
                &view.playback_chapters[placement.index],
                Some(placement.index) == current,
                view.show_chapter_timestamps,
                show_hours,
            ),
            usize::from(available_width),
        );
        placement.width = terminal_text_width(&text).min(available_width);
        placement.text = text;
    }
}

fn interval_is_free(ranges: &[(u16, u16)], start: u16, end: u16) -> bool {
    ranges
        .iter()
        .all(|(used_start, used_end)| start >= *used_end || end <= *used_start)
}

/// Finds the closest shifted label position among every free row interval.
fn nearest_free_label_slot(
    occupied: &[Vec<(u16, u16)>],
    width: u16,
    label_width: u16,
    preferred: u16,
) -> Option<(usize, u16)> {
    let mut best: Option<(u16, bool, usize, u16)> = None;
    for (row, ranges) in occupied.iter().enumerate() {
        let mut cursor = 0_u16;
        let mut consider_interval = |free_start: u16, free_end: u16| {
            if free_end.saturating_sub(free_start) >= label_width {
                let latest = free_end.saturating_sub(label_width);
                let start = preferred.clamp(free_start, latest);
                let distance = start.abs_diff(preferred);
                let left_of_preferred = start < preferred;
                let score = (distance, left_of_preferred, row, start);
                if best.is_none_or(|current| score < current) {
                    best = Some(score);
                }
            }
        };
        for (used_start, used_end) in ranges {
            consider_interval(cursor, *used_start);
            cursor = cursor.max(*used_end);
        }
        consider_interval(cursor, width);
    }
    best.map(|(_, _, row, start)| (row, start))
}

/// Renders expanded artwork as a modal once its terminal pixels are ready.
///
/// While the enlarged target is loading, or while an image protocol advances
/// through its ordinary-cell clearing frame, the existing screen remains
/// visible behind a compact status label. This prevents a fullscreen `Clear`
/// from becoming a black frame before the terminal image can be emitted.
fn render_fullscreen_thumbnail_overlay(
    frame: &mut Frame<'_>,
    view: &ViewModel,
    theme: &Theme,
    hit_map: &mut HitMap,
    renderer: &mut dyn ThumbnailRenderer,
) {
    let Some(details) = view.details.as_ref() else {
        return;
    };
    let expanded_wikidata_entity = details
        .expanded_wikidata_item
        .as_deref()
        .and_then(|item_id| {
            details
                .wikidata_entities
                .iter()
                .find(|entity| entity.item_id == item_id)
        });
    let visible_thumbnail_url = details
        .expanded_thumbnail_url
        .as_ref()
        .or(details.thumbnail_url.as_ref())
        .or_else(|| expanded_wikidata_entity.and_then(|entity| entity.image_url.as_ref()));
    let visible_local_video = details.local_video_thumbnail.as_ref();
    let area = frame.area();

    if let Some(local_video) = visible_local_video {
        renderer.synchronize_local_video(local_video, area);
    } else {
        renderer.synchronize(visible_thumbnail_url, area);
    }
    let prepared = renderer.prepared_artwork_area(area).unwrap_or(area);
    let artwork_area = centered_sized_rect(prepared.width, prepared.height, area);
    let artwork_rendered = renderer.has_rendered_artwork();
    if !artwork_rendered {
        let status_width = area.width.min(48);
        let status_area = centered_sized_rect(status_width, 1, area);
        frame.render_widget(Clear, status_area);
        frame.render_widget(
            Paragraph::new("Loading enlarged thumbnail…")
                .style(theme.muted)
                .alignment(Alignment::Center),
            status_area,
        );
        // A ready terminal protocol deliberately consumes one ordinary-cell
        // transition frame before reporting renderable artwork. Rendering it
        // inside this compact status area advances that state while preserving
        // the useful body around it; the renderer then requests an immediate
        // follow-up frame for the actual fullscreen image.
        renderer.render(frame, status_area, theme);
        hit_map.thumbnail_overlay_area = Some(area);
        hit_map.thumbnail_area = None;
        return;
    }

    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(theme.base), area);
    renderer.render(frame, artwork_area, theme);
    hit_map.thumbnail_overlay_area = Some(area);
    hit_map.thumbnail_area = Some(artwork_area);
}

fn render_frame(
    frame: &mut Frame<'_>,
    view: &ViewModel,
    settings: &UiSettings,
    hit_map: &mut HitMap,
    mut thumbnail_renderer: Option<&mut dyn ThumbnailRenderer>,
) {
    let theme = Theme::for_terminal(settings.funny_mode, view.physical_linux_console);
    hit_map.thumbnail_overlay_area = None;
    frame.render_widget(Block::default().style(theme.base), frame.area());

    let chapter_label_rows = if view.waveform_visible {
        0
    } else {
        chapter_label_row_count(
            view,
            frame.area().width,
            frame.area().height,
            view.download.is_some(),
        )
    };
    let player_rows = if view.waveform_visible {
        WAVEFORM_PLAYER_ROWS
    } else {
        2_u16.saturating_add(chapter_label_rows)
    };
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(8),
            Constraint::Length(if view.download.is_some() { 2 } else { 0 }),
            Constraint::Length(player_rows),
            Constraint::Length(1),
        ])
        .split(frame.area());
    render_tabs(frame, sections[0], view, &theme, hit_map);
    let thumbnail_is_obscured = view.help_open
        || view.project_history_popup.is_some()
        || view.youtube_setup_popup.is_some()
        || view.yandex_music_setup_popup.is_some()
        || view.rss_subscription_popup.is_some()
        || view.preferences_popup.is_some()
        || view.playlist_popup.is_some()
        || view.private_note_popup.is_some()
        || view.local_file_popup.is_some()
        || view.video_comments_popup.is_some()
        || view.error_popup.is_some();
    #[cfg(feature = "qr")]
    let thumbnail_is_obscured = thumbnail_is_obscured || view.video_qr_popup.is_some();
    let thumbnail_is_fullscreen = !thumbnail_is_obscured
        && view.expanded_thumbnail_available()
        && thumbnail_renderer
            .as_ref()
            .is_some_and(|renderer| renderer.is_enabled());
    if thumbnail_is_obscured {
        if let Some(renderer) = thumbnail_renderer.as_mut() {
            renderer.obscure();
        }
        render_body(
            frame,
            sections[1],
            view,
            settings.show_hotkeys,
            settings.thumbnail_height,
            &theme,
            hit_map,
            None,
        );
    } else if thumbnail_is_fullscreen {
        render_body(
            frame,
            sections[1],
            view,
            settings.show_hotkeys,
            settings.thumbnail_height,
            &theme,
            hit_map,
            None,
        );
    } else {
        render_body(
            frame,
            sections[1],
            view,
            settings.show_hotkeys,
            settings.thumbnail_height,
            &theme,
            hit_map,
            thumbnail_renderer.take(),
        );
    }
    if let Some(download) = view.download.as_ref() {
        render_download_bar(frame, sections[2], download, &theme);
    }
    render_seek_bar(frame, sections[3], view, settings, &theme, hit_map);
    let status_line = view.transient_footer_notice.as_deref().unwrap_or("");
    render_buttons(
        frame,
        sections[4],
        settings,
        &theme,
        view.screen,
        view.youtube_search_sort,
        view.radio_sort,
        view.youtube_creative_commons_only,
        view.show_chapter_timestamps,
        view.autoplay,
        view.playlist_item.as_ref(),
        view.playlist_edit_available,
        view.playlist_back_available,
        view.local_folder_sizes_enabled
            .then_some(view.local_size_sort),
        &status_line,
        !view.playback.idle,
        hit_map,
    );
    if thumbnail_is_fullscreen && let Some(renderer) = thumbnail_renderer.as_deref_mut() {
        render_fullscreen_thumbnail_overlay(frame, view, &theme, hit_map, renderer);
    }
    if view.help_open {
        render_help(frame, view, &theme);
    }
    hit_map.project_history_buttons.clear();
    hit_map.project_history_text_area = Rect::default();
    hit_map.project_history_scroll_offset = 0;
    hit_map.project_history_scroll_maximum = 0;
    hit_map.project_history_page_lines = 0;
    if let Some(popup) = view.project_history_popup.as_ref() {
        render_project_history_popup(frame, popup, &theme, hit_map);
    }
    hit_map.youtube_setup_fields.clear();
    hit_map.youtube_setup_buttons.clear();
    if let Some(setup) = view.youtube_setup_popup.as_ref() {
        render_youtube_setup_popup(
            frame,
            setup,
            view.external_opener_available,
            &theme,
            hit_map,
        );
    }
    hit_map.yandex_music_setup_field = None;
    hit_map.yandex_music_setup_buttons.clear();
    if let Some(setup) = view.yandex_music_setup_popup.as_ref() {
        render_yandex_music_setup_popup(
            frame,
            setup,
            view.external_opener_available,
            &theme,
            hit_map,
        );
    }
    hit_map.rss_subscription_field = None;
    hit_map.rss_subscription_buttons.clear();
    if let Some(popup) = view.rss_subscription_popup.as_ref() {
        render_rss_subscription_popup(frame, popup, &theme, hit_map);
    }
    hit_map.preferences_buttons.clear();
    if let Some(preferences) = view.preferences_popup.as_ref() {
        render_preferences_popup(frame, preferences, &theme, hit_map);
    }
    hit_map.local_file_buttons.clear();
    hit_map.local_move_rows = Rect::default();
    hit_map.local_move_first_index = 0;
    if let Some(popup) = view.local_file_popup.as_ref() {
        render_local_file_popup(frame, popup, &theme, hit_map);
    }
    hit_map.playlist_popup_rows = Rect::default();
    hit_map.playlist_popup_first_index = 0;
    hit_map.playlist_popup_fields.clear();
    hit_map.playlist_popup_buttons.clear();
    if let Some(popup) = view.playlist_popup.as_ref() {
        render_playlist_popup(frame, popup, settings.show_hotkeys, &theme, hit_map);
    }
    hit_map.queue_popup_rows = Rect::default();
    hit_map.queue_popup_first_index = 0;
    hit_map.queue_popup_buttons.clear();
    if let Some(popup) = view.queue_popup.as_ref() {
        render_queue_popup(frame, popup, settings.show_hotkeys, &theme, hit_map);
    }
    hit_map.private_note_buttons.clear();
    hit_map.private_note_text_area = Rect::default();
    hit_map.private_note_scroll_offset = 0;
    hit_map.private_note_scroll_maximum = 0;
    if let Some(popup) = view.private_note_popup.as_ref() {
        render_private_note_popup(frame, popup, settings.show_hotkeys, &theme, hit_map);
    }
    hit_map.error_buttons.clear();
    hit_map.video_comments_buttons.clear();
    hit_map.video_comments_text_area = Rect::default();
    hit_map.video_comments_scroll_offset = 0;
    hit_map.video_comments_scroll_maximum = 0;
    hit_map.video_comments_page_lines = 0;
    if let Some(popup) = view.video_comments_popup.as_ref() {
        render_video_comments_popup(frame, popup, &theme, hit_map);
    }
    #[cfg(feature = "qr")]
    {
        hit_map.video_qr_buttons.clear();
        if let Some(popup) = view.video_qr_popup.as_ref() {
            render_video_qr_popup(frame, popup, &theme, hit_map);
        }
    }
    if let Some(error) = view.error_popup.as_ref() {
        render_error_popup(
            frame,
            error,
            view.external_opener_available,
            &theme,
            hit_map,
        );
    }
    if view.physical_linux_console {
        normalize_linux_console_buffer(frame.buffer_mut());
    }
}

fn render_download_bar(frame: &mut Frame<'_>, area: Rect, download: &DownloadView, theme: &Theme) {
    let completed = !download.active && download.completed_path.is_some();
    let ratio = if completed {
        1.0
    } else {
        download
            .total_bytes
            .filter(|total| *total > 0)
            .map_or(0.0, |total| {
                download_ratio(download.downloaded_bytes, total)
            })
    };
    let label = if let Some(path) = download.completed_path.as_deref() {
        format!("Downloaded: {path}")
    } else {
        let transferred = match download.total_bytes {
            Some(total) if total > 0 => format!(
                "{:.1}% · {} / {}",
                ratio * 100.0,
                human_bytes(download.downloaded_bytes),
                human_bytes(total)
            ),
            _ => format!("{} · size unknown", human_bytes(download.downloaded_bytes)),
        };
        let speed = download
            .bytes_per_second
            .map(|value| format!(" · {}/s", human_bytes(value)))
            .unwrap_or_default();
        let eta = download
            .eta_seconds
            .map(|value| format!(" · ETA {}", format_duration(Duration::from_secs(value))))
            .unwrap_or_default();
        format!(
            "{} {} · {transferred}{speed}{eta}",
            if download.active {
                "Downloading"
            } else {
                "Download stopped:"
            },
            download.title
        )
    };
    frame.render_widget(
        Gauge::default()
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(theme.border),
            )
            .gauge_style(if completed {
                Style::default().fg(Color::White).bg(Color::Green)
            } else {
                theme.progress
            })
            .ratio(ratio)
            .label(label),
        area,
    );
}

fn render_tabs(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ViewModel,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    hit_map.tabs.clear();
    if area.is_empty() {
        return;
    }
    const FULL_DIVIDER: &str = " │ ";
    const COMPACT_DIVIDER: &str = "│";
    let enabled = Screen::ALL
        .into_iter()
        .filter(|screen| screen.enabled())
        .collect::<Vec<_>>();
    let full_width = enabled
        .iter()
        .map(|screen| usize::from(terminal_text_width(screen.label())))
        .sum::<usize>()
        .saturating_add(
            usize::from(terminal_text_width(FULL_DIVIDER))
                .saturating_mul(enabled.len().saturating_sub(1)),
        );
    let compact = full_width > usize::from(area.width);
    let divider = if compact {
        COMPACT_DIVIDER
    } else {
        FULL_DIVIDER
    };
    let divider_width = terminal_text_width(divider);
    let compact_width = enabled
        .iter()
        .map(|screen| usize::from(terminal_text_width(screen.compact_label())))
        .sum::<usize>()
        .saturating_add(usize::from(divider_width).saturating_mul(enabled.len().saturating_sub(1)));
    let visible = if compact && compact_width > usize::from(area.width) {
        active_tab_window(&enabled, view.screen, area.width, divider_width)
    } else {
        0..enabled.len()
    };
    let mut spans = Vec::with_capacity(visible.len().saturating_mul(2));
    let mut x = area.x;
    for (index, screen) in enabled[visible].iter().copied().enumerate() {
        if index > 0 {
            if x >= area.right() {
                break;
            }
            spans.push(Span::styled(divider, theme.muted));
            x = x.saturating_add(divider_width);
        }
        if x >= area.right() {
            break;
        }
        let label = if compact {
            screen.compact_label()
        } else {
            screen.label()
        };
        let width = terminal_text_width(label).min(area.right().saturating_sub(x));
        if width == 0 {
            break;
        }
        spans.push(Span::styled(
            label,
            if screen == view.screen {
                theme.selected
            } else {
                theme.base
            },
        ));
        hit_map.tabs.push((screen, Rect::new(x, area.y, width, 1)));
        x = x.saturating_add(width);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Chooses a contiguous compact-tab window that always contains the active tab.
fn active_tab_window(
    screens: &[Screen],
    active: Screen,
    available_width: u16,
    divider_width: u16,
) -> std::ops::Range<usize> {
    let Some(active_index) = screens.iter().position(|screen| *screen == active) else {
        return 0..screens.len();
    };
    let widths = screens
        .iter()
        .map(|screen| terminal_text_width(screen.compact_label()))
        .collect::<Vec<_>>();
    let mut start = active_index;
    let mut end = active_index.saturating_add(1);
    let mut used = widths[active_index].min(available_width);

    while start > 0 || end < screens.len() {
        let left_cost = (start > 0).then(|| divider_width.saturating_add(widths[start - 1]));
        let right_cost = (end < screens.len()).then(|| divider_width.saturating_add(widths[end]));
        let prefer_left =
            active_index.saturating_sub(start) <= end.saturating_sub(active_index + 1);
        let added = if prefer_left {
            if left_cost.is_some_and(|cost| used.saturating_add(cost) <= available_width) {
                start -= 1;
                left_cost
            } else if right_cost.is_some_and(|cost| used.saturating_add(cost) <= available_width) {
                end += 1;
                right_cost
            } else {
                None
            }
        } else if right_cost.is_some_and(|cost| used.saturating_add(cost) <= available_width) {
            end += 1;
            right_cost
        } else if left_cost.is_some_and(|cost| used.saturating_add(cost) <= available_width) {
            start -= 1;
            left_cost
        } else {
            None
        };
        let Some(added) = added else {
            break;
        };
        used = used.saturating_add(added);
    }
    start..end
}

/// Builds a concise result-panel title without repeating the active tab label.
///
/// Search screens retain the search kind while idle and show only the submitted
/// query once results exist. Non-search screens rely on the active top tab and
/// therefore do not consume a second row for a duplicate title.
fn search_panel_title(view: &ViewModel) -> String {
    if !view.search_editing {
        // Which screens collect a query is not restated here: the window asks
        // the same question to decide whether it draws a search field.
        if view.screen.search_verb().is_none() {
            return String::new();
        }
        // Radio filters as the user types, so an idle empty filter says nothing
        // the tab has not already said.
        if view.screen == Screen::Radio && view.search_query.trim().is_empty() {
            return String::new();
        }
    }
    let search_title = if view.search_editing {
        let mut query = view.search_query.clone();
        let requested = view.search_cursor_byte.min(query.len());
        let cursor = if requested == query.len() {
            requested
        } else {
            query
                .grapheme_indices(true)
                .map(|(index, _)| index)
                .take_while(|index| *index <= requested)
                .last()
                .unwrap_or_default()
        };
        query.insert(cursor, '▏');
        match view.screen {
            Screen::Radio => format!(" Filter: {query} "),
            Screen::YandexMusic => format!(
                " {} search: {query} ",
                view.yandex_music_search_kind.title_label()
            ),
            _ => format!(" Search: {query} "),
        }
    } else if view.screen == Screen::Radio {
        format!(" Filter: {} ", view.search_query.trim())
    } else if view.screen == Screen::YandexMusic {
        match view.yandex_music_route {
            YandexMusicRouteView::Recommendations => " My Wave ".to_owned(),
            YandexMusicRouteView::Search => {
                let scope = view.yandex_music_search_kind.title_label();
                if view.search_query.is_empty() {
                    format!(" {scope} search ")
                } else {
                    format!(" {scope} · {} ", view.search_query)
                }
            }
            YandexMusicRouteView::Album => " Album ".to_owned(),
            YandexMusicRouteView::Artist => " Artist ".to_owned(),
        }
    } else if view.search_query.is_empty() {
        match view.screen {
            Screen::Search => format!(
                " {} search ",
                match view.search_kind {
                    SearchKind::Videos => "Video",
                    SearchKind::Channels => "Channel",
                }
            ),
            Screen::YouTubeMusic
            | Screen::Bandcamp
            | Screen::ApplePodcasts
            | Screen::LibriVox
            | Screen::TrackerMusic => " Search ".to_owned(),
            Screen::YandexMusic => {
                unreachable!("Yandex Music returned with its search scope above")
            }
            Screen::Local
            | Screen::Radio
            | Screen::Subscriptions
            | Screen::Downloaded
            | Screen::History
            | Screen::Playlists
            | Screen::Statistics => unreachable!("non-search screens returned above"),
        }
    } else {
        format!(" {} ", view.search_query)
    };
    if view
        .search_activity
        .is_some_and(|activity| activity.screen() == view.screen)
    {
        let frame =
            ASCII_ACTIVITY_FRAMES[view.search_animation_frame % ASCII_ACTIVITY_FRAMES.len()];
        format!(" {frame} {}", search_title.trim_start())
    } else {
        search_title
    }
}

/// Returns the playback-progress marker displayed independently of subscription state.
fn watched_marker(watched_percent: u8, playback_started: bool) -> &'static str {
    if !playback_started {
        return "●";
    }
    match watched_percent {
        0..=90 => "◐",
        _ => "○",
    }
}

/// Whether a row carries complete YouTube-video presentation metadata.
///
/// YouTube and `YouTube Music` share [`SourceKind::YouTube`] identities. The
/// live video row constructor supplies the exact `YouTube` presentation label
/// together with orientation, while restored rows without that semantic detail
/// retain the source-neutral progress presentation instead of being guessed.
fn uses_youtube_video_title_style(row: &RowView) -> bool {
    row.media_id
        .as_ref()
        .is_some_and(|media_id| media_id.source == SourceKind::YouTube && row.source == "YouTube")
}

/// Returns the end of the first standalone duration field in row metadata.
///
/// Subtitles retain their source-specific text and ordering. This recognizes
/// only canonical `M:SS` and `H:MM:SS` fields separated by ` · `, preventing
/// dates, URLs, sample rates, and other colon-bearing metadata from becoming
/// accidental progress anchors.
fn subtitle_duration_field_end(subtitle: &str) -> Option<usize> {
    const SEPARATOR: &str = " · ";

    let mut field_start = 0;
    for field in subtitle.split(SEPARATOR) {
        let field_end = field_start + field.len();
        if is_standalone_duration(field) {
            return Some(field_end);
        }
        field_start = field_end.saturating_add(SEPARATOR.len());
    }
    None
}

/// Reports whether one complete metadata field is `M:SS` or `H:MM:SS`.
fn is_standalone_duration(field: &str) -> bool {
    let components = field.split(':').collect::<Vec<_>>();
    match components.as_slice() {
        [minutes, seconds] => {
            numeric_component_under_sixty(minutes) && two_digit_component_under_sixty(seconds)
        }
        [hours, minutes, seconds] => {
            !hours.is_empty()
                && hours.bytes().all(|byte| byte.is_ascii_digit())
                && two_digit_component_under_sixty(minutes)
                && two_digit_component_under_sixty(seconds)
        }
        _ => false,
    }
}

/// Validates a one- or two-digit numeric component in the range `0..60`.
fn numeric_component_under_sixty(component: &str) -> bool {
    !component.is_empty()
        && component.len() <= 2
        && component.bytes().all(|byte| byte.is_ascii_digit())
        && component.parse::<u8>().is_ok_and(|value| value < 60)
}

/// Validates a zero-padded two-digit numeric component in the range `0..60`.
fn two_digit_component_under_sixty(component: &str) -> bool {
    component.len() == 2 && numeric_component_under_sixty(component)
}

fn render_body(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ViewModel,
    show_hotkeys: bool,
    thumbnail_height: u16,
    theme: &Theme,
    hit_map: &mut HitMap,
    thumbnail_renderer: Option<&mut dyn ThumbnailRenderer>,
) {
    hit_map.waveform_seek = None;
    hit_map.detail_links.clear();
    hit_map.detail_buttons.clear();
    hit_map.detail_text_rows.clear();
    hit_map.details_panel = Rect::default();
    hit_map.thumbnail_area = None;
    hit_map.details_scroll_offset = 0;
    hit_map.details_scroll_maximum = 0;
    hit_map.rows = Rect::default();
    hit_map.rows_first_index = 0;
    hit_map.rows_row_height = 2;
    hit_map.subscription_source_rows = Rect::default();
    hit_map.subscription_source_first_index = 0;
    hit_map.subscription_item_rows = Rect::default();
    hit_map.subscription_item_first_index = 0;
    hit_map.subscription_source_buttons.clear();

    if view.screen == Screen::Subscriptions {
        render_subscriptions_body(
            frame,
            area,
            view,
            show_hotkeys,
            thumbnail_height,
            theme,
            hit_map,
            thumbnail_renderer,
        );
        return;
    }

    let horizontal = area.width >= 80;
    let panes = Layout::default()
        .direction(if horizontal {
            Direction::Horizontal
        } else {
            Direction::Vertical
        })
        .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
        .split(area);

    let search_title = search_panel_title(view);
    hit_map.rows_row_height = row_list_height(&view.rows);
    (hit_map.rows, hit_map.rows_first_index) = render_row_list(
        frame,
        panes[0],
        search_title.trim(),
        &view.rows,
        true,
        view.selected,
        view.playing_media_id.as_ref(),
        view.playback.paused,
        view.radio_recording
            .as_ref()
            .map(|recording| recording.station_id.as_str()),
        !view.physical_linux_console,
        theme.heading,
        theme,
    );

    match view.right_panel_mode {
        RightPanelMode::Details => {
            render_details(
                frame,
                panes[1],
                view,
                show_hotkeys,
                thumbnail_height,
                theme,
                hit_map,
                thumbnail_renderer,
            );
        }
        RightPanelMode::Channel => {
            render_subscription_source_details(
                frame,
                panes[1],
                view,
                show_hotkeys,
                thumbnail_height,
                theme,
                hit_map,
                thumbnail_renderer,
            );
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the renderer keeps list data and presentation state explicit"
)]
fn render_row_list(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    rows: &[RowView],
    show_source: bool,
    selected_index: usize,
    playing_media_id: Option<&MediaId>,
    playback_paused: bool,
    recording_station_id: Option<&str>,
    allow_started_title_italics: bool,
    heading_style: Style,
    theme: &Theme,
) -> (Rect, usize) {
    let rows_area = render_main_panel_heading(frame, area, title, heading_style);
    let row_height = row_list_height(rows);
    let visible_rows =
        usize::from(rows_area.height / row_height).max(usize::from(rows_area.height > 0));
    let selected_index = selected_index.min(rows.len().saturating_sub(1));
    let first_index = selected_index
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(rows.len().saturating_sub(visible_rows));
    let playing_index = playing_media_id.and_then(|media_id| {
        rows.iter()
            .position(|row| row.media_id.as_ref() == Some(media_id))
    });
    let items = rows
        .iter()
        .enumerate()
        .skip(first_index)
        .take(visible_rows)
        .map(|(index, row)| {
            let selected = index == selected_index;
            let playing = playing_index == Some(index);
            let playing_symbol = if playback_paused {
                PLAYBACK_PAUSED_SYMBOL
            } else {
                PLAYBACK_PLAYING_SYMBOL
            };
            let row_style = if selected {
                theme.selected.fg(Color::Black)
            } else if playing {
                theme.accent.add_modifier(Modifier::BOLD)
            } else {
                theme.base
            };
            let has_playback_progress = row.media_id.is_some() && !row.hide_watched_marker;
            let playback_started = row.playback_started || row.watched_percent > 0;
            let youtube_video_title = uses_youtube_video_title_style(row);
            let mut title_style = if selected {
                row_style
            } else if youtube_video_title && playback_started {
                if row.vertical {
                    theme.vertical_video_started
                } else {
                    theme.muted
                }
            } else if row.vertical {
                let style = theme.vertical_video;
                if playing {
                    style.add_modifier(Modifier::BOLD)
                } else {
                    style
                }
            } else {
                row_style
            };
            if youtube_video_title && !selected {
                if playback_started {
                    title_style = title_style
                        .remove_modifier(Modifier::BOLD)
                        .remove_modifier(Modifier::ITALIC);
                } else {
                    title_style = title_style.add_modifier(Modifier::BOLD);
                }
            } else if !youtube_video_title
                && has_playback_progress
                && playback_started
                && allow_started_title_italics
            {
                title_style = title_style.add_modifier(Modifier::ITALIC);
            }
            let show_watched_marker = has_playback_progress && !youtube_video_title;
            let marker = if row.subscribed { "◆" } else { " " };
            let progress = if !has_playback_progress || row.watched_percent == 0 {
                String::new()
            } else {
                format!(" {}%", row.watched_percent)
            };
            let source_style = if selected || playing {
                row_style
            } else {
                source_style(&row.source, theme)
            };
            let secondary_style = if selected || playing {
                row_style
            } else {
                theme.muted
            };
            let watched_style = if selected || playing {
                row_style
            } else if !playback_started {
                theme.muted
            } else {
                theme.accent
            };
            let marked_style = if selected || playing {
                row_style
            } else {
                theme.accent
            };
            let favorite_style = if selected || playing {
                row_style
            } else {
                theme.accent.add_modifier(Modifier::BOLD)
            };
            let radio_recording = recording_station_id.is_some_and(|station_id| {
                row.media_id.as_ref().is_some_and(|media_id| {
                    media_id.source == SourceKind::Radio && media_id.external_id == station_id
                })
            });
            let recording_style = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
            let mut title_spans = if row.compact {
                let mut spans = Vec::with_capacity(3);
                if playing {
                    spans.push(Span::styled(format!("{playing_symbol} "), row_style));
                }
                if row.radio_favorite {
                    spans.push(Span::styled("★ ", favorite_style));
                }
                if radio_recording {
                    spans.push(Span::styled("● ", recording_style));
                }
                if row.local_marked {
                    spans.push(Span::styled("✓ ", marked_style));
                }
                if show_watched_marker {
                    spans.push(Span::styled(
                        format!("{} ", watched_marker(row.watched_percent, playback_started)),
                        watched_style,
                    ));
                }
                spans
            } else if show_source {
                let mut spans = vec![
                    Span::styled(
                        format!("{} ", if playing { playing_symbol } else { " " }),
                        row_style,
                    ),
                    Span::styled(format!("{marker} "), source_style),
                ];
                if row.local_marked {
                    spans.push(Span::styled("✓ ", marked_style));
                }
                if show_watched_marker {
                    spans.push(Span::styled(
                        format!("{} ", watched_marker(row.watched_percent, playback_started)),
                        watched_style,
                    ));
                }
                spans
            } else if playing {
                let mut spans = vec![Span::styled(format!("{playing_symbol} "), row_style)];
                if row.local_marked {
                    spans.push(Span::styled("✓ ", marked_style));
                }
                if show_watched_marker {
                    spans.push(Span::styled(
                        format!("{} ", watched_marker(row.watched_percent, playback_started)),
                        watched_style,
                    ));
                }
                spans
            } else {
                let mut spans = Vec::with_capacity(2);
                if row.local_marked {
                    spans.push(Span::styled("✓ ", marked_style));
                }
                if show_watched_marker {
                    spans.push(Span::styled(
                        format!("{} ", watched_marker(row.watched_percent, playback_started)),
                        watched_style,
                    ));
                }
                spans
            };
            title_spans.push(Span::styled(&row.title, title_style));
            if row_height == 1 && !row.subtitle.is_empty() {
                title_spans.push(Span::styled(" · ", secondary_style));
                title_spans.push(Span::styled(&row.subtitle, secondary_style));
            }
            if row_height == 1 && !progress.is_empty() {
                title_spans.push(Span::styled(progress.clone(), secondary_style));
            }
            let line = Line::from(title_spans);
            if row_height == 1 {
                return ListItem::new(line).style(row_style);
            }
            let mut subtitle_spans = Vec::new();
            if !row.compact && (show_source || playing) {
                subtitle_spans.push(Span::styled("    ", row_style));
            }
            if !row.compact && show_source && !row.source.is_empty() {
                subtitle_spans.push(Span::styled(&row.source, source_style));
                if !row.subtitle.is_empty() || !progress.is_empty() {
                    subtitle_spans.push(Span::styled(" · ", row_style));
                }
            }
            if !row.subtitle.is_empty() {
                if let Some(duration_end) = subtitle_duration_field_end(&row.subtitle)
                    && !progress.is_empty()
                {
                    subtitle_spans
                        .push(Span::styled(&row.subtitle[..duration_end], secondary_style));
                    subtitle_spans.push(Span::styled(progress, secondary_style));
                    subtitle_spans
                        .push(Span::styled(&row.subtitle[duration_end..], secondary_style));
                } else {
                    subtitle_spans.push(Span::styled(&row.subtitle, secondary_style));
                    if !progress.is_empty() {
                        subtitle_spans.push(Span::styled(progress, secondary_style));
                    }
                }
            } else if !progress.is_empty() {
                subtitle_spans.push(Span::styled(
                    progress.trim_start().to_owned(),
                    secondary_style,
                ));
            }
            let subtitle = Line::from(subtitle_spans);
            ListItem::new(vec![line, subtitle]).style(row_style)
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items), rows_area);
    (rows_area, first_index)
}

/// Returns the uniform physical height for a homogeneous compact list.
fn row_list_height(rows: &[RowView]) -> u16 {
    if !rows.is_empty() && rows.iter().all(|row| row.compact) {
        1
    } else {
        2
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the renderer receives the same explicit dependencies as the normal body"
)]
fn render_subscriptions_body(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ViewModel,
    show_hotkeys: bool,
    thumbnail_height: u16,
    theme: &Theme,
    hit_map: &mut HitMap,
    thumbnail_renderer: Option<&mut dyn ThumbnailRenderer>,
) {
    let subscriptions = &view.subscriptions;
    match subscriptions.layout {
        SubscriptionsLayout::DrillDown if subscriptions.route == SubscriptionRoute::Items => {
            let panes = Layout::default()
                .direction(if area.width >= 80 {
                    Direction::Horizontal
                } else {
                    Direction::Vertical
                })
                .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
                .split(area);
            let list_sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(panes[0]);
            (
                hit_map.subscription_item_rows,
                hit_map.subscription_item_first_index,
            ) = render_row_list(
                frame,
                list_sections[0],
                &subscription_items_heading(subscriptions),
                &subscriptions.items,
                false,
                subscriptions.selected_item,
                view.playing_media_id.as_ref(),
                view.playback.paused,
                view.radio_recording
                    .as_ref()
                    .map(|recording| recording.station_id.as_str()),
                !view.physical_linux_console,
                theme.heading,
                theme,
            );
            render_subscription_item_buttons(
                frame,
                list_sections[1],
                false,
                false,
                subscriptions.loading,
                view.search_animation_frame,
                subscriptions.source_kind,
                show_hotkeys,
                theme,
                hit_map,
            );
            render_details(
                frame,
                panes[1],
                view,
                show_hotkeys,
                thumbnail_height,
                theme,
                hit_map,
                thumbnail_renderer,
            );
        }
        SubscriptionsLayout::DrillDown => {
            let panes = Layout::default()
                .direction(if area.width >= 80 {
                    Direction::Horizontal
                } else {
                    Direction::Vertical
                })
                .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
                .split(area);
            (
                hit_map.subscription_source_rows,
                hit_map.subscription_source_first_index,
            ) = render_subscription_source_list(
                frame,
                panes[0],
                view,
                show_hotkeys,
                theme.heading,
                theme,
                hit_map,
            );
            render_channel(
                frame,
                panes[1],
                view,
                show_hotkeys,
                thumbnail_height,
                theme,
                hit_map,
                thumbnail_renderer,
            );
        }
        SubscriptionsLayout::Split => {
            let panes = Layout::default()
                .direction(if area.width >= 80 {
                    Direction::Horizontal
                } else {
                    Direction::Vertical
                })
                .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
                .split(area);
            let source_heading = if subscriptions.focus == SubscriptionPane::Sources {
                theme
                    .accent
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                theme.heading
            };
            (
                hit_map.subscription_source_rows,
                hit_map.subscription_source_first_index,
            ) = render_subscription_source_list(
                frame,
                panes[0],
                view,
                show_hotkeys,
                source_heading,
                theme,
                hit_map,
            );
            if subscriptions.description_expanded {
                render_details(
                    frame,
                    panes[1],
                    view,
                    show_hotkeys,
                    thumbnail_height,
                    theme,
                    hit_map,
                    thumbnail_renderer,
                );
                let footer = Rect::new(
                    panes[1].x,
                    panes[1].bottom().saturating_sub(1),
                    panes[1].width,
                    if panes[1].height > 0 { 1 } else { 0 },
                );
                render_subscription_item_buttons(
                    frame,
                    footer,
                    true,
                    true,
                    subscriptions.loading,
                    view.search_animation_frame,
                    subscriptions.source_kind,
                    show_hotkeys,
                    theme,
                    hit_map,
                );
            } else {
                hit_map.information_panel_identity = None;
                if let Some(renderer) = thumbnail_renderer {
                    renderer.clear();
                }
                let item_heading = if subscriptions.focus == SubscriptionPane::Items {
                    theme
                        .accent
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    theme.heading
                };
                let sections = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(1), Constraint::Length(1)])
                    .split(panes[1]);
                let heading = subscription_items_heading(subscriptions);
                (
                    hit_map.subscription_item_rows,
                    hit_map.subscription_item_first_index,
                ) = render_row_list(
                    frame,
                    sections[0],
                    &heading,
                    &subscriptions.items,
                    false,
                    subscriptions.selected_item,
                    view.playing_media_id.as_ref(),
                    view.playback.paused,
                    view.radio_recording
                        .as_ref()
                        .map(|recording| recording.station_id.as_str()),
                    !view.physical_linux_console,
                    item_heading,
                    theme,
                );
                render_subscription_item_buttons(
                    frame,
                    sections[1],
                    !subscriptions.items.is_empty(),
                    false,
                    subscriptions.loading,
                    view.search_animation_frame,
                    subscriptions.source_kind,
                    show_hotkeys,
                    theme,
                    hit_map,
                );
            }
        }
    }
}

/// Renders the source list and its non-row RSS subscription control.
fn render_subscription_source_list(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ViewModel,
    show_hotkeys: bool,
    heading_style: Style,
    theme: &Theme,
    hit_map: &mut HitMap,
) -> (Rect, usize) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let rendered = render_row_list(
        frame,
        sections[0],
        "Sources",
        &view.subscriptions.sources,
        true,
        view.subscriptions.selected_source,
        view.playing_media_id.as_ref(),
        view.playback.paused,
        view.radio_recording
            .as_ref()
            .map(|recording| recording.station_id.as_str()),
        !view.physical_linux_console,
        heading_style,
        theme,
    );
    if !sections[1].is_empty() {
        let label = button("a", "Add RSS feed", show_hotkeys);
        let width = terminal_text_width(&label).min(sections[1].width);
        if width > 0 {
            let target = Rect::new(sections[1].x, sections[1].y, width, 1);
            frame.render_widget(Paragraph::new(label).style(theme.accent), target);
            hit_map
                .subscription_source_buttons
                .push((UiAction::OpenRssSubscriptionPopup, target));
        }
    }
    rendered
}

/// Builds the source-aware item heading for both subscription layouts.
fn subscription_items_heading(subscriptions: &SubscriptionsView) -> String {
    let provider = match subscriptions.source_kind {
        SubscriptionKind::YouTube => "YouTube",
        SubscriptionKind::Rss => "RSS/Atom",
        SubscriptionKind::Other => "Subscription",
    };
    let mut heading = if subscriptions.source_title.is_empty() {
        provider.to_owned()
    } else {
        format!("{} · {provider}", subscriptions.source_title)
    };
    if subscriptions.source_kind == SubscriptionKind::YouTube
        && let Some(count) = subscriptions.source_subscriber_count
    {
        heading.push_str(" · ");
        heading.push_str(&format_count(count));
        heading.push_str(" subscribers");
    }
    if subscriptions.source_kind == SubscriptionKind::YouTube
        && !subscriptions.source_created.is_empty()
    {
        heading.push_str(" · created ");
        heading.push_str(&subscriptions.source_created);
    }
    heading
}

fn render_subscription_item_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    description_available: bool,
    description_expanded: bool,
    loading: bool,
    animation_frame: usize,
    source_kind: SubscriptionKind,
    show_hotkeys: bool,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    if area.is_empty() {
        return;
    }
    let refresh_label = if loading {
        let frame = ASCII_ACTIVITY_FRAMES[animation_frame % ASCII_ACTIVITY_FRAMES.len()];
        format!("Refresh {} {frame}", subscription_item_noun(source_kind))
    } else {
        format!("Refresh {}", subscription_item_noun(source_kind))
    };
    let refresh = (
        button("R", &refresh_label, show_hotkeys),
        UiAction::RefreshSubscriptionVideos,
    );
    let description = description_available.then(|| {
        (
            button(
                if description_expanded { "i/Esc" } else { "i" },
                if description_expanded {
                    match source_kind {
                        SubscriptionKind::Rss => "Back to episodes",
                        _ => "Back to videos",
                    }
                } else {
                    "Details"
                },
                show_hotkeys,
            ),
            UiAction::ToggleSubscriptionDescription,
        )
    });
    let mut buttons = Vec::with_capacity(2);
    if description_expanded && let Some(description) = description.clone() {
        buttons.push(description);
    }
    buttons.push(refresh);
    if !description_expanded && let Some(description) = description {
        buttons.push(description);
    }
    let mut x = area.x;
    for (label, action) in buttons {
        let available = area.right().saturating_sub(x);
        let width = terminal_text_width(&label).min(available);
        if width == 0 {
            break;
        }
        let target = Rect::new(x, area.y, width, 1);
        frame.render_widget(Paragraph::new(label).style(theme.accent), target);
        hit_map.detail_buttons.push((action, target));
        x = x.saturating_add(width).saturating_add(2);
    }
}

/// Returns the plural item noun used by one subscription source.
const fn subscription_item_noun(source_kind: SubscriptionKind) -> &'static str {
    match source_kind {
        SubscriptionKind::Rss => "episodes",
        _ => "videos",
    }
}

fn render_details(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ViewModel,
    show_hotkeys: bool,
    thumbnail_height: u16,
    theme: &Theme,
    hit_map: &mut HitMap,
    thumbnail_renderer: Option<&mut dyn ThumbnailRenderer>,
) {
    let terminal_window = thumbnail_renderer
        .as_ref()
        .is_some_and(|renderer| renderer.is_enabled())
        .then(current_terminal_window_metrics)
        .flatten();
    render_details_with_terminal_window(
        frame,
        area,
        view,
        show_hotkeys,
        thumbnail_height,
        terminal_window,
        theme,
        hit_map,
        thumbnail_renderer,
    );
}

#[allow(
    clippy::too_many_arguments,
    reason = "the testable details renderer keeps terminal-window metrics explicit"
)]
fn render_details_with_terminal_window(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ViewModel,
    show_hotkeys: bool,
    configured_thumbnail_height: u16,
    terminal_window: Option<TerminalWindowMetrics>,
    theme: &Theme,
    hit_map: &mut HitMap,
    thumbnail_renderer: Option<&mut dyn ThumbnailRenderer>,
) {
    let empty_message = if completed_search_has_no_rows(view) {
        "Nothing found"
    } else {
        "Select an item to load details lazily."
    };
    render_information_panel(
        frame,
        area,
        view,
        show_hotkeys,
        theme,
        hit_map,
        "",
        empty_message,
        view.screen.details_kind(),
        true,
        ThumbnailSizing::adaptive_youtube(configured_thumbnail_height, terminal_window),
        thumbnail_renderer,
    );
}

/// Distinguishes a completed empty result set from a screen awaiting input.
fn completed_search_has_no_rows(view: &ViewModel) -> bool {
    if view.search_editing
        || view.search_activity.is_some()
        || !view.rows.is_empty()
        || view.search_query.trim().is_empty()
    {
        return false;
    }
    match view.screen {
        Screen::YandexMusic => view.yandex_music_route == YandexMusicRouteView::Search,
        Screen::Search
        | Screen::YouTubeMusic
        | Screen::Bandcamp
        | Screen::ApplePodcasts
        | Screen::LibriVox
        | Screen::TrackerMusic
        | Screen::Radio => true,
        Screen::Subscriptions
        | Screen::Local
        | Screen::Playlists
        | Screen::Downloaded
        | Screen::History
        | Screen::Statistics => false,
    }
}

/// Returns a real YouTube `@handle` carried by a safe channel URL.
///
/// Display names are handled separately so UI text can never be converted
/// into a guessed channel address.
fn youtube_channel_handle(url: Option<&url::Url>) -> Option<String> {
    let url = url?;
    if url.scheme() != "https"
        || !url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("youtube.com")
                || host.eq_ignore_ascii_case("www.youtube.com")
                || host.eq_ignore_ascii_case("m.youtube.com")
        })
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let mut segments = url
        .path_segments()?
        .map(decode_url_path_segment_once)
        .collect::<Option<Vec<_>>>()?;
    if segments.last().is_some_and(String::is_empty) {
        segments.pop();
    }
    let [handle] = segments.as_slice() else {
        return None;
    };
    handle
        .strip_prefix('@')
        .is_some_and(valid_youtube_channel_display_alias)
        .then(|| handle.clone())
}

/// Checks a decoded handle before presenting it as an actionable channel URL.
fn valid_youtube_channel_display_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= 128
        && !matches!(alias, "." | "..")
        && !alias.chars().any(|character| {
            character.is_control()
                || character.is_whitespace()
                || matches!(character, '/' | '\\' | '?' | '#' | '%' | '@' | ':')
        })
}

/// Thumbnail sizing inputs retained until the responsive image width is known.
#[derive(Clone, Copy)]
struct ThumbnailSizing {
    /// User-configured height used when adaptive YouTube sizing does not apply.
    configured_height: u16,
    /// Whether YouTube artwork may expand from its rendered column width.
    adaptive_youtube: bool,
    /// Current terminal cell and pixel geometry, when the terminal reports it.
    terminal_window: Option<TerminalWindowMetrics>,
}

impl ThumbnailSizing {
    /// Creates fixed sizing for channels and other non-video information panes.
    const fn fixed(configured_height: u16) -> Self {
        Self {
            configured_height,
            adaptive_youtube: false,
            terminal_window: None,
        }
    }

    /// Creates responsive sizing for media Details panes.
    const fn adaptive_youtube(
        configured_height: u16,
        terminal_window: Option<TerminalWindowMetrics>,
    ) -> Self {
        Self {
            configured_height,
            adaptive_youtube: true,
            terminal_window,
        }
    }

    /// Resolves the preferred row count after the artwork column is known.
    fn preferred_height(
        self,
        details: &DetailView,
        artwork_width: u16,
        description_width: u16,
    ) -> u16 {
        let height = youtube_thumbnail_height(
            self.configured_height,
            artwork_width,
            self.adaptive_youtube && details.source == "YouTube",
            self.adaptive_youtube && youtube_description_is_short(details, description_width),
            details.thumbnail_dimensions,
            self.terminal_window,
        );
        if details.source.starts_with("Yandex Music") {
            yandex_music_thumbnail_height(
                height,
                artwork_width,
                details.thumbnail_dimensions,
                self.terminal_window,
            )
        } else if details.source == "Local image" {
            height.saturating_mul(2)
        } else {
            height
        }
    }
}

/// One control and its current compact-layout placement.
#[derive(Clone)]
struct DetailButtonPlacement {
    /// Zero-based metadata row containing the rendered label.
    line_index: usize,
    /// Terminal-cell offset from the panel's left edge.
    column: u16,
    /// Exact rendered label used to size the mouse target.
    label: String,
    /// Exact style retained when a left-side action later shares this row.
    style: Style,
    /// Action dispatched by a click inside the label.
    action: UiAction,
}

/// Responsive action column rendered beside artwork.
struct DetailActionRail {
    /// Controls in deterministic visual and keyboard order.
    buttons: Vec<DetailButtonPlacement>,
    /// Stable column width across labels whose text changes when toggled.
    width: u16,
    /// Row count including one empty row between adjacent labels.
    height: u16,
    /// Width remaining for artwork to the left of the rail.
    artwork_width: u16,
    /// Requested artwork-block height after fitting the available pane.
    ///
    /// The decoded image may aspect-fit to fewer rows. Keeping the rail tied
    /// to this stable block avoids moving controls when asynchronous artwork
    /// replaces its loading placeholder.
    artwork_height: u16,
}

/// Empty columns between actual artwork and its action rail.
const DETAIL_ACTION_RAIL_GUTTER: u16 = 2;

/// Smallest useful artwork column when controls move beside it.
///
/// Keeping 48 terminal cells prevents a side rail from making standard
/// media artwork harder to inspect on narrow information panes.
const MIN_DETAIL_ACTION_RAIL_ARTWORK_WIDTH: u16 = 48;

/// Returns a stable rail slot width for labels that change after activation.
fn detail_button_layout_width(button_placement: &DetailButtonPlacement, show_hotkeys: bool) -> u16 {
    let stable_label = match &button_placement.action {
        UiAction::ToggleRadioFavorite => Some(button("f", "Unfavorite", show_hotkeys)),
        UiAction::ToggleTodoPlaylist => Some(button("l", "Remove from todo", show_hotkeys)),
        UiAction::EditPrivateNote => Some(button("n", "Edit private note", show_hotkeys)),
        UiAction::ToggleSubscription => Some(button("s", "Unsubscribe (locally)", show_hotkeys)),
        UiAction::ToggleRadioRecording => Some(button("r", "Stop recording", show_hotkeys)),
        UiAction::FingerprintLocalAudio => Some(button("f", "Fingerprinting |", show_hotkeys)),
        UiAction::ToggleYandexMusicLike => Some(button("L", "Remove like", show_hotkeys)),
        UiAction::ToggleYandexMusicDislike => Some(button("X", "Remove dislike", show_hotkeys)),
        _ => None,
    };
    stable_label.as_deref().map_or_else(
        || terminal_text_width(&button_placement.label),
        |label| terminal_text_width(&button_placement.label).max(terminal_text_width(label)),
    )
}

/// Chooses a spaced side rail only when every control and useful artwork fit.
#[allow(
    clippy::too_many_arguments,
    reason = "the pure geometry helper keeps responsive layout inputs explicit"
)]
fn detail_action_rail(
    buttons: &[DetailButtonPlacement],
    panel_width: u16,
    panel_height: u16,
    metadata_height: u16,
    text_reserve: u16,
    sizing: ThumbnailSizing,
    details: &DetailView,
    show_hotkeys: bool,
) -> Option<DetailActionRail> {
    if buttons.is_empty() {
        return None;
    }
    let width = buttons
        .iter()
        .map(|button| detail_button_layout_width(button, show_hotkeys))
        .max()
        .unwrap_or_default();
    let artwork_width = panel_width.checked_sub(width.saturating_add(DETAIL_ACTION_RAIL_GUTTER))?;
    if width == 0 || artwork_width < MIN_DETAIL_ACTION_RAIL_ARTWORK_WIDTH {
        return None;
    }
    let button_count = u16::try_from(buttons.len()).unwrap_or(u16::MAX);
    let height = button_count.saturating_mul(2).saturating_sub(1);
    let available_height = panel_height
        .saturating_sub(metadata_height)
        .saturating_sub(text_reserve);
    let artwork_height = sizing
        .preferred_height(details, artwork_width, panel_width)
        .min(available_height);
    (artwork_height >= MIN_THUMBNAIL_HEIGHT && height <= artwork_height).then(|| {
        let mut buttons = buttons.to_vec();
        buttons.sort_by_key(|button| (button.line_index, button.column));
        DetailActionRail {
            buttons,
            width,
            height,
            artwork_width,
            artwork_height,
        }
    })
}

/// Builds the stable renderer identity used for terminal-cell invalidation.
fn information_panel_identity(
    area: Rect,
    kind: InformationPanelKind,
    title: &str,
    show_text_selection: bool,
    details: Option<&DetailView>,
) -> InformationPanelIdentity {
    let owner = details.map_or(InformationPanelOwner::Empty, |details| {
        if kind == InformationPanelKind::Channel && !details.channel_id.is_empty() {
            InformationPanelOwner::Channel(details.channel_id.clone())
        } else if let Some(media_id) = details.media_id.as_ref() {
            InformationPanelOwner::Media(media_id.clone())
        } else {
            InformationPanelOwner::Display {
                title: details.title.clone(),
                source: details.source.clone(),
            }
        }
    });
    InformationPanelIdentity {
        area,
        kind,
        title: title.to_owned(),
        show_text_selection,
        owner,
    }
}

/// Forces one owned pane to rewrite every terminal cell on its next flush.
///
/// A terminal may measure an emoji differently from Ratatui. In that case the
/// physical trailing character can occupy a cell that Ratatui already models
/// as blank, so an ordinary buffer clear produces no diff. `AlwaysUpdate`
/// repairs that divergence without clearing or repainting the whole screen.
fn invalidate_terminal_area(frame: &mut Frame<'_>, area: Rect) {
    let area = area.intersection(frame.area());
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            frame.buffer_mut()[(x, y)].set_diff_option(CellDiffOption::AlwaysUpdate);
        }
    }
}

/// Appends one clipped, right-aligned control and remembers its hit target.
fn push_right_detail_button<'a>(
    lines: &mut Vec<Line<'a>>,
    buttons: &mut Vec<DetailButtonPlacement>,
    panel_width: u16,
    label: String,
    style: Style,
    action: UiAction,
) {
    let rendered_width = terminal_text_width(&label).min(panel_width);
    let column = panel_width.saturating_sub(rendered_width);
    let line_index = lines.len();
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(usize::from(column))),
        Span::styled(label.clone(), style),
    ]));
    buttons.push(DetailButtonPlacement {
        line_index,
        column,
        label,
        style,
        action,
    });
}

/// Places a left-side action in the earliest ordered space before a right control.
///
/// Pairing the two columns keeps actions near the top without overlapping on
/// narrow panes. The monotonic row cursor preserves action order; once no
/// remaining right-control row has enough room, the action and its successors
/// receive appended rows.
fn push_left_detail_button<'a>(
    lines: &mut Vec<Line<'a>>,
    right_buttons: &[DetailButtonPlacement],
    next_left_row: &mut usize,
    panel_width: u16,
    label: String,
    style: Style,
    action: UiAction,
) -> DetailButtonPlacement {
    let label_width = terminal_text_width(&label).min(panel_width);
    let shared_row = right_buttons.iter().find(|button| {
        button.line_index >= *next_left_row && button.column >= label_width.saturating_add(2)
    });
    let line_index = if let Some(button) = shared_row {
        let gap = button.column.saturating_sub(label_width);
        lines[button.line_index] = Line::from(vec![
            Span::styled(label.clone(), style),
            Span::raw(" ".repeat(usize::from(gap))),
            Span::styled(button.label.clone(), button.style),
        ]);
        button.line_index
    } else {
        let line_index = lines.len();
        lines.push(Line::styled(label.clone(), style));
        line_index
    };
    *next_left_row = line_index.saturating_add(1);
    DetailButtonPlacement {
        line_index,
        column: 0,
        label,
        style,
        action,
    }
}

fn render_information_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ViewModel,
    show_hotkeys: bool,
    theme: &Theme,
    hit_map: &mut HitMap,
    title: &str,
    empty_message: &str,
    kind: InformationPanelKind,
    show_text_selection: bool,
    thumbnail_sizing: ThumbnailSizing,
    mut thumbnail_renderer: Option<&mut dyn ThumbnailRenderer>,
) {
    hit_map.details_panel = area;
    let identity = information_panel_identity(
        area,
        kind,
        title,
        show_text_selection,
        view.details.as_ref(),
    );
    if hit_map.information_panel_identity.as_ref() != Some(&identity) {
        invalidate_terminal_area(frame, area);
        hit_map.information_panel_identity = Some(identity);
    }
    let heading_style = if view.details_focused {
        theme
            .accent
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        theme.heading
    };
    let inner = render_main_panel_heading(frame, area, title.trim(), heading_style);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let Some(details) = view.details.as_ref() else {
        frame.render_widget(
            Paragraph::new(empty_message)
                .style(theme.muted)
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    };

    let title_already_visible = if view.screen == Screen::Subscriptions {
        view.subscriptions.source_title == details.title
    } else {
        view.rows
            .get(view.selected)
            .is_some_and(|row| row.title == details.title)
    };
    let mut lines = if !show_text_selection && !title_already_visible {
        vec![Line::styled(&details.title, theme.heading)]
    } else {
        Vec::new()
    };
    let mut right_buttons = Vec::with_capacity(4);
    if show_text_selection
        && kind == InformationPanelKind::Video
        && view.video_comments_available
        && details
            .media_id
            .as_ref()
            .is_some_and(|media_id| media_id.source == SourceKind::YouTube)
    {
        push_right_detail_button(
            &mut lines,
            &mut right_buttons,
            inner.width,
            button("F6", "Twenty comments", show_hotkeys),
            theme.accent,
            UiAction::OpenVideoComments,
        );
    }
    if cfg!(feature = "local-trash") && view.screen == Screen::Downloaded {
        push_right_detail_button(
            &mut lines,
            &mut right_buttons,
            inner.width,
            button("x", "Move to Trash", show_hotkeys),
            theme.accent,
            UiAction::RequestDownloadedTrash,
        );
    }
    if view.external_opener_available
        && matches!(
            kind,
            InformationPanelKind::Video | InformationPanelKind::Channel
        )
        && details.channel_webpage_url.is_some()
    {
        let channel_label = youtube_channel_handle(details.channel_webpage_url.as_ref())
            .or_else(|| {
                (!details.channel_name.trim().is_empty())
                    .then(|| details.channel_name.trim().to_owned())
            })
            .or_else(|| (!details.title.trim().is_empty()).then(|| details.title.trim().to_owned()))
            .unwrap_or_else(|| "channel".to_owned());
        let label = button(
            "O",
            &format!("{} channel · {channel_label}", system_url_opener_name()),
            show_hotkeys,
        );
        push_right_detail_button(
            &mut lines,
            &mut right_buttons,
            inner.width,
            label,
            theme.accent,
            UiAction::OpenChannelInBrowser,
        );
    }
    if view.external_opener_available
        && show_text_selection
        && matches!(
            kind,
            InformationPanelKind::Video
                | InformationPanelKind::Podcast
                | InformationPanelKind::Audiobook
                | InformationPanelKind::Radio
                | InformationPanelKind::YandexMusic
        )
    {
        let opener_name = system_url_opener_name();
        let action_text = match kind {
            InformationPanelKind::Podcast => format!("{opener_name} podcast"),
            InformationPanelKind::Audiobook => format!("{opener_name} audiobook"),
            InformationPanelKind::Radio => details.channel_webpage_url.as_ref().map_or_else(
                || format!("{opener_name} station website"),
                |url| format!("{opener_name} · {url}"),
            ),
            InformationPanelKind::YandexMusic if view.yandex_music_actions.track_selected => {
                format!("{opener_name} track")
            }
            InformationPanelKind::YandexMusic => format!("{opener_name} item"),
            _ => format!("{opener_name} video"),
        };
        let label = button(
            if kind == InformationPanelKind::Radio {
                "O"
            } else {
                "o"
            },
            &action_text,
            show_hotkeys,
        );
        push_right_detail_button(
            &mut lines,
            &mut right_buttons,
            inner.width,
            label,
            theme.accent,
            UiAction::OpenInBrowser,
        );
    }
    if kind == InformationPanelKind::Radio {
        let active = view.radio_recording.is_some();
        push_right_detail_button(
            &mut lines,
            &mut right_buttons,
            inner.width,
            button(
                "r",
                if active { "Stop recording" } else { "Record" },
                show_hotkeys,
            ),
            if active {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                theme.accent
            },
            UiAction::ToggleRadioRecording,
        );
    }
    if kind == InformationPanelKind::Local && details.local_fingerprint_available {
        let action_text = if details.local_fingerprint_pending {
            let frame = ASCII_ACTIVITY_FRAMES
                [view.local_fingerprint_animation_frame % ASCII_ACTIVITY_FRAMES.len()];
            format!("Fingerprinting {frame}")
        } else {
            "Fingerprint".to_owned()
        };
        push_right_detail_button(
            &mut lines,
            &mut right_buttons,
            inner.width,
            button("f", &action_text, show_hotkeys),
            if details.local_fingerprint_pending {
                theme.selected
            } else {
                theme.accent
            },
            UiAction::FingerprintLocalAudio,
        );
    }
    if cfg!(feature = "local-move") && kind == InformationPanelKind::Local && details.local_movable
    {
        push_right_detail_button(
            &mut lines,
            &mut right_buttons,
            inner.width,
            button("m", "Move", show_hotkeys),
            theme.accent,
            UiAction::BeginLocalMove,
        );
    }
    if cfg!(feature = "local-trash")
        && kind == InformationPanelKind::Local
        && details.local_trashable
    {
        push_right_detail_button(
            &mut lines,
            &mut right_buttons,
            inner.width,
            button("Delete", "Move to Trash", show_hotkeys),
            theme.accent,
            UiAction::RequestLocalTrash,
        );
    }
    if kind == InformationPanelKind::Local {
        push_right_detail_button(
            &mut lines,
            &mut right_buttons,
            inner.width,
            button(
                "H",
                if view.show_all_local_files {
                    "Show all files"
                } else {
                    "Media files only"
                },
                show_hotkeys,
            ),
            if view.show_all_local_files {
                theme.selected
            } else {
                theme.accent
            },
            UiAction::ToggleLocalAllFiles,
        );
    }
    let mut next_left_row = 0;
    let radio_favorite_button = (kind == InformationPanelKind::Radio).then(|| {
        let label = button(
            "f",
            if details.radio_favorite {
                "Unfavorite"
            } else {
                "Favorite"
            },
            show_hotkeys,
        );
        push_left_detail_button(
            &mut lines,
            &right_buttons,
            &mut next_left_row,
            inner.width,
            label,
            if details.radio_favorite {
                theme.selected
            } else {
                theme.accent
            },
            UiAction::ToggleRadioFavorite,
        )
    });
    let yandex_like_button = (view.screen == Screen::YandexMusic
        && view.yandex_music_actions.track_selected)
        .then(|| {
            let active = view.yandex_music_actions.reaction == YandexMusicReactionView::Liked;
            push_left_detail_button(
                &mut lines,
                &right_buttons,
                &mut next_left_row,
                inner.width,
                button(
                    "L",
                    if active { "Remove like" } else { "Like" },
                    show_hotkeys,
                ),
                if active { theme.selected } else { theme.accent },
                UiAction::ToggleYandexMusicLike,
            )
        });
    let yandex_dislike_button = (view.screen == Screen::YandexMusic
        && view.yandex_music_actions.track_selected)
        .then(|| {
            let active = view.yandex_music_actions.reaction == YandexMusicReactionView::Disliked;
            push_left_detail_button(
                &mut lines,
                &right_buttons,
                &mut next_left_row,
                inner.width,
                button(
                    "X",
                    if active { "Remove dislike" } else { "Dislike" },
                    show_hotkeys,
                ),
                if active { theme.selected } else { theme.accent },
                UiAction::ToggleYandexMusicDislike,
            )
        });
    let yandex_artist_button = (view.screen == Screen::YandexMusic
        && view.yandex_music_actions.artist_available)
        .then(|| {
            push_left_detail_button(
                &mut lines,
                &right_buttons,
                &mut next_left_row,
                inner.width,
                button("g", "Open artist", show_hotkeys),
                theme.accent,
                UiAction::OpenYandexMusicArtist,
            )
        });
    let yandex_album_button = (view.screen == Screen::YandexMusic
        && view.yandex_music_actions.album_available)
        .then(|| {
            push_left_detail_button(
                &mut lines,
                &right_buttons,
                &mut next_left_row,
                inner.width,
                button("b", "Open album", show_hotkeys),
                theme.accent,
                UiAction::OpenYandexMusicAlbum,
            )
        });
    let yandex_download_album_button =
        (view.screen == Screen::YandexMusic && view.yandex_music_actions.album_open).then(|| {
            push_left_detail_button(
                &mut lines,
                &right_buttons,
                &mut next_left_row,
                inner.width,
                button("Shift+D", "Download album", show_hotkeys),
                theme.accent,
                UiAction::DownloadYandexMusicAlbum,
            )
        });
    let yandex_download_recommendations_button = (view.screen == Screen::YandexMusic
        && view.yandex_music_actions.twenty_recommendations_available)
        .then(|| {
            push_left_detail_button(
                &mut lines,
                &right_buttons,
                &mut next_left_row,
                inner.width,
                button("R", "Download 20 recommendations", show_hotkeys),
                theme.accent,
                UiAction::DownloadTwentyYandexMusicRecommendations,
            )
        });
    let details_playlist_item = view
        .playlist_item
        .as_ref()
        .filter(|item| details.media_id.as_ref() == Some(&item.media_id));
    let todo_button = details_playlist_item.map(|item| {
        let label = button(
            "l",
            if item.in_todo {
                "Remove from todo"
            } else {
                "Add to todo"
            },
            show_hotkeys,
        );
        push_left_detail_button(
            &mut lines,
            &right_buttons,
            &mut next_left_row,
            inner.width,
            label,
            if item.in_todo {
                theme.selected
            } else {
                theme.accent
            },
            UiAction::ToggleTodoPlaylist,
        )
    });
    let playlist_button = details_playlist_item.map(|_| {
        let label = button("P", "Playlist…", show_hotkeys);
        push_left_detail_button(
            &mut lines,
            &right_buttons,
            &mut next_left_row,
            inner.width,
            label,
            theme.accent,
            UiAction::OpenPlaylistPopup,
        )
    });
    let edit_playlist_button = (view.screen == Screen::Playlists && view.playlist_edit_available)
        .then(|| {
            let label = button("e", "Edit playlist", show_hotkeys);
            push_left_detail_button(
                &mut lines,
                &right_buttons,
                &mut next_left_row,
                inner.width,
                label,
                theme.accent,
                UiAction::EditSelectedPlaylist,
            )
        });
    let private_note_button = view.private_note_available.then(|| {
        let label = button(
            "n",
            if details.has_private_note {
                "Edit private note"
            } else {
                "Add private note"
            },
            show_hotkeys,
        );
        push_left_detail_button(
            &mut lines,
            &right_buttons,
            &mut next_left_row,
            inner.width,
            label,
            if details.has_private_note {
                theme.selected
            } else {
                theme.accent
            },
            UiAction::EditPrivateNote,
        )
    });
    let subscription_button = (!details.channel_id.is_empty()).then(|| {
        let label = button(
            "s",
            if details.channel_subscribed {
                "Unsubscribe (locally)"
            } else {
                "Subscribe (locally)"
            },
            show_hotkeys,
        );
        push_left_detail_button(
            &mut lines,
            &right_buttons,
            &mut next_left_row,
            inner.width,
            label,
            theme.accent,
            UiAction::ToggleSubscription,
        )
    });
    let rename_button = (cfg!(feature = "local-rename")
        && kind == InformationPanelKind::Local
        && details.local_renamable)
        .then(|| {
            let label = button("r", "Rename", show_hotkeys);
            push_left_detail_button(
                &mut lines,
                &right_buttons,
                &mut next_left_row,
                inner.width,
                label,
                theme.accent,
                UiAction::BeginLocalRename,
            )
        });
    match kind {
        InformationPanelKind::Video => {
            let mut spans = Vec::new();
            for (name, value) in [
                ("Length", details.length.as_str()),
                ("Views", details.views.as_str()),
                ("Likes", details.likes.as_str()),
                ("Comments", details.comments.as_str()),
            ] {
                let value = value.trim();
                if value.is_empty() || value.eq_ignore_ascii_case("unknown") {
                    continue;
                }
                if !spans.is_empty() {
                    spans.push(Span::raw("  "));
                }
                spans.push(Span::styled(format!("{name}: "), theme.muted));
                spans.push(Span::raw(value.to_owned()));
            }
            if !spans.is_empty() {
                lines.push(Line::from(spans));
            }
        }
        InformationPanelKind::Podcast | InformationPanelKind::Audiobook => {
            if !details.length.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("Length: ", theme.muted),
                    Span::raw(&details.length),
                ]));
            }
        }
        InformationPanelKind::YandexMusic => {}
        InformationPanelKind::Channel => {
            if let Some(count) = details.channel_subscriber_count {
                lines.push(Line::from(vec![
                    Span::styled("Subscribers: ", theme.muted),
                    Span::raw(format_count(count)),
                ]));
            }
            if !details.channel_created.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("Joined: ", theme.muted),
                    Span::raw(&details.channel_created),
                ]));
            }
            if let Some(count) = details.channel_video_count {
                lines.push(Line::from(vec![
                    Span::styled("Videos: ", theme.muted),
                    Span::raw(format_count(count)),
                ]));
            }
            if let Some(count) = details.channel_total_view_count {
                lines.push(Line::from(vec![
                    Span::styled("Total views: ", theme.muted),
                    Span::raw(format_count(count)),
                ]));
            }
            if !details.channel_country.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("Country: ", theme.muted),
                    Span::raw(&details.channel_country),
                ]));
            }
            if details.channel_links_truncated {
                lines.push(Line::styled(
                    "Some channel links were omitted by safety limits.",
                    theme.muted,
                ));
            }
        }
        InformationPanelKind::Radio => {}
        InformationPanelKind::Local => {
            if !details.length.is_empty() && details.length != "unknown" {
                lines.push(Line::from(vec![
                    Span::styled("Length: ", theme.muted),
                    Span::raw(&details.length),
                ]));
            }
        }
        InformationPanelKind::Generic => {}
    }
    if show_text_selection && !details.playlist_names.is_empty() {
        let summary = format!("Playlists: {}", details.playlist_names.join(", "));
        lines.extend(
            wrap_text_lines(&summary, inner.width)
                .into_iter()
                .map(Line::raw),
        );
    }
    if is_creative_commons_license(&details.license)
        || is_librivox_public_domain_license(&details.source, &details.license)
    {
        lines.push(Line::from(vec![
            Span::styled("License: ", theme.muted),
            Span::styled(display_license_label(&details.license), theme.accent),
        ]));
    }
    let mut detail_buttons = [
        radio_favorite_button,
        yandex_like_button,
        yandex_dislike_button,
        yandex_artist_button,
        yandex_album_button,
        yandex_download_album_button,
        yandex_download_recommendations_button,
        todo_button,
        playlist_button,
        edit_playlist_button,
        private_note_button,
        subscription_button,
        rename_button,
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    // Preserve the compact layout's established left-actions-first hit-map
    // order. The side rail independently sorts by visual row and column.
    detail_buttons.extend(right_buttons);
    let expanded_wikidata_entity = details
        .expanded_wikidata_item
        .as_deref()
        .and_then(|item_id| {
            details
                .wikidata_entities
                .iter()
                .find(|entity| entity.item_id == item_id)
        });
    let visible_thumbnail_url = details
        .thumbnail_url
        .as_ref()
        .or_else(|| expanded_wikidata_entity.and_then(|entity| entity.image_url.as_ref()));
    let visible_local_video = details.local_video_thumbnail.as_ref();
    let text_reserve = if details.thumbnail_expanded {
        0
    } else {
        u16::from(!details.description.is_empty()) + u16::from(!details.links.is_empty())
    };
    let compact_metadata_height = u16::try_from(
        lines
            .iter()
            .enumerate()
            .filter(|(line_index, _)| {
                !detail_buttons
                    .iter()
                    .any(|button| button.line_index == *line_index)
            })
            .count(),
    )
    .unwrap_or(u16::MAX)
    .min(inner.height);
    let side_rail = (!details.thumbnail_expanded
        && thumbnail_renderer
            .as_ref()
            .is_some_and(|renderer| renderer.is_enabled())
        && (visible_thumbnail_url.is_some() || visible_local_video.is_some()))
    .then(|| {
        detail_action_rail(
            &detail_buttons,
            inner.width,
            inner.height,
            compact_metadata_height,
            text_reserve,
            thumbnail_sizing,
            details,
            show_hotkeys,
        )
    })
    .flatten();
    if side_rail.is_some() {
        lines = lines
            .into_iter()
            .enumerate()
            .filter_map(|(line_index, line)| {
                (!detail_buttons
                    .iter()
                    .any(|button| button.line_index == line_index))
                .then_some(line)
            })
            .collect();
    }
    let metadata_height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .min(inner.height);
    let metadata_area = Rect::new(inner.x, inner.y, inner.width, metadata_height);
    // Metadata uses one terminal row per field so the subscription button's
    // mouse target remains stable even when a title or channel name is long.
    frame.render_widget(Paragraph::new(lines), metadata_area);
    if show_text_selection {
        for line_index in 0..usize::from(metadata_height) {
            if side_rail.is_none()
                && detail_buttons
                    .iter()
                    .any(|button| button.line_index == line_index)
            {
                continue;
            }
            capture_selectable_details_row(
                frame,
                hit_map,
                Rect::new(
                    metadata_area.x,
                    metadata_area.y.saturating_add(
                        u16::try_from(line_index).unwrap_or(metadata_height.saturating_sub(1)),
                    ),
                    metadata_area.width,
                    1,
                ),
            );
        }
    }
    if side_rail.is_none() {
        for button in &detail_buttons {
            if button.line_index >= usize::from(metadata_height) {
                continue;
            }
            let width =
                terminal_text_width(&button.label).min(inner.width.saturating_sub(button.column));
            if width > 0 {
                hit_map.detail_buttons.push((
                    button.action.clone(),
                    Rect::new(
                        inner.x.saturating_add(button.column),
                        inner.y.saturating_add(
                            u16::try_from(button.line_index)
                                .unwrap_or(metadata_height.saturating_sub(1)),
                        ),
                        width,
                        1,
                    ),
                ));
            }
        }
    }

    let mut cursor_y = metadata_area.bottom();
    let mut remaining_height = inner.bottom().saturating_sub(cursor_y);
    if let Some(renderer) = thumbnail_renderer.as_mut() {
        let artwork_width = side_rail
            .as_ref()
            .map_or(inner.width, |rail| rail.artwork_width);
        let preferred_thumbnail_height = side_rail.as_ref().map_or_else(
            || thumbnail_sizing.preferred_height(details, artwork_width, inner.width),
            |rail| rail.artwork_height,
        );
        let rendered_thumbnail_height = if details.thumbnail_expanded {
            remaining_height
        } else {
            remaining_height
                .saturating_sub(text_reserve)
                .min(preferred_thumbnail_height)
        };
        if renderer.is_enabled()
            && (visible_thumbnail_url.is_some() || visible_local_video.is_some())
            && rendered_thumbnail_height >= MIN_THUMBNAIL_HEIGHT
        {
            let available_thumbnail_area =
                Rect::new(inner.x, cursor_y, artwork_width, rendered_thumbnail_height);
            if let Some(local_video) = visible_local_video {
                renderer.synchronize_local_video(local_video, available_thumbnail_area);
            } else {
                renderer.synchronize(visible_thumbnail_url, available_thumbnail_area);
            }
            // The worker aspect-fits and encodes ready artwork for this exact
            // area. Rendering into that same rectangle prevents
            // `ratatui-image` from repeating resize/encode work on the TUI
            // thread. Loading and failure placeholders retain the requested
            // area. Expanded artwork still reserves every remaining row so it
            // continues to hide the following Details text.
            let thumbnail_area = renderer
                .prepared_artwork_area(available_thumbnail_area)
                .unwrap_or(available_thumbnail_area);
            let mut reserved_thumbnail_height = if details.thumbnail_expanded {
                available_thumbnail_area.height
            } else {
                thumbnail_area.height
            };
            let artwork_rendered = renderer.has_rendered_artwork();
            renderer.render(frame, thumbnail_area, theme);
            if artwork_rendered {
                hit_map.thumbnail_area = Some(thumbnail_area);
            }
            if let Some(rail) = side_rail.as_ref() {
                let rail_x = inner.right().saturating_sub(rail.width);
                for (index, button) in rail.buttons.iter().enumerate() {
                    let row_offset = u16::try_from(index).unwrap_or(u16::MAX).saturating_mul(2);
                    let button_area =
                        Rect::new(rail_x, cursor_y.saturating_add(row_offset), rail.width, 1);
                    frame.render_widget(
                        Paragraph::new(Line::styled(&button.label, button.style)),
                        button_area,
                    );
                    let width = terminal_text_width(&button.label).min(button_area.width);
                    if width > 0 {
                        hit_map.detail_buttons.push((
                            button.action.clone(),
                            Rect::new(rail_x, button_area.y, width, 1),
                        ));
                    }
                }
                // Keep text below both the decoded pixels and the stable rail.
                // This avoids a layout jump when a panoramic image finishes
                // loading into fewer rows than its requested artwork block.
                reserved_thumbnail_height = reserved_thumbnail_height.max(rail.height);
            }
            cursor_y = cursor_y.saturating_add(reserved_thumbnail_height);
            if !details.thumbnail_expanded && cursor_y < inner.bottom() {
                // Keep following metadata visually separate from the artwork.
                // Reserving the row for loading placeholders too prevents a
                // layout jump when decoded pixels replace the placeholder.
                cursor_y = cursor_y.saturating_add(1);
            }
            remaining_height = inner.bottom().saturating_sub(cursor_y);
        } else {
            renderer.clear();
        }
    }
    if !details.links.is_empty() && remaining_height > 0 {
        let description_reserve = if details.description.is_empty() {
            0
        } else {
            remaining_height.min(1)
        };
        let mut link_rows = Vec::with_capacity(details.links.len().saturating_mul(2));
        for (index, link) in details.links.iter().enumerate() {
            link_rows.push(Some((index, link)));
            let next_is_wikidata = details
                .links
                .get(index.saturating_add(1))
                .is_some_and(|next| next.wikidata_item_id.is_some());
            if !next_is_wikidata
                && matches!(
                    link.presentation,
                    DetailLinkPresentation::LabelAndUrlSpaced
                        | DetailLinkPresentation::LabelOnlySpaced
                        | DetailLinkPresentation::UrlOnlySpaced
                )
            {
                link_rows.push(None);
            }
        }
        let desired_link_height = u16::try_from(link_rows.len()).unwrap_or(u16::MAX);
        let link_height =
            desired_link_height.min(remaining_height.saturating_sub(description_reserve));
        if link_height > 0 {
            let selected_link = view
                .selected_detail_link
                .unwrap_or_default()
                .min(details.links.len().saturating_sub(1));
            let selected_row = link_rows
                .iter()
                .position(|row| {
                    row.as_ref()
                        .is_some_and(|(index, _)| *index == selected_link)
                })
                .unwrap_or_default();
            let first_row = selected_row
                .saturating_add(1)
                .saturating_sub(usize::from(link_height));
            for row in link_rows
                .iter()
                .skip(first_row)
                .take(usize::from(link_height))
            {
                let Some((index, link)) = row else {
                    cursor_y = cursor_y.saturating_add(1);
                    continue;
                };
                let link_area = Rect::new(inner.x, cursor_y, inner.width, 1);
                let mut content_offset = 0_u16;
                let mut spans = Vec::new();
                if let Some(item_id) = link.wikidata_item_id.as_deref() {
                    let expanded = details.expanded_wikidata_item.as_deref() == Some(item_id);
                    let disclosure = button("W", if expanded { "▾" } else { "▸" }, show_hotkeys);
                    let disclosure_width = terminal_text_width(&disclosure);
                    spans.push(Span::styled(
                        disclosure,
                        theme.accent.add_modifier(Modifier::BOLD),
                    ));
                    spans.push(Span::raw(" "));
                    content_offset =
                        content_offset.saturating_add(disclosure_width.saturating_add(1));
                    if disclosure_width > 0 {
                        hit_map.detail_buttons.push((
                            UiAction::ToggleWikidataStatements(*index),
                            Rect::new(
                                link_area.x,
                                link_area.y,
                                disclosure_width.min(link_area.width),
                                1,
                            ),
                        ));
                    }
                }
                if !link.prefix.is_empty() {
                    spans.push(Span::styled(&link.prefix, theme.base));
                    content_offset =
                        content_offset.saturating_add(terminal_text_width(&link.prefix));
                }
                let external_offset = content_offset;
                let external_width = match link.presentation {
                    DetailLinkPresentation::LabelAndUrl
                    | DetailLinkPresentation::LabelAndUrlSpaced => {
                        spans.extend([
                            Span::styled(&link.label, theme.base),
                            Span::styled(" — ", theme.muted),
                            Span::styled(&link.url, theme.muted),
                        ]);
                        terminal_text_width(&link.label)
                            .saturating_add(terminal_text_width(" — "))
                            .saturating_add(terminal_text_width(&link.url))
                    }
                    DetailLinkPresentation::LabelOnly | DetailLinkPresentation::LabelOnlySpaced => {
                        spans.push(Span::styled(
                            &link.label,
                            if view.external_opener_available {
                                theme.accent.add_modifier(Modifier::UNDERLINED)
                            } else {
                                theme.muted
                            },
                        ));
                        terminal_text_width(&link.label)
                    }
                    DetailLinkPresentation::UrlOnly | DetailLinkPresentation::UrlOnlySpaced => {
                        spans.push(Span::styled(
                            &link.url,
                            if view.external_opener_available {
                                theme.accent.add_modifier(Modifier::UNDERLINED)
                            } else {
                                theme.muted
                            },
                        ));
                        terminal_text_width(&link.url)
                    }
                };
                content_offset = content_offset.saturating_add(external_width);
                if let Some(target) = link.internal_target.as_ref() {
                    spans.push(Span::raw(" "));
                    content_offset = content_offset.saturating_add(1);
                    let marker = "↪";
                    let marker_width = terminal_text_width(marker);
                    spans.push(Span::styled(
                        marker,
                        theme.accent.add_modifier(Modifier::BOLD),
                    ));
                    let marker_offset = content_offset.min(link_area.width);
                    let marker_area = Rect::new(
                        link_area.x.saturating_add(marker_offset),
                        link_area.y,
                        marker_width.min(link_area.width.saturating_sub(marker_offset)),
                        1,
                    );
                    if marker_area.width > 0 {
                        hit_map.detail_buttons.push((target.action(), marker_area));
                    }
                }
                frame.render_widget(Paragraph::new(Line::from(spans)), link_area);
                if show_text_selection {
                    capture_selectable_details_row(frame, hit_map, link_area);
                }
                let clickable_offset = external_offset.min(link_area.width);
                let clickable_area = Rect::new(
                    link_area.x.saturating_add(clickable_offset),
                    link_area.y,
                    external_width.min(link_area.width.saturating_sub(clickable_offset)),
                    1,
                );
                if view.external_opener_available && clickable_area.width > 0 {
                    hit_map.detail_links.push((*index, clickable_area));
                }
                cursor_y = cursor_y.saturating_add(1);
            }
            remaining_height = inner.bottom().saturating_sub(cursor_y);
        }
    }

    let expanded_wikidata_text = details.expanded_wikidata_item.as_deref().map(|item_id| {
        expanded_wikidata_entity.map_or_else(
            || {
                if details.loading_wikidata_item.as_deref() == Some(item_id) {
                    "Loading Wikidata properties…"
                } else {
                    "Wikidata properties are unavailable."
                }
            },
            |entity| entity.text.as_str(),
        )
    });
    let lastfm_description = (!details.lastfm_artist_description.is_empty()).then(|| {
        if details.description.is_empty() {
            format!(
                "Last.fm artist description:\n{}",
                details.lastfm_artist_description
            )
        } else {
            format!(
                "{}\n\nLast.fm artist description:\n{}",
                details.description, details.lastfm_artist_description
            )
        }
    });
    let body_is_wikidata = expanded_wikidata_text.is_some();
    let body_source = expanded_wikidata_text
        .or_else(|| lastfm_description.as_deref())
        .unwrap_or(&details.description);
    let wikidata_value_links = expanded_wikidata_entity
        .map(|entity| entity.value_links.as_slice())
        .unwrap_or_default();
    let wikidata_media_controls = expanded_wikidata_entity
        .map(|entity| entity.media_controls.as_slice())
        .unwrap_or_default();
    if remaining_height > 0 && !body_source.is_empty() {
        let description_area = Rect::new(inner.x, cursor_y, inner.width, remaining_height);
        let (description_text_area, scrollbar_area) = if description_area.width > 1 {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(description_area);
            (columns[0], columns[1])
        } else {
            (description_area, Rect::default())
        };
        let description_lines = wrap_description_source(
            body_source,
            usize::from(description_text_area.width.max(1)),
            if body_is_wikidata {
                &[]
            } else {
                &details.video_links
            },
        );
        let visible_lines = usize::from(description_text_area.height);
        let maximum_offset = description_lines.len().saturating_sub(visible_lines);
        let offset = view.details_scroll.min(maximum_offset);
        hit_map.details_scroll_offset = offset;
        hit_map.details_scroll_maximum = maximum_offset;
        let visible = description_lines
            .iter()
            .skip(offset)
            .take(visible_lines)
            .enumerate();
        let active_chapter_line = (!body_is_wikidata)
            .then(|| active_description_chapter_line(view, details))
            .flatten();
        for (visible_index, source_line) in visible {
            let row = description_text_area.y.saturating_add(
                u16::try_from(visible_index).unwrap_or(description_text_area.height),
            );
            let mut spans = Vec::new();
            let mut cell_cursor = 0_u16;
            let active_line = active_chapter_line.as_ref().is_some_and(|active| {
                source_line.start_byte < active.end && source_line.end_byte > active.start
            });
            let line_style = if active_line {
                theme.active_chapter.add_modifier(Modifier::BOLD)
            } else {
                theme.base
            };
            for token in &source_line.tokens {
                match *token {
                    WrappedDescriptionToken::Source {
                        start_byte,
                        end_byte,
                    } => {
                        if body_is_wikidata {
                            append_wikidata_source_spans(
                                body_source,
                                wikidata_value_links,
                                wikidata_media_controls,
                                start_byte,
                                end_byte,
                                description_text_area,
                                row,
                                view.playing_media_id.as_ref(),
                                &view.playback,
                                view.selected_wikidata_media,
                                view.external_opener_available,
                                theme,
                                hit_map,
                                &mut spans,
                                &mut cell_cursor,
                            );
                        } else {
                            append_description_source_spans(
                                details,
                                body_source,
                                start_byte,
                                end_byte,
                                active_line,
                                description_text_area,
                                row,
                                theme,
                                hit_map,
                                &mut spans,
                                &mut cell_cursor,
                            );
                        }
                    }
                    WrappedDescriptionToken::VideoAction { link_index } => {
                        let Some(link) = details.video_links.get(link_index) else {
                            continue;
                        };
                        let action_width = terminal_text_width(DESCRIPTION_VIDEO_ACTION_SYMBOL);
                        spans.push(Span::styled(
                            DESCRIPTION_VIDEO_ACTION_SYMBOL,
                            theme
                                .accent
                                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                        ));
                        if action_width == 1 {
                            hit_map.description_video_actions.push((
                                UiAction::ActivateDescriptionVideo {
                                    video_id: link.video_id.clone(),
                                    start_seconds: link.start_seconds,
                                },
                                Rect::new(
                                    description_text_area.x.saturating_add(cell_cursor),
                                    row,
                                    1,
                                    1,
                                ),
                            ));
                        }
                        cell_cursor = cell_cursor.saturating_add(action_width);
                    }
                }
            }
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(line_style),
                Rect::new(description_text_area.x, row, description_text_area.width, 1),
            );
            if show_text_selection {
                capture_selectable_details_row(
                    frame,
                    hit_map,
                    Rect::new(description_text_area.x, row, description_text_area.width, 1),
                );
            }
        }
        if description_lines.len() > visible_lines
            && scrollbar_area.width > 0
            && scrollbar_area.height > 0
        {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .track_style(theme.muted)
                .thumb_symbol("█")
                .thumb_style(theme.accent);
            let mut scrollbar_state = ScrollbarState::new(maximum_offset.saturating_add(1))
                .position(offset)
                .viewport_content_length(visible_lines);
            frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
        }
    }
    if show_text_selection {
        highlight_details_text_selection(frame, view, hit_map, theme);
    }
}

/// Appends one expanded-Wikidata source token while preserving item links
/// across terminal wrapping.
#[allow(
    clippy::too_many_arguments,
    reason = "render geometry and mutable frame products belong to one row operation"
)]
fn append_wikidata_source_spans<'a>(
    source: &'a str,
    value_links: &[DetailWikidataValueLinkView],
    media_controls: &[DetailWikidataMediaView],
    start_byte: usize,
    end_byte: usize,
    description_area: Rect,
    row: u16,
    playing_media_id: Option<&MediaId>,
    playback: &PlaybackStatus,
    selected_media: Option<usize>,
    external_opener_available: bool,
    theme: &Theme,
    hit_map: &mut HitMap,
    spans: &mut Vec<Span<'a>>,
    cell_cursor: &mut u16,
) {
    let mut source_cursor = start_byte;
    let mut links = value_links
        .iter()
        .filter(|link| link.start_byte < end_byte && link.end_byte > start_byte)
        .peekable();
    let mut controls = media_controls
        .iter()
        .enumerate()
        .filter(|(_, control)| {
            control.marker_start_byte < end_byte && control.marker_end_byte > start_byte
        })
        .peekable();
    loop {
        let control_first = match (controls.peek(), links.peek()) {
            (Some((_, control)), Some(link)) => control.marker_start_byte <= link.start_byte,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        if control_first {
            let (index, control) = controls.next().expect("peeked control must remain");
            let start = control.marker_start_byte.max(start_byte);
            let end = control.marker_end_byte.min(end_byte);
            if source_cursor < start {
                let plain = &source[source_cursor..start];
                *cell_cursor = cell_cursor.saturating_add(terminal_text_width(plain));
                spans.push(Span::raw(plain));
            }
            let actively_playing =
                playing_media_id == Some(&control.media_id) && !playback.idle && !playback.paused;
            let marker = if actively_playing {
                WIKIDATA_MEDIA_PAUSE_SYMBOL
            } else {
                WIKIDATA_MEDIA_PLAY_SYMBOL
            };
            let marker_width = terminal_text_width(marker);
            let selected = selected_media == Some(index);
            spans.push(Span::styled(
                marker,
                if selected {
                    theme
                        .accent
                        .add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else {
                    theme.accent.add_modifier(Modifier::BOLD)
                },
            ));
            let available = description_area.width.saturating_sub(*cell_cursor);
            let target_width = marker_width.min(available);
            if target_width > 0 {
                hit_map.detail_buttons.push((
                    UiAction::ActivateWikidataMedia(index),
                    Rect::new(
                        description_area.x.saturating_add(*cell_cursor),
                        row,
                        target_width,
                        1,
                    ),
                ));
            }
            *cell_cursor = cell_cursor.saturating_add(marker_width);
            source_cursor = end;
        } else {
            let link = links.next().expect("peeked link must remain");
            let start = link.start_byte.max(start_byte);
            let end = link.end_byte.min(end_byte);
            if source_cursor < start {
                let plain = &source[source_cursor..start];
                *cell_cursor = cell_cursor.saturating_add(terminal_text_width(plain));
                spans.push(Span::raw(plain));
            }
            let linked = &source[start..end];
            let linked_width = terminal_text_width(linked);
            spans.push(Span::styled(
                linked,
                if external_opener_available {
                    theme
                        .accent
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    theme.muted
                },
            ));
            let available = description_area.width.saturating_sub(*cell_cursor);
            let target_width = linked_width.min(available);
            if external_opener_available && target_width > 0 {
                hit_map.detail_buttons.push((
                    UiAction::OpenWikidataValue(link.url.clone()),
                    Rect::new(
                        description_area.x.saturating_add(*cell_cursor),
                        row,
                        target_width,
                        1,
                    ),
                ));
            }
            *cell_cursor = cell_cursor.saturating_add(linked_width);
            source_cursor = end;
        }
    }
    if source_cursor < end_byte {
        let plain = &source[source_cursor..end_byte];
        *cell_cursor = cell_cursor.saturating_add(terminal_text_width(plain));
        spans.push(Span::raw(plain));
    }
}

/// Appends one source-text token while retaining clickable timecode spans.
#[allow(
    clippy::too_many_arguments,
    reason = "render geometry and mutable frame products belong to one row operation"
)]
fn append_description_source_spans<'a>(
    details: &'a DetailView,
    source: &'a str,
    start_byte: usize,
    end_byte: usize,
    active_line: bool,
    description_area: Rect,
    row: u16,
    theme: &Theme,
    hit_map: &mut HitMap,
    spans: &mut Vec<Span<'a>>,
    cell_cursor: &mut u16,
) {
    let mut source_cursor = start_byte;
    for timecode in details
        .timecodes
        .iter()
        .filter(|timecode| timecode.start_byte < end_byte && timecode.end_byte > start_byte)
    {
        let start = timecode.start_byte.max(start_byte);
        let end = timecode.end_byte.min(end_byte);
        if source_cursor < start {
            let plain = &source[source_cursor..start];
            *cell_cursor = cell_cursor.saturating_add(terminal_text_width(plain));
            spans.push(Span::raw(plain));
        }
        let linked = &source[start..end];
        let linked_width = terminal_text_width(linked);
        spans.push(Span::styled(
            linked,
            if active_line {
                theme
                    .active_chapter
                    .add_modifier(Modifier::UNDERLINED | Modifier::BOLD)
            } else {
                theme.accent.add_modifier(Modifier::UNDERLINED)
            },
        ));
        if let Some(media_id) = details.media_id.as_ref()
            && linked_width > 0
        {
            hit_map.detail_buttons.push((
                UiAction::ActivateTimecode {
                    media_id: media_id.clone(),
                    seconds: timecode.seconds,
                },
                Rect::new(
                    description_area.x.saturating_add(*cell_cursor),
                    row,
                    linked_width.min(description_area.width),
                    1,
                ),
            ));
        }
        *cell_cursor = cell_cursor.saturating_add(linked_width);
        source_cursor = end;
    }
    if source_cursor < end_byte {
        let plain = &source[source_cursor..end_byte];
        *cell_cursor = cell_cursor.saturating_add(terminal_text_width(plain));
        spans.push(Span::raw(plain));
    }
}

/// Finds the complete physical description line for the chapter that owns the
/// current playback position.
fn active_description_chapter_line(
    view: &ViewModel,
    details: &DetailView,
) -> Option<std::ops::Range<usize>> {
    if details.media_id.as_ref() != view.playing_media_id.as_ref() {
        return None;
    }
    let active = current_chapter_index(&view.playback_chapters, view.playback.position)?;
    let seconds = view.playback_chapters.get(active)?.start_seconds;
    let timecode = details
        .timecodes
        .iter()
        .find(|timecode| timecode.is_chapter && timecode.seconds == seconds)?;
    let start = details.description[..timecode.start_byte]
        .rfind('\n')
        .map_or(0, |newline| newline.saturating_add(1));
    let end = details.description[timecode.end_byte..]
        .find('\n')
        .map_or(details.description.len(), |offset| {
            timecode.end_byte.saturating_add(offset)
        });
    Some(start..end)
}

/// Records only non-padding cells from one explicitly selectable text row.
fn capture_selectable_details_row(frame: &mut Frame<'_>, hit_map: &mut HitMap, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut cells = (area.left()..area.right())
        .map(|x| frame.buffer_mut()[(x, area.y)].symbol().to_owned())
        .collect::<Vec<_>>();
    while cells
        .last()
        .is_some_and(|symbol| symbol.is_empty() || symbol.chars().all(char::is_whitespace))
    {
        cells.pop();
    }
    if !cells.is_empty() {
        hit_map.detail_text_rows.push(SelectableDetailsRow {
            x: area.x,
            y: area.y,
            cells,
        });
    }
}

impl HitMap {
    /// Maps a pointer to a semantic Details position.
    ///
    /// A press must land on an exact text cell. Drag and release events use
    /// clipping so leaving the selectable region extends only to its nearest
    /// visible boundary, never into the result panel or thumbnail.
    fn details_text_position(&self, x: u16, y: u16, clip: bool) -> Option<DetailsTextPosition> {
        if let Some((row, mapped)) = self.detail_text_rows.iter().enumerate().find(|(_, row)| {
            row.y == y && usize::from(x.saturating_sub(row.x)) < row.cells.len() && x >= row.x
        }) {
            return Some(DetailsTextPosition {
                row,
                column: usize::from(x.saturating_sub(mapped.x)),
            });
        }
        if !clip {
            return None;
        }

        let (row, mapped) = self
            .detail_text_rows
            .iter()
            .enumerate()
            .min_by_key(|(_, row)| row.y.abs_diff(y))?;
        let column = if x <= mapped.x {
            0
        } else {
            usize::from(x.saturating_sub(mapped.x)).min(mapped.cells.len().saturating_sub(1))
        };
        Some(DetailsTextPosition { row, column })
    }

    /// Reconstructs the selected visible cells with semantic row separators.
    fn selected_details_text(&self, selection: DetailsTextSelection) -> String {
        let (start, end) = ordered_details_positions(selection.anchor, selection.focus);
        let mut selected = String::new();
        for row_index in start.row..=end.row {
            let Some(row) = self.detail_text_rows.get(row_index) else {
                continue;
            };
            let first = if row_index == start.row {
                start.column
            } else {
                0
            };
            let last = if row_index == end.row {
                end.column
            } else {
                row.cells.len().saturating_sub(1)
            };
            if !selected.is_empty() {
                selected.push('\n');
            }
            if first <= last {
                for symbol in row
                    .cells
                    .iter()
                    .skip(first)
                    .take(last.saturating_sub(first).saturating_add(1))
                {
                    selected.push_str(symbol);
                }
            }
        }
        truncate_utf8(&mut selected, MAX_DETAILS_SELECTION_BYTES);
        selected
    }
}

fn ordered_details_positions(
    first: DetailsTextPosition,
    second: DetailsTextPosition,
) -> (DetailsTextPosition, DetailsTextPosition) {
    if first <= second {
        (first, second)
    } else {
        (second, first)
    }
}

fn truncate_utf8(value: &mut String, maximum_bytes: usize) {
    if value.len() <= maximum_bytes {
        return;
    }
    let mut boundary = maximum_bytes;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

/// Applies selection styling only to cells registered as Details text.
fn highlight_details_text_selection(
    frame: &mut Frame<'_>,
    view: &ViewModel,
    hit_map: &HitMap,
    theme: &Theme,
) {
    let Some(selection) = view.details_text_selection else {
        return;
    };
    let (start, end) = ordered_details_positions(selection.anchor, selection.focus);
    for row_index in start.row..=end.row {
        let Some(row) = hit_map.detail_text_rows.get(row_index) else {
            continue;
        };
        let first = if row_index == start.row {
            start.column
        } else {
            0
        };
        let last = if row_index == end.row {
            end.column
        } else {
            row.cells.len().saturating_sub(1)
        };
        for column in first..=last.min(row.cells.len().saturating_sub(1)) {
            let x = row
                .x
                .saturating_add(u16::try_from(column).unwrap_or(u16::MAX));
            frame.buffer_mut()[(x, row.y)].set_style(theme.selected);
        }
    }
}

fn is_creative_commons_license(label: &str) -> bool {
    let normalized = label.trim().to_ascii_lowercase();
    normalized.contains("creative commons") || normalized.contains("creativecommons.org")
}

/// Returns whether Details contains `LibriVox`'s exact, jurisdiction-qualified license label.
fn is_librivox_public_domain_license(source: &str, label: &str) -> bool {
    source.eq_ignore_ascii_case("LibriVox")
        && label
            .trim()
            .eq_ignore_ascii_case("Public domain in the United States")
}

/// Formats one non-negative count with comma-separated thousands groups.
fn format_count(count: u64) -> String {
    let digits = count.to_string();
    let mut formatted = String::with_capacity(digits.len().saturating_add(digits.len() / 3));
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(digit);
    }
    formatted
}

/// Canonicalizes `YouTube`'s localized spelling of its CC Attribution label.
///
/// Exact licence URLs and other provider terms remain unchanged because they
/// can carry information required by attribution or upload workflows.
fn display_license_label(label: &str) -> &str {
    let normalized = label.trim();
    if normalized.eq_ignore_ascii_case("Creative Commons Attribution")
        || normalized.eq_ignore_ascii_case("Creative Commons Attribution licence")
        || normalized.eq_ignore_ascii_case("Creative Commons Attribution license")
    {
        "Creative Commons Attribution"
    } else {
        label
    }
}

fn render_channel(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ViewModel,
    show_hotkeys: bool,
    thumbnail_height: u16,
    theme: &Theme,
    hit_map: &mut HitMap,
    thumbnail_renderer: Option<&mut dyn ThumbnailRenderer>,
) {
    render_information_panel(
        frame,
        area,
        view,
        show_hotkeys,
        theme,
        hit_map,
        " Channel ",
        "No channel is selected.",
        InformationPanelKind::Channel,
        false,
        ThumbnailSizing::fixed(thumbnail_height),
        thumbnail_renderer,
    );
}

/// Renders source metadata using the semantics of the selected OPML entry.
fn render_subscription_source_details(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ViewModel,
    show_hotkeys: bool,
    thumbnail_height: u16,
    theme: &Theme,
    hit_map: &mut HitMap,
    thumbnail_renderer: Option<&mut dyn ThumbnailRenderer>,
) {
    let (empty_message, kind) = match view.subscriptions.source_kind {
        SubscriptionKind::YouTube => ("No channel is selected.", InformationPanelKind::Channel),
        SubscriptionKind::Rss => (
            "No podcast feed is selected.",
            InformationPanelKind::Podcast,
        ),
        SubscriptionKind::Other => (
            "No subscription source is selected.",
            InformationPanelKind::Generic,
        ),
    };
    render_information_panel(
        frame,
        area,
        view,
        show_hotkeys,
        theme,
        hit_map,
        "",
        empty_message,
        kind,
        false,
        ThumbnailSizing::fixed(thumbnail_height),
        thumbnail_renderer,
    );
}

fn render_waveform(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ViewModel,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    hit_map.waveform_seek = None;
    if area.width == 0 || area.height == 0 {
        return;
    }

    let (media_id, generation, duration, pyramid) = match &view.waveform {
        WaveformView::Unavailable => {
            frame.render_widget(
                Paragraph::new("Waveform is available for playable local files.")
                    .style(theme.muted)
                    .wrap(Wrap { trim: true }),
                area,
            );
            return;
        }
        WaveformView::Loading { .. } => {
            frame.render_widget(
                Paragraph::new("Generating local waveform…")
                    .style(theme.muted)
                    .wrap(Wrap { trim: true }),
                area,
            );
            return;
        }
        WaveformView::Failed { message, .. } => {
            frame.render_widget(
                Paragraph::new(format!("Waveform unavailable: {message}"))
                    .style(theme.muted)
                    .wrap(Wrap { trim: true }),
                area,
            );
            return;
        }
        WaveformView::Ready {
            media_id,
            generation,
            duration,
            pyramid,
        } => (media_id, *generation, *duration, pyramid),
    };

    let columns = usize::from(area.width);
    let peaks = pyramid.reduced_for_width(columns);
    if peaks.is_empty() {
        frame.render_widget(
            Paragraph::new("The local file contains no waveform samples.")
                .style(theme.muted)
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let played_columns =
        if view.waveform_playback_matches && !duration.is_zero() && !view.playback.idle {
            usize::try_from(
                view.playback
                    .position
                    .as_nanos()
                    .saturating_mul(columns as u128)
                    .checked_div(duration.as_nanos())
                    .unwrap_or_default()
                    .min(columns as u128),
            )
            .unwrap_or(columns)
        } else {
            0
        };

    let waveform_height = area.height.min(WAVEFORM_ROWS);
    let waveform_area = Rect::new(area.x, area.y, area.width, waveform_height);
    let row_count = usize::from(waveform_height);
    let mut played_rows = (0..row_count)
        .map(|_| String::with_capacity(played_columns))
        .collect::<Vec<_>>();
    let mut remaining_rows = (0..row_count)
        .map(|_| String::with_capacity(columns.saturating_sub(played_columns)))
        .collect::<Vec<_>>();
    for column in 0..columns {
        let index = column
            .saturating_mul(peaks.len())
            .checked_div(columns)
            .unwrap_or_default()
            .min(peaks.len().saturating_sub(1));
        let symbols = waveform_column_symbols(peaks[index], row_count);
        let target_rows = if column < played_columns {
            &mut played_rows
        } else {
            &mut remaining_rows
        };
        for (row, symbol) in symbols.into_iter().take(row_count).enumerate() {
            target_rows[row].push(symbol);
        }
    }

    let played_style = theme.progress.add_modifier(Modifier::BOLD);
    let lines = played_rows
        .into_iter()
        .zip(remaining_rows)
        .map(|(played, remaining)| {
            Line::from(vec![
                Span::styled(played, played_style),
                Span::styled(remaining, theme.muted),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), waveform_area);
    if waveform_area.width > 0 && !duration.is_zero() {
        hit_map.waveform_seek = Some(WaveformSeekTarget {
            area: waveform_area,
            media_id: media_id.clone(),
            generation,
            duration,
        });
    }
}

/// Maps one peak magnitude to bottom-up terminal block cells.
///
/// Every row contributes eight amplitude levels. Silence retains a one-eighth
/// baseline in the bottom row so the seek target remains visible across quiet
/// passages.
fn waveform_column_symbols(peak: Peak, row_count: usize) -> [char; WAVEFORM_ROWS as usize] {
    const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let row_count = row_count.clamp(1, WAVEFORM_ROWS as usize);
    let maximum_level = row_count.saturating_mul(8);
    let amplitude = i32::from(peak.maximum)
        .abs()
        .max(i32::from(peak.minimum).abs()) as usize;
    let level = if amplitude == 0 {
        1
    } else {
        1 + amplitude
            .saturating_sub(1)
            .saturating_mul(maximum_level.saturating_sub(1))
            / 32_767
    }
    .min(maximum_level);
    let mut symbols = [' '; WAVEFORM_ROWS as usize];
    for (row, symbol) in symbols.iter_mut().take(row_count).enumerate() {
        let levels_below = row_count.saturating_sub(row + 1).saturating_mul(8);
        *symbol = BLOCKS[level.saturating_sub(levels_below).min(8)];
    }
    symbols
}

fn render_seek_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ViewModel,
    settings: &UiSettings,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    hit_map.seek_bar = Rect::default();
    hit_map.seek_markers.clear();
    hit_map.waveform_seek = None;
    hit_map.now_playing = None;
    let duration = view.playback.duration.unwrap_or(Duration::ZERO);
    let ratio = if duration.is_zero() {
        0.0
    } else {
        (view.playback.position.as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0)
    };
    let state = if view.playback.idle {
        None
    } else if view.playback.buffering {
        Some("buffering")
    } else if view.playback.paused {
        Some(PLAYBACK_PAUSED_SYMBOL)
    } else {
        None
    };
    let state_suffix = state.map_or_else(String::new, |state| format!(" {state}"));
    let marker = match settings.seek_bar_style {
        SeekBarStyle::Line => "",
        SeekBarStyle::NyanCat => " =^.^= ",
    };
    let title_spacing = if !view.playback.idle && view.playback.title.is_some() {
        " "
    } else {
        ""
    };
    let live_seekable =
        view.playback.live && view.playback.live_seekable_range.is_some() && !duration.is_zero();
    let recording_prefix = view
        .radio_recording
        .as_ref()
        .is_some_and(|recording| {
            view.playing_media_id.as_ref().is_some_and(|media_id| {
                media_id.source == SourceKind::Radio && media_id.external_id == recording.station_id
            })
        })
        .then_some("● REC  ")
        .unwrap_or_default();
    let recording_active = !recording_prefix.is_empty();
    let status_prefix = if view.playback.live {
        if live_seekable {
            format!(
                "{recording_prefix}LIVE −{} / {} buffer  {}×  vol {}%{state_suffix}{title_spacing}",
                format_duration(duration.saturating_sub(view.playback.position)),
                format_duration(duration),
                trim_speed(view.playback.speed),
                view.playback.volume,
            )
        } else {
            format!(
                "{recording_prefix}LIVE  {}×  vol {}%{state_suffix}{title_spacing}",
                trim_speed(view.playback.speed),
                view.playback.volume,
            )
        }
    } else {
        format!(
            "{} / {}  {}×  vol {}%{}{state_suffix}{title_spacing}",
            format_duration(view.playback.position),
            if duration.is_zero() {
                "--:--".to_owned()
            } else {
                format_duration(duration)
            },
            trim_speed(view.playback.speed),
            view.playback.volume,
            if view.repeating { "  repeat" } else { "" },
        )
    };
    let title = (!view.playback.idle)
        .then_some(view.playback.title.as_deref())
        .flatten()
        .map(|title| {
            view.radio_now_playing.as_ref().map_or_else(
                || title.to_owned(),
                |metadata| format!("{title} · {metadata}"),
            )
        });
    let title_offset = title.as_ref().map(|_| terminal_text_width(&status_prefix));
    let title_width = title
        .as_deref()
        .map(terminal_text_width)
        .unwrap_or_default();
    let label = format!(
        "{status_prefix}{}{marker}",
        title.as_deref().unwrap_or_default()
    );
    if view.waveform_visible {
        let waveform_height = area.height.saturating_sub(1).min(WAVEFORM_ROWS);
        let waveform_area = Rect::new(area.x, area.y, area.width, waveform_height);
        render_waveform(frame, waveform_area, view, theme, hit_map);
        if area.height > waveform_height {
            let status_area = Rect::new(
                area.x,
                area.y.saturating_add(waveform_height),
                area.width,
                1,
            );
            render_seek_status(
                frame,
                status_area,
                &label,
                title_offset,
                title_width,
                recording_active,
                hit_map,
            );
        }
        return;
    }
    if view.playback.live && !live_seekable {
        let status_area = if area.height >= 2 {
            Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1)
        } else {
            area
        };
        render_seek_status(
            frame,
            status_area,
            &label,
            title_offset,
            title_width,
            recording_active,
            hit_map,
        );
        return;
    }
    if !view.playback_chapters.is_empty() && area.height >= 3 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
        let label_area = rows[0];
        let track_area = rows[1];
        let status_area = rows[2];
        let gauge = Gauge::default()
            .gauge_style(theme.progress)
            .ratio(ratio)
            .label("");
        frame.render_widget(gauge, track_area);
        render_buffered_ranges(frame, track_area, &view.playback, duration, theme);
        render_chapter_timeline(
            frame, label_area, track_area, view, duration, theme, hit_map,
        );
        render_seek_status(
            frame,
            status_area,
            &label,
            title_offset,
            title_width,
            recording_active,
            hit_map,
        );
        hit_map.seek_bar = track_area;
    } else if area.height >= 2 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        let track_area = rows[0];
        let status_area = rows[1];
        let gauge = Gauge::default()
            .gauge_style(theme.progress)
            .ratio(ratio)
            .label("");
        frame.render_widget(gauge, track_area);
        render_buffered_ranges(frame, track_area, &view.playback, duration, theme);
        if !view.playback_chapters.is_empty() {
            render_chapter_timeline(
                frame,
                Rect::new(track_area.x, track_area.y, track_area.width, 0),
                track_area,
                view,
                duration,
                theme,
                hit_map,
            );
        }
        render_seek_status(
            frame,
            status_area,
            &label,
            title_offset,
            title_width,
            recording_active,
            hit_map,
        );
        hit_map.seek_bar = track_area;
    } else {
        let visible_label = truncate_terminal_text(&label, usize::from(area.width));
        let gauge = Gauge::default()
            .gauge_style(theme.progress)
            .ratio(ratio)
            .label(visible_label.clone());
        frame.render_widget(gauge, area);
        render_buffered_ranges(frame, area, &view.playback, duration, theme);
        restore_seek_label(frame, area, &visible_label);
        set_now_playing_target(area, &visible_label, title_offset, title_width, hit_map);
        hit_map.seek_bar = area;
    }
}

/// Renders the status beneath the seek track and records only the visible
/// now-playing title as a mouse target.
fn render_seek_status(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    title_offset: Option<u16>,
    title_width: u16,
    recording_active: bool,
    hit_map: &mut HitMap,
) {
    let visible_label = truncate_terminal_text(label, usize::from(area.width));
    let line = if recording_active && visible_label.starts_with("● REC  ") {
        Line::from(vec![
            Span::styled(
                "● REC",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(visible_label["● REC".len()..].to_owned()),
        ])
    } else {
        Line::raw(visible_label.clone())
    };
    frame.render_widget(Paragraph::new(line).alignment(Alignment::Center), area);
    set_now_playing_target(area, &visible_label, title_offset, title_width, hit_map);
}

/// Maps the visible title portion of a centered seek-status label.
fn set_now_playing_target(
    area: Rect,
    visible_label: &str,
    title_offset: Option<u16>,
    title_width: u16,
    hit_map: &mut HitMap,
) {
    let Some(title_offset) = title_offset else {
        return;
    };
    let visible_width = terminal_text_width(visible_label).min(area.width);
    if title_offset >= visible_width {
        return;
    }
    let label_x = centered_line_x(area, visible_width);
    let visible_title_width = title_width.min(visible_width.saturating_sub(title_offset));
    if visible_title_width > 0 {
        hit_map.now_playing = Some(Rect::new(
            label_x.saturating_add(title_offset),
            area.y,
            visible_title_width,
            1,
        ));
    }
}

fn render_chapter_timeline(
    frame: &mut Frame<'_>,
    label_area: Rect,
    track_area: Rect,
    view: &ViewModel,
    duration: Duration,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    if track_area.is_empty() {
        return;
    }
    let visible_indices = visible_chapter_indices(view, duration);
    let current = current_visible_chapter_index(view, &visible_indices);
    if duration.is_zero() {
        if !label_area.is_empty()
            && let Some(index) = current
        {
            let chapter = &view.playback_chapters[index];
            let label = truncate_terminal_text(
                &if view.show_chapter_timestamps {
                    format!(
                        "▶ {} {}",
                        format_duration(Duration::from_secs(chapter.start_seconds)),
                        chapter_title_for_display(&chapter.title)
                    )
                } else {
                    format!("▶ {}", chapter_title_for_display(&chapter.title))
                },
                usize::from(label_area.width),
            );
            frame.render_widget(Paragraph::new(label).style(theme.accent), label_area);
        }
        return;
    }

    let mut marker_columns = vec![Vec::new(); usize::from(track_area.width)];
    for index in visible_indices.iter().copied() {
        let chapter = &view.playback_chapters[index];
        if chapter.start_seconds >= duration.as_secs() {
            continue;
        }
        let column = rounded_duration_column(
            Duration::from_secs(chapter.start_seconds),
            duration,
            track_area.width.saturating_sub(1),
        );
        if let Some(indices) = marker_columns.get_mut(usize::from(column)) {
            indices.push(index);
        }
    }

    let marker_action = |seconds| {
        view.playing_media_id
            .as_ref()
            .map(|media_id| UiAction::ActivateTimecode {
                media_id: media_id.clone(),
                seconds,
            })
    };
    for (column, indices) in marker_columns
        .iter()
        .enumerate()
        .filter(|(_, indices)| !indices.is_empty())
    {
        let selected = current
            .filter(|current| indices.contains(current))
            .unwrap_or(indices[0]);
        let active = current == Some(selected);
        let glyph = if indices.len() > 1 {
            "┇"
        } else if active {
            "┃"
        } else {
            "│"
        };
        let x = track_area
            .x
            .saturating_add(u16::try_from(column).unwrap_or(track_area.width.saturating_sub(1)));
        frame.buffer_mut()[(x, track_area.y)]
            .set_symbol(glyph)
            .set_style(if active {
                theme.accent.add_modifier(Modifier::BOLD)
            } else {
                theme.muted
            });
        if let Some(action) = marker_action(view.playback_chapters[selected].start_seconds) {
            hit_map
                .seek_markers
                .push((action, Rect::new(x, track_area.y, 1, 1)));
        }
    }

    if label_area.is_empty() {
        return;
    }

    let layout = chapter_label_layout(
        view,
        duration,
        label_area.width,
        label_area.height.min(MAX_CHAPTER_LABEL_ROWS),
    );
    for placement in layout.placements {
        let chapter = &view.playback_chapters[placement.index];
        let label_rect = Rect::new(
            label_area.x.saturating_add(placement.start),
            label_area.y.saturating_add(placement.row),
            placement.width,
            1,
        );
        frame.render_widget(
            Paragraph::new(placement.text).style(if Some(placement.index) == current {
                theme.accent.add_modifier(Modifier::BOLD)
            } else {
                theme.muted
            }),
            label_rect,
        );
        if let Some(action) = marker_action(chapter.start_seconds) {
            hit_map.seek_markers.push((action, label_rect));
        }
    }
}

/// Formats one chapter label while keeping its seek position independent from
/// the user's timestamp-visibility preference.
fn chapter_timeline_label(
    chapter: &Chapter,
    current: bool,
    show_timestamp: bool,
    show_hours: bool,
) -> String {
    let prefix = if current { "▶ " } else { "" };
    if show_timestamp {
        format!(
            "{prefix}{} {}",
            format_timeline_timestamp(chapter.start_seconds, show_hours),
            chapter_title_for_display(&chapter.title)
        )
    } else {
        format!("{prefix}{}", chapter_title_for_display(&chapter.title))
    }
}

fn current_chapter_index(chapters: &[Chapter], position: Duration) -> Option<usize> {
    chapters
        .iter()
        .rposition(|chapter| chapter.start_seconds <= position.as_secs())
}

/// Returns chapters that remain visible and navigable for the current
/// advertisement preference and known media duration.
fn visible_chapter_indices(view: &ViewModel, duration: Duration) -> Vec<usize> {
    view.playback_chapters
        .iter()
        .enumerate()
        .filter(|(_, chapter)| {
            (duration.is_zero() || chapter.start_seconds < duration.as_secs())
                && (!view.skip_advertisement_chapters
                    || !is_advertisement_chapter_title(&chapter.title))
        })
        .map(|(index, _)| index)
        .collect()
}

fn current_visible_chapter_index(view: &ViewModel, visible: &[usize]) -> Option<usize> {
    visible.iter().copied().rfind(|index| {
        view.playback_chapters[*index].start_seconds <= view.playback.position.as_secs()
    })
}

fn format_timeline_timestamp(seconds: u64, show_hours: bool) -> String {
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    if show_hours {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn truncate_terminal_text(value: &str, maximum_width: usize) -> String {
    if Span::raw(value).width() <= maximum_width {
        return value.to_owned();
    }
    if maximum_width == 0 {
        return String::new();
    }
    if maximum_width == 1 {
        return "…".to_owned();
    }
    let mut output = String::new();
    let mut width = 0_usize;
    for character in value.chars() {
        let character_width = Span::raw(character.to_string()).width();
        if width.saturating_add(character_width) >= maximum_width {
            break;
        }
        output.push(character);
        width = width.saturating_add(character_width);
    }
    output.push('…');
    output
}

fn render_buffered_ranges(
    frame: &mut Frame<'_>,
    area: Rect,
    playback: &PlaybackStatus,
    duration: Duration,
    theme: &Theme,
) {
    if area.is_empty() || duration.is_zero() {
        return;
    }
    let played_end = rounded_duration_column(playback.position, duration, area.width);
    for range in &playback.buffered_ranges {
        let start = range.start.min(duration);
        let end = range.end.min(duration);
        if start >= end {
            continue;
        }
        let start = floored_duration_column(start, duration, area.width).max(played_end);
        let end = ceiled_duration_column(end, duration, area.width);
        if start >= end {
            continue;
        }
        for y in area.top()..area.bottom() {
            for offset in start..end {
                frame.buffer_mut()[(area.x.saturating_add(offset), y)]
                    .set_symbol(ratatui::symbols::block::FULL)
                    .set_style(theme.cached);
            }
        }
    }
}

fn floored_duration_column(value: Duration, duration: Duration, width: u16) -> u16 {
    let total = duration.as_nanos();
    if total == 0 {
        return 0;
    }
    let column = value
        .min(duration)
        .as_nanos()
        .saturating_mul(u128::from(width))
        / total;
    u16::try_from(column).unwrap_or(width).min(width)
}

fn ceiled_duration_column(value: Duration, duration: Duration, width: u16) -> u16 {
    let total = duration.as_nanos();
    if total == 0 {
        return 0;
    }
    let numerator = value
        .min(duration)
        .as_nanos()
        .saturating_mul(u128::from(width));
    let column = numerator.saturating_add(total.saturating_sub(1)) / total;
    u16::try_from(column).unwrap_or(width).min(width)
}

fn rounded_duration_column(value: Duration, duration: Duration, width: u16) -> u16 {
    let total = duration.as_nanos();
    if total == 0 {
        return 0;
    }
    let numerator = value
        .min(duration)
        .as_nanos()
        .saturating_mul(u128::from(width));
    let column = numerator.saturating_add(total / 2) / total;
    u16::try_from(column).unwrap_or(width).min(width)
}

fn restore_seek_label(frame: &mut Frame<'_>, area: Rect, label: &str) {
    if area.is_empty() {
        return;
    }
    let label = Span::raw(label);
    let width = u16::try_from(label.width())
        .unwrap_or(u16::MAX)
        .min(area.width);
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area.y.saturating_add(area.height / 2);
    // A raw span has no foreground or background fields, so `set_span`
    // replaces the glyphs while preserving the played/cached cell styles.
    frame.buffer_mut().set_span(x, y, &label, width);
}

fn render_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    settings: &UiSettings,
    theme: &Theme,
    _screen: Screen,
    _youtube_search_sort: YouTubeSearchSort,
    _radio_sort: RadioSort,
    _youtube_creative_commons_only: bool,
    _show_chapter_timestamps: bool,
    autoplay: bool,
    _playlist_item: Option<&PlaylistItemView>,
    _playlist_edit_available: bool,
    _playlist_back_available: bool,
    _local_size_sort: Option<LocalSizeSort>,
    status: &str,
    _playback_active: bool,
    hit_map: &mut HitMap,
) {
    hit_map.buttons.clear();
    if !status.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::raw(status))
                .alignment(Alignment::Left)
                .style(theme.accent.add_modifier(Modifier::BOLD)),
            area,
        );
        return;
    }

    let full_buttons = vec![
        (
            button("/", "Search", settings.show_hotkeys),
            UiAction::BeginSearch,
        ),
        (
            button(
                "A",
                if autoplay {
                    "Autoplay: on"
                } else {
                    "Autoplay: off"
                },
                settings.show_hotkeys,
            ),
            UiAction::ToggleAutoplay,
        ),
        (
            button("k", "Move up", settings.show_hotkeys),
            UiAction::MoveSelection(-1),
        ),
        (
            button("j", "Move down", settings.show_hotkeys),
            UiAction::MoveSelection(1),
        ),
        (
            button("↑", "Volume up", settings.show_hotkeys),
            UiAction::ChangeVolume(5),
        ),
        (
            button("↓", "Volume down", settings.show_hotkeys),
            UiAction::ChangeVolume(-5),
        ),
        (
            button("p", "Preferences", settings.show_hotkeys),
            UiAction::OpenPreferences,
        ),
        (
            button("?", "Help", settings.show_hotkeys),
            UiAction::ToggleHelp,
        ),
    ];
    let full_width = full_buttons
        .iter()
        .map(|(label, _)| usize::from(terminal_text_width(label)))
        .sum::<usize>()
        .saturating_add((full_buttons.len() - 1) * 2);
    let buttons = if full_width <= usize::from(area.width) {
        full_buttons
    } else {
        let compact_buttons = vec![
            (
                button("/", "Search", settings.show_hotkeys),
                UiAction::BeginSearch,
            ),
            (
                button(
                    "A",
                    if autoplay { "on" } else { "off" },
                    settings.show_hotkeys,
                ),
                UiAction::ToggleAutoplay,
            ),
            (
                button("k", "Up", settings.show_hotkeys),
                UiAction::MoveSelection(-1),
            ),
            (
                button("j", "Down", settings.show_hotkeys),
                UiAction::MoveSelection(1),
            ),
            (
                button("↑", "+", settings.show_hotkeys),
                UiAction::ChangeVolume(5),
            ),
            (
                button("↓", "-", settings.show_hotkeys),
                UiAction::ChangeVolume(-5),
            ),
            (
                button("p", "Prefs", settings.show_hotkeys),
                UiAction::OpenPreferences,
            ),
            (
                button("?", "Help", settings.show_hotkeys),
                UiAction::ToggleHelp,
            ),
        ];
        compact_buttons
    };
    let controls = buttons
        .iter()
        .map(|(label, _)| label.as_str())
        .collect::<Vec<_>>()
        .join("  ");
    let line_width = terminal_text_width(&controls);
    let mut button_x = centered_line_x(area, line_width);
    for (label, action) in &buttons {
        let width = terminal_text_width(label);
        let visible_width = area.right().saturating_sub(button_x).min(width);
        if visible_width > 0 {
            hit_map.buttons.push((
                action.clone(),
                Rect::new(button_x, area.y, visible_width, 1),
            ));
        }
        button_x = button_x.saturating_add(width).saturating_add(2);
    }
    frame.render_widget(
        Paragraph::new(Line::raw(controls))
            .alignment(Alignment::Center)
            .style(theme.base),
        area,
    );
}

fn centered_line_x(area: Rect, line_width: u16) -> u16 {
    area.x
        .saturating_add((area.width / 2).saturating_sub(line_width.min(area.width) / 2))
}

fn search_kind_help(view: &ViewModel) -> &'static str {
    if view.screen == Screen::YandexMusic {
        "  v all/music/podcasts/audiobooks search"
    } else {
        "  v video/channel search     N relevance/newest     C CC-only videos"
    }
}

fn render_help(frame: &mut Frame<'_>, view: &ViewModel, theme: &Theme) {
    let area = centered_rect(76, 92, frame.area());
    frame.render_widget(Clear, area);
    let mut local_help =
        "  Local: Esc parent     PageUp/Down page     H all files     Z size".to_owned();
    if cfg!(feature = "local-rename") {
        local_help.push_str("     r rename");
    }
    if cfg!(feature = "local-move") {
        local_help.push_str("     m move     Shift+J/K mark");
    }
    if cfg!(feature = "local-trash") {
        local_help.push_str("     Delete trash");
    }
    if !cfg!(feature = "local-browser") {
        local_help.clear();
    }
    #[cfg(feature = "qr")]
    let private_note_help =
        "  n private note     t Details-only text selection\n  Q selected YouTube video QR code";
    #[cfg(not(feature = "qr"))]
    let private_note_help = "  n private note     t Details-only text selection";
    let help = [
        "Navigation",
        "  / search     Tab next tab     Shift+Tab previous tab     S subscriptions",
        "  Ctrl+Tab/Ctrl+Shift+Tab are aliases when the terminal distinguishes them.",
        "  F2 offline     F3 history     Backspace back",
        "  F4 playlists     F5 stats     p preferences",
        "  F9 recent commits and installation details",
        search_kind_help(view),
        "  j/k select     Enter open/play",
        "  ↪ internal video: click the marker after a YouTube URL",
        local_help.as_str(),
        "  Radio: B cycles name / high-bitrate / low-bitrate order",
        "  Subscriptions channel: R refresh videos     i description",
        "  Playlists: e edit selected playlist     Esc or Backspace up",
        "  F8 pointer: arrows move, Enter clicks, Esc/F8 exits.",
        "  Linux /dev/ttyN: physical mouse input requires a running GPM daemon.",
        "",
        "Playback",
        "  Space pause     ←/→ 5 s     0–9 seek by 10%",
        "  ↑/↓ volume     </> speed 10%     [/] chapter     T chapter times",
        "  {/} previous / next item in the queue or its source list",
        "  r repeat     A autoplay next item from same source list   w waveform",
        "  Details: Alt+←/→ history  Alt+↑/↓ (Linux TTY: Alt+u/d) scroll",
        "",
        "Actions",
        "  Ctrl+n play next     a add to queue     u show queue     d download",
        "  o video page",
        "  l toggle todo     P choose playlist",
        "  O channel page     i subscription description     p preferences",
        "  y copy link     c channel info     s local subscribe/unsubscribe",
        private_note_help,
        "  Alt+j/k select external link     Alt+Enter open selected link",
        "",
        "Mouse",
        "  Click Details; wheel/PageUp/PageDown or Alt+↑/↓ scroll.",
        "  Press t, then drag visible Details text to copy it; t/Esc exits selection.",
        "  The result list, borders, buttons, scrollbar, and thumbnail are never selected.",
        "  Click tabs, rows, links, buttons, seek; wheel elsewhere selects rows.",
        "",
        "Press ? or Esc to close help. Press q or Ctrl+C to quit.",
    ];
    let footer = Line::styled(
        format!(
            " Youta v{} · {} ",
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_REPOSITORY")
        ),
        theme.muted,
    )
    .centered();
    frame.render_widget(
        Paragraph::new(help.join("\n"))
            .block(panel_block(" Youta help ", theme).title_bottom(footer))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Renders deterministic build history immediately and optional newer GitHub commits.
fn render_project_history_popup(
    frame: &mut Frame<'_>,
    popup: &ProjectHistoryPopupView,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let area = centered_rect(92, 90, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(" Recent Youta commits ", theme), area);

    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let content_area = sections[0];
    let (text_area, scrollbar_area) = if content_area.width > 1 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(content_area);
        (columns[0], columns[1])
    } else {
        (content_area, Rect::default())
    };

    let mut content = Vec::<Line<'static>>::new();
    append_project_history_field(
        &mut content,
        "Installation",
        &popup.installation,
        text_area.width,
        theme,
    );
    append_project_history_field(
        &mut content,
        "Executable",
        &popup.executable_path,
        text_area.width,
        theme,
    );
    append_project_history_field(
        &mut content,
        "Started in",
        &popup.started_in,
        text_area.width,
        theme,
    );
    if let Some(source) = popup.build_source.as_deref() {
        append_project_history_field(&mut content, "Build source", source, text_area.width, theme);
    }
    let current_hash = popup.current_hash.as_deref().unwrap_or("unknown");
    content.push(Line::styled(
        format!("Current build: {}", short_project_commit_hash(current_hash)),
        theme.selected,
    ));
    content.push(Line::raw(""));
    let remote_status = match &popup.remote_state {
        ProjectHistoryRemoteState::Embedded => "Showing history embedded at build time.".to_owned(),
        ProjectHistoryRemoteState::Checking => "Checking GitHub for newer commits…".to_owned(),
        ProjectHistoryRemoteState::UpToDate => "GitHub: embedded history is up to date.".to_owned(),
        ProjectHistoryRemoteState::Updated => {
            "GitHub: newer commits are cached in RAM for this process.".to_owned()
        }
        ProjectHistoryRemoteState::Unavailable(error) => {
            format!("GitHub check unavailable: {error}. Showing embedded history.")
        }
    };
    content.extend(
        wrap_text_lines(&remote_status, text_area.width)
            .into_iter()
            .map(|line| Line::styled(line, theme.muted)),
    );
    content.push(Line::raw(""));

    for (index, commit) in popup.commits.iter().take(10).enumerate() {
        if index > 0 {
            content.push(Line::raw(""));
        }
        let current = popup
            .current_hash
            .as_deref()
            .is_some_and(|hash| hash.eq_ignore_ascii_case(&commit.hash));
        let date = commit
            .committed_at
            .get(..10)
            .unwrap_or(&commit.committed_at);
        let header = format!(
            "{} · {date}{}",
            short_project_commit_hash(&commit.hash),
            if current { " · current version" } else { "" }
        );
        let style = if current {
            theme.selected
        } else {
            theme.heading
        };
        content.push(Line::styled(header, style));
        content.extend(
            wrap_text_lines(&commit.message, text_area.width)
                .into_iter()
                .map(|line| Line::styled(line, if current { theme.selected } else { theme.base })),
        );
    }
    if popup.commits.is_empty() {
        content.push(Line::styled("No embedded commit history.", theme.muted));
    }

    let content_len = content.len();
    let visible_lines = usize::from(text_area.height);
    let maximum_offset = content_len.saturating_sub(visible_lines);
    let offset = popup.scroll_offset.min(maximum_offset);
    hit_map.project_history_text_area = text_area;
    hit_map.project_history_scroll_offset = offset;
    hit_map.project_history_scroll_maximum = maximum_offset;
    hit_map.project_history_page_lines = visible_lines.max(1);
    frame.render_widget(
        Paragraph::new(
            content
                .into_iter()
                .skip(offset)
                .take(visible_lines)
                .collect::<Vec<_>>(),
        ),
        text_area,
    );
    if maximum_offset > 0 && scrollbar_area.width > 0 && scrollbar_area.height > 0 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .track_style(theme.muted)
            .thumb_symbol("█")
            .thumb_style(theme.accent);
        let mut state = ScrollbarState::new(maximum_offset.saturating_add(1))
            .position(offset)
            .viewport_content_length(visible_lines);
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut state);
    }

    let label = "[Esc] Close";
    frame.render_widget(
        Paragraph::new(label)
            .alignment(Alignment::Center)
            .style(theme.accent),
        sections[1],
    );
    let width = terminal_text_width(label).min(sections[1].width);
    let x = sections[1]
        .x
        .saturating_add(sections[1].width.saturating_sub(width) / 2);
    hit_map.project_history_buttons.push((
        UiAction::DismissProjectHistory,
        Rect::new(x, sections[1].y, width, 1),
    ));
}

/// Appends one wrapping provenance field without losing long filesystem paths.
fn append_project_history_field(
    content: &mut Vec<Line<'static>>,
    label: &str,
    value: &str,
    width: u16,
    theme: &Theme,
) {
    let field = format!("{label}: {value}");
    content.extend(
        wrap_text_lines(&field, width)
            .into_iter()
            .map(|line| Line::styled(line, theme.base)),
    );
}

/// Returns the conventional twelve-character display form of a full hash.
fn short_project_commit_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

/// Formats one independently updating version row without exposing diagnostics.
fn yt_dlp_version_lookup_text(lookup: &YtDlpVersionLookupView) -> String {
    match lookup {
        YtDlpVersionLookupView::Loading => "Loading…".to_owned(),
        YtDlpVersionLookupView::Available {
            version,
            released_on: Some(released_on),
        } if !released_on.trim().is_empty() => {
            format!("{version} (released {released_on})")
        }
        YtDlpVersionLookupView::Available { version, .. } => version.clone(),
        YtDlpVersionLookupView::Unavailable { reason } if reason.trim().is_empty() => {
            "Unavailable".to_owned()
        }
        YtDlpVersionLookupView::Unavailable { reason } => {
            format!("Unavailable ({reason})")
        }
    }
}

/// Builds the short visible body while the complete report remains copyable.
fn yt_dlp_forbidden_body(view: &YtDlpForbiddenView) -> String {
    let mut lines = vec![
        "403 from yt-dlp — try later or update it.".to_owned(),
        String::new(),
        "A 403 can be temporary or authentication-related.".to_owned(),
        String::new(),
        format!("Installed: {}", yt_dlp_version_lookup_text(&view.installed)),
        format!(
            "GitHub latest: {}",
            yt_dlp_version_lookup_text(&view.github_latest)
        ),
    ];
    if let Some(gentoo) = view.gentoo.as_ref() {
        lines.push(format!(
            "Gentoo latest stable ({}): {}",
            gentoo.arch,
            yt_dlp_version_lookup_text(&gentoo.latest_stable)
        ));
    }
    lines.extend([String::new(), format!("Project: {}", view.project_url)]);
    if let Some(gentoo) = view.gentoo.as_ref() {
        lines.push(format!("Gentoo package: {}", gentoo.package_url));
    }
    lines.join("\n")
}

/// Renders the actions allowed by the active diagnostic body and records their
/// exact mouse targets.
fn render_error_popup_controls(
    frame: &mut Frame<'_>,
    error: &ErrorPopupView,
    external_opener_available: bool,
    theme: &Theme,
    hit_map: &mut HitMap,
    area: Rect,
) {
    let mut buttons: Vec<(String, UiAction)> =
        if let Some(forbidden) = error.yt_dlp_forbidden.as_ref() {
            let mut buttons = Vec::new();
            if external_opener_available {
                buttons.push(("[u] Project".to_owned(), UiAction::OpenYtDlpProject));
                if forbidden.gentoo.is_some() {
                    buttons.push((
                        "[p] Gentoo package".to_owned(),
                        UiAction::OpenGentooYtDlpPackage,
                    ));
                }
            }
            buttons.push(("[c] Copy report".to_owned(), UiAction::CopyErrorReport));
            buttons
        } else {
            match &error.github_issue_submission {
                GitHubIssueSubmissionView::Idle => {
                    let mut buttons = vec![("[c] Copy".to_owned(), UiAction::CopyErrorReport)];
                    if external_opener_available {
                        buttons.push((
                            "[i] Copy + open issue".to_owned(),
                            UiAction::CopyAndOpenGitHubIssue,
                        ));
                    }
                    if error.gh_available {
                        buttons.push((
                            "[g] Submit GitHub issue".to_owned(),
                            UiAction::RequestGitHubIssueSubmission,
                        ));
                    }
                    buttons
                }
                GitHubIssueSubmissionView::Confirming => vec![
                    ("[c] Copy".to_owned(), UiAction::CopyErrorReport),
                    (
                        "[Enter] Submit".to_owned(),
                        UiAction::ConfirmGitHubIssueSubmission,
                    ),
                ],
                GitHubIssueSubmissionView::Submitting => Vec::new(),
                GitHubIssueSubmissionView::Submitted { .. } => {
                    let mut buttons = vec![("[c] Copy".to_owned(), UiAction::CopyErrorReport)];
                    if external_opener_available {
                        buttons.push((
                            "[o] Open issue".to_owned(),
                            UiAction::OpenGitHubIssueSubmissionTarget,
                        ));
                    }
                    buttons
                }
                GitHubIssueSubmissionView::OutcomeUnknown { .. } => {
                    let mut buttons = vec![("[c] Copy".to_owned(), UiAction::CopyErrorReport)];
                    if external_opener_available {
                        buttons.push((
                            "[o] Check existing issues".to_owned(),
                            UiAction::OpenGitHubIssueSubmissionTarget,
                        ));
                    }
                    buttons
                }
                GitHubIssueSubmissionView::Failed { .. } => {
                    let mut buttons = vec![("[c] Copy".to_owned(), UiAction::CopyErrorReport)];
                    if external_opener_available {
                        buttons.push((
                            "[i] Copy + open issue".to_owned(),
                            UiAction::CopyAndOpenGitHubIssue,
                        ));
                    }
                    if error.gh_available {
                        buttons.push((
                            "[g] Retry submission".to_owned(),
                            UiAction::RequestGitHubIssueSubmission,
                        ));
                    }
                    buttons
                }
            }
        };
    if matches!(
        error.github_issue_submission,
        GitHubIssueSubmissionView::Confirming
    ) && error.yt_dlp_forbidden.is_none()
    {
        buttons.push((
            "[Esc] Cancel".to_owned(),
            UiAction::CancelGitHubIssueSubmission,
        ));
    } else if !matches!(
        error.github_issue_submission,
        GitHubIssueSubmissionView::Submitting
    ) || error.yt_dlp_forbidden.is_some()
    {
        buttons.push(("[Esc] Close".to_owned(), UiAction::DismissErrorPopup));
    }
    let labels_width = buttons
        .iter()
        .map(|(label, _)| label.chars().count())
        .sum::<usize>()
        .saturating_add(buttons.len().saturating_sub(1) * 3);
    let labels_width = u16::try_from(labels_width).unwrap_or(u16::MAX);
    let mut button_x = area
        .x
        .saturating_add(area.width.saturating_sub(labels_width) / 2);
    let controls = buttons
        .iter()
        .map(|(label, _)| label.as_str())
        .collect::<Vec<_>>()
        .join("   ");
    frame.render_widget(
        Paragraph::new(controls.as_str())
            .alignment(Alignment::Center)
            .style(theme.accent),
        area,
    );
    for (label, action) in buttons {
        let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
        let clipped_width = area.right().saturating_sub(button_x).min(width);
        if clipped_width > 0 {
            hit_map
                .error_buttons
                .push((action, Rect::new(button_x, area.y, clipped_width, 1)));
        }
        button_x = button_x.saturating_add(width).saturating_add(3);
    }
}

/// Returns the persistent submission notice that must remain visible beside
/// transient copy or opener feedback.
fn github_issue_submission_notice(state: &GitHubIssueSubmissionView) -> Option<String> {
    match state {
        GitHubIssueSubmissionView::Idle => None,
        GitHubIssueSubmissionView::Confirming => Some(
            "This creates a public issue in vitaly-zdanevich/youta with the complete diagnostic report."
                .to_owned(),
        ),
        GitHubIssueSubmissionView::Submitting => {
            Some("Submitting the public GitHub issue…".to_owned())
        }
        GitHubIssueSubmissionView::Submitted { url } => {
            Some(format!("GitHub issue created:\n{url}"))
        }
        GitHubIssueSubmissionView::OutcomeUnknown { issues_url } => Some(format!(
            "GitHub may have created the issue. Check existing issues before retrying:\n{issues_url}"
        )),
        GitHubIssueSubmissionView::Failed { message } => {
            Some(format!("GitHub issue submission failed:\n{message}"))
        }
    }
}

fn render_error_popup(
    frame: &mut Frame<'_>,
    error: &ErrorPopupView,
    external_opener_available: bool,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let area = centered_rect(92, 88, frame.area());
    frame.render_widget(Clear, area);
    let title = if error.title.trim().is_empty() {
        " Youta error ".to_owned()
    } else {
        format!(" {} ", error.title.trim())
    };
    frame.render_widget(panel_block(&title, theme), area);

    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let submission_notice = error
        .yt_dlp_forbidden
        .is_none()
        .then(|| github_issue_submission_notice(&error.github_issue_submission))
        .flatten();
    let submission_notice_lines = submission_notice
        .as_deref()
        .map(|notice| wrap_diagnostic_report(notice, usize::from(inner.width.max(1))))
        .unwrap_or_default();
    let submission_notice_height = u16::try_from(submission_notice_lines.len())
        .unwrap_or(u16::MAX)
        .min(inner.height.saturating_sub(3));
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(submission_notice_height),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let submission_notice_area = sections[0];
    let report_area = sections[1];
    let position_area = sections[2];
    let buttons_area = sections[3];

    if submission_notice_height > 0 {
        frame.render_widget(
            Paragraph::new(
                submission_notice_lines
                    .iter()
                    .take(usize::from(submission_notice_height))
                    .cloned()
                    .map(Line::raw)
                    .collect::<Vec<_>>(),
            )
            .style(theme.accent),
            submission_notice_area,
        );
    }

    let (report_text_area, scrollbar_area) = if report_area.width > 1 {
        let report_columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(report_area);
        (report_columns[0], report_columns[1])
    } else {
        (report_area, Rect::default())
    };
    let specialized_body = error.yt_dlp_forbidden.as_ref().map(yt_dlp_forbidden_body);
    let visible_body = specialized_body.as_deref().unwrap_or(&error.report);
    let report_lines =
        wrap_diagnostic_report(visible_body, usize::from(report_text_area.width.max(1)));
    let visible_lines = usize::from(report_text_area.height);
    let maximum_offset = report_lines.len().saturating_sub(visible_lines);
    let offset = error.scroll_offset.min(maximum_offset);
    let visible = report_lines
        .iter()
        .skip(offset)
        .take(visible_lines)
        .cloned()
        .map(Line::raw)
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(visible)
            .style(theme.base)
            .wrap(Wrap { trim: false }),
        report_text_area,
    );
    if report_lines.len() > visible_lines && scrollbar_area.width > 0 && scrollbar_area.height > 0 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .track_style(theme.muted)
            .thumb_symbol("█")
            .thumb_style(theme.accent);
        let mut scrollbar_state = ScrollbarState::new(report_lines.len())
            .position(offset)
            .viewport_content_length(visible_lines);
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }

    let first_line = if report_lines.is_empty() || visible_lines == 0 {
        0
    } else {
        offset.saturating_add(1)
    };
    let last_line = offset.saturating_add(visible_lines).min(report_lines.len());
    let report_position = format!("Lines {first_line}–{last_line} of {}", report_lines.len());
    let position = if error.yt_dlp_forbidden.is_some() {
        error.action_status.clone().unwrap_or_default()
    } else if let Some(status) = &error.action_status {
        format!("{status} | {report_position}")
    } else {
        report_position
    };
    frame.render_widget(
        Paragraph::new(position)
            .alignment(Alignment::Right)
            .style(theme.muted),
        position_area,
    );

    render_error_popup_controls(
        frame,
        error,
        external_opener_available,
        theme,
        hit_map,
        buttons_area,
    );
}

/// Renders one bounded, resize-aware public-comments popup.
fn render_video_comments_popup(
    frame: &mut Frame<'_>,
    popup: &VideoCommentsPopupView,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let area = centered_rect(84, 82, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(" YouTube comments ", theme), area);

    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(popup.video_title.as_str())
            .style(theme.heading)
            .wrap(Wrap { trim: true }),
        sections[0],
    );

    let content_area = sections[1];
    let (text_area, scrollbar_area) = if content_area.width > 1 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(content_area);
        (columns[0], columns[1])
    } else {
        (content_area, Rect::default())
    };
    let mut content = Vec::new();
    match &popup.state {
        VideoCommentsPopupState::Loading => {
            content.push(Line::styled("Loading top comments…", theme.muted));
        }
        VideoCommentsPopupState::Empty => {
            content.push(Line::styled("No public comments.", theme.muted));
        }
        VideoCommentsPopupState::Error(error) => {
            let message = format!("Could not load comments: {error}");
            content.extend(
                wrap_text_lines(&message, text_area.width)
                    .into_iter()
                    .map(|line| Line::styled(line, theme.accent)),
            );
        }
        VideoCommentsPopupState::Ready => {
            for (index, comment) in popup.comments.iter().enumerate() {
                if index > 0 {
                    content.push(Line::raw(""));
                }
                let mut header = vec![
                    Span::styled(comment.author_name.clone(), theme.heading),
                    Span::styled(" · ", theme.muted),
                    Span::raw(format!(
                        "{} {}",
                        format_count(comment.like_count),
                        if comment.like_count == 1 {
                            "like"
                        } else {
                            "likes"
                        }
                    )),
                ];
                if let Some(published) = comment.published.as_deref() {
                    header.push(Span::styled(" · ", theme.muted));
                    header.push(Span::raw(published.to_owned()));
                }
                content.push(Line::from(header));
                content.extend(
                    wrap_text_lines(&comment.text, text_area.width)
                        .into_iter()
                        .map(Line::raw),
                );
            }
        }
    }
    if content.is_empty() {
        content.push(Line::raw(""));
    }
    let content_len = content.len();
    let visible_lines = usize::from(text_area.height);
    let maximum_offset = content_len.saturating_sub(visible_lines);
    let offset = popup.scroll_offset.min(maximum_offset);
    hit_map.video_comments_text_area = text_area;
    hit_map.video_comments_scroll_offset = offset;
    hit_map.video_comments_scroll_maximum = maximum_offset;
    hit_map.video_comments_page_lines = visible_lines.max(1);
    let visible = content
        .into_iter()
        .skip(offset)
        .take(visible_lines)
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible).style(theme.base), text_area);
    if maximum_offset > 0 && scrollbar_area.width > 0 && scrollbar_area.height > 0 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .track_style(theme.muted)
            .thumb_symbol("█")
            .thumb_style(theme.accent);
        let mut state = ScrollbarState::new(maximum_offset.saturating_add(1))
            .position(offset)
            .viewport_content_length(visible_lines);
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut state);
    }

    let label = "[Esc] Close";
    frame.render_widget(
        Paragraph::new(label)
            .alignment(Alignment::Center)
            .style(theme.accent),
        sections[2],
    );
    let width = terminal_text_width(label).min(sections[2].width);
    let x = sections[2]
        .x
        .saturating_add(sections[2].width.saturating_sub(width) / 2);
    hit_map.video_comments_buttons.push((
        UiAction::DismissVideoComments,
        Rect::new(x, sections[2].y, width, 1),
    ));
}

/// Number of light modules retained around every QR symbol for scanner reliability.
#[cfg(feature = "qr")]
const QR_QUIET_ZONE_MODULES: usize = 4;

/// Renders the selected video's canonical URL as an offline, scanner-safe QR popup.
#[cfg(feature = "qr")]
fn render_video_qr_popup(
    frame: &mut Frame<'_>,
    popup: &VideoQrPopupView,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let symbol_modules = popup
        .matrix
        .width()
        .saturating_add(QR_QUIET_ZONE_MODULES.saturating_mul(2));
    let symbol_rows = symbol_modules.saturating_add(1) / 2;
    let required_width = u16::try_from(symbol_modules)
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let required_height = u16::try_from(symbol_rows)
        .unwrap_or(u16::MAX)
        .saturating_add(3);

    if frame.area().width < required_width || frame.area().height < required_height {
        render_video_qr_size_fallback(frame, required_width, required_height, theme, hit_map);
        return;
    }

    let area = centered_sized_rect(required_width, required_height, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(" YouTube video QR ", theme), area);
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(u16::try_from(symbol_rows).unwrap_or(u16::MAX)),
            Constraint::Length(1),
        ])
        .split(inner);

    let qr_style = Style::default().fg(Color::Black).bg(Color::White);
    let lines = (0..symbol_rows)
        .map(|terminal_row| {
            let top = terminal_row.saturating_mul(2);
            let bottom = top.saturating_add(1);
            let symbols = (0..symbol_modules)
                .map(|x| {
                    match (
                        qr_module_is_dark(&popup.matrix, x, top),
                        qr_module_is_dark(&popup.matrix, x, bottom),
                    ) {
                        (true, true) => '█',
                        (true, false) => '▀',
                        (false, true) => '▄',
                        (false, false) => ' ',
                    }
                })
                .collect::<String>();
            Line::styled(symbols, qr_style)
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines).style(qr_style), sections[0]);
    render_video_qr_close_control(frame, sections[1], theme, hit_map);
}

/// Returns whether one quiet-zone-inclusive coordinate contains a dark module.
#[cfg(feature = "qr")]
fn qr_module_is_dark(matrix: &QrMatrix, x: usize, y: usize) -> bool {
    let width = matrix.width();
    let within_x = x.checked_sub(QR_QUIET_ZONE_MODULES).filter(|x| *x < width);
    let within_y = y.checked_sub(QR_QUIET_ZONE_MODULES).filter(|y| *y < width);
    within_x
        .zip(within_y)
        .is_some_and(|(x, y)| matrix.is_dark(x, y))
}

/// Renders an explicit resize request rather than clipping an unscannable QR symbol.
#[cfg(feature = "qr")]
fn render_video_qr_size_fallback(
    frame: &mut Frame<'_>,
    required_width: u16,
    required_height: u16,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let width = frame.area().width.min(64);
    let height = frame.area().height.min(8);
    let area = centered_sized_rect(width, height, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(" YouTube video QR ", theme), area);
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(format!(
            "Terminal is too small for this QR code. Resize to at least {required_width}×{required_height} cells."
        ))
        .alignment(Alignment::Center)
        .style(theme.base)
        .wrap(Wrap { trim: true }),
        sections[0],
    );
    render_video_qr_close_control(frame, sections[1], theme, hit_map);
}

/// Renders and records the popup's sole mouse action.
#[cfg(feature = "qr")]
fn render_video_qr_close_control(
    frame: &mut Frame<'_>,
    area: Rect,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let label = "[Esc] Close";
    frame.render_widget(
        Paragraph::new(label)
            .alignment(Alignment::Center)
            .style(theme.accent),
        area,
    );
    let width = terminal_text_width(label).min(area.width);
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    hit_map
        .video_qr_buttons
        .push((UiAction::DismissVideoQr, Rect::new(x, area.y, width, 1)));
}

fn render_youtube_setup_popup(
    frame: &mut Frame<'_>,
    setup: &YouTubeSetupPopupView,
    external_opener_available: bool,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let height_percent = if frame.area().height < 30 { 100 } else { 88 };
    let width_percent = if frame.area().width < 90 { 100 } else { 94 };
    let area = centered_rect(width_percent, height_percent, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(" Configure YouTube metadata ", theme), area);

    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(9),
            Constraint::Min(if setup.validation_error.is_some() {
                4
            } else {
                3
            }),
            Constraint::Length(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new("Choose one metadata provider. Tab/↑/↓ switches; Enter saves and retries.")
            .style(theme.base)
            .wrap(Wrap { trim: false }),
        sections[0],
    );

    let api_selected = setup.selected_field == YouTubeSetupField::ApiKey;
    let api_key = masked_setup_value(
        &setup.api_key,
        usize::from(sections[1].width.saturating_sub(2)),
    );
    let api_key = if api_key.is_empty() {
        "enter an API key".to_owned()
    } else {
        api_key
    };
    frame.render_widget(
        Paragraph::new(api_key)
            .style(if api_selected {
                theme.accent
            } else {
                theme.muted
            })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(if api_selected {
                        theme.accent
                    } else {
                        theme.border
                    })
                    .title(if api_selected {
                        " ▶ YouTube API key (masked) "
                    } else {
                        " YouTube API key (masked) "
                    }),
            ),
        sections[1],
    );
    hit_map
        .youtube_setup_fields
        .push((YouTubeSetupField::ApiKey, sections[1]));

    let invidious_selected = setup.selected_field == YouTubeSetupField::InvidiousUrl;
    let invidious = if setup.invidious_url.is_empty() {
        "https://your-invidious-instance.example".to_owned()
    } else {
        truncate_setup_value(
            &setup.invidious_url,
            usize::from(sections[2].width.saturating_sub(2)),
        )
    };
    frame.render_widget(
        Paragraph::new(invidious)
            .style(if invidious_selected {
                theme.accent
            } else {
                theme.muted
            })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(if invidious_selected {
                        theme.accent
                    } else {
                        theme.border
                    })
                    .title(if invidious_selected {
                        " ▶ Invidious instance URL "
                    } else {
                        " Invidious instance URL "
                    }),
            ),
        sections[2],
    );
    hit_map
        .youtube_setup_fields
        .push((YouTubeSetupField::InvidiousUrl, sections[2]));

    let guide_sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(4),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(2),
            Constraint::Length(1),
        ])
        .split(sections[3]);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("YouTube API-key steps: ", theme.heading),
            Span::raw(
                "(1) create/select a Google Cloud project; (2) enable YouTube Data API v3; \
                 (3) Credentials > Create credentials > API key; (4) edit key > API \
                 restrictions > Restrict key > YouTube Data API v3 > Save; (5) paste it above. \
                 Restriction blocks other Google APIs.",
            ),
        ]))
        .style(theme.base)
        .wrap(Wrap { trim: false }),
        guide_sections[0],
    );
    let external_link_style = if external_opener_available {
        theme.accent.add_modifier(Modifier::UNDERLINED)
    } else {
        theme.muted
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                if external_opener_available {
                    "[F1] Google guide: "
                } else {
                    "Google guide: "
                },
                if external_opener_available {
                    theme.accent
                } else {
                    theme.muted
                },
            ),
            Span::styled(
                YOUTUBE_API_KEY_GUIDE_URL.trim_start_matches("https://"),
                external_link_style,
            ),
        ]))
        .wrap(Wrap { trim: false }),
        guide_sections[1],
    );
    if external_opener_available {
        hit_map
            .youtube_setup_buttons
            .push((UiAction::OpenYouTubeApiKeyGuide, guide_sections[1]));
    }

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                if external_opener_available {
                    "[F2] Google Cloud: "
                } else {
                    "Google Cloud: "
                },
                if external_opener_available {
                    theme.accent
                } else {
                    theme.muted
                },
            ),
            Span::styled(
                GOOGLE_CLOUD_CREDENTIALS_URL.trim_start_matches("https://"),
                external_link_style,
            ),
        ]))
        .wrap(Wrap { trim: false }),
        guide_sections[2],
    );
    if external_opener_available {
        hit_map
            .youtube_setup_buttons
            .push((UiAction::OpenGoogleCloudCredentials, guide_sections[2]));
    }

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Invidious: ", theme.heading),
            Span::raw(
                "choose a public instance from the official list, or paste your self-hosted base \
                 URL above.",
            ),
        ]))
        .style(theme.base)
        .wrap(Wrap { trim: false }),
        guide_sections[3],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                if external_opener_available {
                    "[F3] Instance list: "
                } else {
                    "Instance list: "
                },
                if external_opener_available {
                    theme.accent
                } else {
                    theme.muted
                },
            ),
            Span::styled(
                INVIDIOUS_INSTANCES_URL.trim_start_matches("https://"),
                external_link_style,
            ),
        ]))
        .wrap(Wrap { trim: false }),
        guide_sections[4],
    );
    if external_opener_available {
        hit_map
            .youtube_setup_buttons
            .push((UiAction::OpenInvidiousInstances, guide_sections[4]));
    }

    let storage = format!(
        "API key saves to: {}\nInvidious URL saves to: {}\nAPI keys are plaintext; Unix permissions: directories 0700, files 0600. Environment variables override saved values.{}",
        setup.api_key_path,
        setup.invidious_path,
        setup
            .validation_error
            .as_ref()
            .map_or_else(String::new, |error| format!("\nError: {error}"))
    );
    frame.render_widget(
        Paragraph::new(storage)
            .style(if setup.validation_error.is_some() {
                Style::default().fg(Color::Red)
            } else {
                theme.muted
            })
            .wrap(Wrap { trim: false }),
        sections[4],
    );

    let buttons = [
        ("[Enter] Save and retry", UiAction::SubmitYouTubeSetup),
        ("[Esc] Cancel", UiAction::DismissYouTubeSetup),
    ];
    let controls = buttons
        .iter()
        .map(|(label, _)| *label)
        .collect::<Vec<_>>()
        .join("   ");
    frame.render_widget(
        Paragraph::new(controls.as_str())
            .alignment(Alignment::Center)
            .style(theme.accent),
        sections[5],
    );
    let labels_width = buttons
        .iter()
        .map(|(label, _)| label.chars().count())
        .sum::<usize>()
        .saturating_add(3);
    let labels_width = u16::try_from(labels_width).unwrap_or(u16::MAX);
    let mut button_x = sections[5]
        .x
        .saturating_add(sections[5].width.saturating_sub(labels_width) / 2);
    for (label, action) in buttons {
        let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
        hit_map.youtube_setup_buttons.push((
            action,
            Rect::new(button_x, sections[5].y, width, sections[5].height),
        ));
        button_x = button_x.saturating_add(width).saturating_add(3);
    }
}

/// Renders one masked Yandex Music OAuth-token editor.
fn render_yandex_music_setup_popup(
    frame: &mut Frame<'_>,
    setup: &YandexMusicSetupPopupView,
    external_opener_available: bool,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let height = if setup.validation_error.is_some() {
        18
    } else if setup.validating {
        17
    } else {
        16
    };
    let area = centered_sized_rect(100, height, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        panel_block(
            if setup.validating {
                " Validating Yandex Music… "
            } else {
                " Configure Yandex Music "
            },
            theme,
        ),
        area,
    );

    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(if setup.validation_error.is_some() || setup.validating {
                3
            } else {
                2
            }),
            Constraint::Length(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(
            "Yandex Music requires a user OAuth access token—not an API key. \
             Paste it below; rendering and diagnostics always mask it.",
        )
        .style(theme.base)
        .wrap(Wrap { trim: false }),
        sections[0],
    );

    let token = masked_setup_value(
        &setup.token,
        usize::from(sections[1].width.saturating_sub(2)),
    );
    frame.render_widget(
        Paragraph::new(if token.is_empty() {
            "enter an OAuth token".to_owned()
        } else {
            token
        })
        .style(if setup.validating {
            theme.muted
        } else {
            theme.accent
        })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(if setup.validating {
                    theme.muted
                } else {
                    theme.accent
                })
                .title(if setup.validating {
                    " OAuth token (masked; read-only) "
                } else {
                    " ▶ OAuth token (masked) "
                }),
        ),
        sections[1],
    );
    if !setup.validating {
        hit_map.yandex_music_setup_field = Some(sections[1]);
    }

    frame.render_widget(
        Paragraph::new(
            "The integration uses Yandex Music's private client API and can change when Yandex updates it.",
        )
        .style(theme.muted)
        .wrap(Wrap { trim: false }),
        sections[2],
    );

    let external_link_style = if external_opener_available {
        theme.accent.add_modifier(Modifier::UNDERLINED)
    } else {
        theme.muted
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                if external_opener_available {
                    "[F1] OAuth guide: "
                } else {
                    "OAuth guide: "
                },
                if external_opener_available {
                    theme.accent
                } else {
                    theme.muted
                },
            ),
            Span::styled(
                YANDEX_OAUTH_GUIDE_URL.trim_start_matches("https://"),
                external_link_style,
            ),
        ])),
        sections[3],
    );
    if external_opener_available {
        hit_map
            .yandex_music_setup_buttons
            .push((UiAction::OpenYandexOAuthGuide, sections[3]));
    }

    let storage = format!(
        "{}Token saves to: {}\nPlaintext credential; Unix permissions: directories 0700, files 0600. \
         YOUTA_PROVIDERS__YANDEX_MUSIC_TOKEN overrides the saved value.{}",
        if setup.validating {
            "Validating the candidate token before saving…\n"
        } else {
            ""
        },
        setup.token_path,
        setup
            .validation_error
            .as_ref()
            .map_or_else(String::new, |error| format!("\nError: {error}"))
    );
    frame.render_widget(
        Paragraph::new(storage)
            .style(if setup.validation_error.is_some() {
                Style::default().fg(Color::Red)
            } else {
                theme.muted
            })
            .wrap(Wrap { trim: false }),
        sections[4],
    );

    let submit_label = if setup.validating {
        "[Enter] Validating…"
    } else {
        "[Enter] Save and load"
    };
    let cancel_label = "[Esc] Cancel";
    let controls = format!("{submit_label}   {cancel_label}");
    frame.render_widget(
        Paragraph::new(controls.as_str())
            .alignment(Alignment::Center)
            .style(if setup.validating {
                theme.muted
            } else {
                theme.accent
            }),
        sections[5],
    );
    let controls_width = u16::try_from(controls.chars().count()).unwrap_or(u16::MAX);
    let mut button_x = sections[5]
        .x
        .saturating_add(sections[5].width.saturating_sub(controls_width) / 2);
    let submit_width = u16::try_from(submit_label.chars().count()).unwrap_or(u16::MAX);
    if !setup.validating {
        hit_map.yandex_music_setup_buttons.push((
            UiAction::SubmitYandexMusicSetup,
            Rect::new(button_x, sections[5].y, submit_width, sections[5].height),
        ));
    }
    button_x = button_x.saturating_add(submit_width).saturating_add(3);
    let cancel_width = u16::try_from(cancel_label.chars().count()).unwrap_or(u16::MAX);
    hit_map.yandex_music_setup_buttons.push((
        UiAction::DismissYandexMusicSetup,
        Rect::new(button_x, sections[5].y, cancel_width, sections[5].height),
    ));
}

fn render_rss_subscription_popup(
    frame: &mut Frame<'_>,
    popup: &RssSubscriptionPopupView,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let height = if popup.validation_error.is_some() {
        14
    } else {
        12
    };
    let area = centered_sized_rect(96, height, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(" Add RSS podcast feed ", theme), area);
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.is_empty() {
        return;
    }
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Min(2),
            Constraint::Length(u16::from(popup.validation_error.is_some()) * 2),
            Constraint::Length(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(
            "Paste an absolute HTTP(S) RSS or Atom URL.\nSaves a portable audio/video podcast feed subscription to OPML.",
        )
        .style(theme.base)
        .wrap(Wrap { trim: false }),
        sections[0],
    );
    let url = if popup.url.is_empty() {
        "https://example.org/podcast.xml"
    } else {
        popup.url.as_str()
    };
    frame.render_widget(
        Paragraph::new(truncate_setup_value(
            url,
            usize::from(sections[1].width.saturating_sub(2)),
        ))
        .style(if popup.url.is_empty() {
            theme.muted
        } else {
            theme.accent
        })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.accent)
                .title(" ▶ Feed URL "),
        ),
        sections[1],
    );
    hit_map.rss_subscription_field = Some(sections[1]);

    frame.render_widget(
        Paragraph::new(format!(
            "Will save to: {}\nThe portable OPML file remains private to your Youta configuration.",
            popup.config_path
        ))
        .style(theme.muted)
        .wrap(Wrap { trim: false }),
        sections[2],
    );
    if let Some(error) = popup.validation_error.as_deref() {
        frame.render_widget(
            Paragraph::new(format!("Error: {error}"))
                .style(Style::default().fg(Color::Red))
                .wrap(Wrap { trim: false }),
            sections[3],
        );
    }

    let buttons = [
        ("[Enter] Add feed", UiAction::SubmitRssSubscription),
        ("[Esc] Cancel", UiAction::DismissRssSubscriptionPopup),
    ];
    let controls = buttons
        .iter()
        .map(|(label, _)| *label)
        .collect::<Vec<_>>()
        .join("   ");
    frame.render_widget(
        Paragraph::new(controls)
            .alignment(Alignment::Center)
            .style(theme.accent),
        sections[4],
    );
    let labels_width = buttons
        .iter()
        .map(|(label, _)| terminal_text_width(label))
        .sum::<u16>()
        .saturating_add(3);
    let mut button_x = sections[4]
        .x
        .saturating_add(sections[4].width.saturating_sub(labels_width) / 2);
    for (label, action) in buttons {
        let width = terminal_text_width(label);
        hit_map.rss_subscription_buttons.push((
            action,
            Rect::new(button_x, sections[4].y, width, sections[4].height),
        ));
        button_x = button_x.saturating_add(width).saturating_add(3);
    }
}

fn render_playlist_popup(
    frame: &mut Frame<'_>,
    popup: &PlaylistPopupView,
    show_hotkeys: bool,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let area = centered_sized_rect(82, 20, frame.area());
    frame.render_widget(Clear, area);
    let title = match popup.mode {
        PlaylistPopupMode::Choose => " Add to playlist ",
        PlaylistPopupMode::Create => " New playlist ",
        PlaylistPopupMode::Edit => " Edit playlist ",
    };
    frame.render_widget(panel_block(title, theme), area);
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.is_empty() {
        return;
    }
    match popup.mode {
        PlaylistPopupMode::Choose => {
            render_playlist_chooser(frame, inner, popup, show_hotkeys, theme, hit_map);
        }
        PlaylistPopupMode::Create | PlaylistPopupMode::Edit => {
            render_playlist_editor(frame, inner, popup, show_hotkeys, theme, hit_map);
        }
    }
}

fn render_playlist_chooser(
    frame: &mut Frame<'_>,
    area: Rect,
    popup: &PlaylistPopupView,
    show_hotkeys: bool,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(if popup.validation_error.is_some() {
                2
            } else {
                0
            }),
            Constraint::Length(1),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(format!(
            "Item: {}\nEnter toggles membership and keeps this chooser open.",
            popup.item_title
        ))
        .style(theme.base)
        .wrap(Wrap { trim: false }),
        sections[0],
    );

    hit_map.playlist_popup_rows = sections[1];
    if popup.playlists.is_empty() {
        frame.render_widget(
            Paragraph::new("No playlists yet. Press n to create one.")
                .style(theme.muted)
                .alignment(Alignment::Center),
            sections[1],
        );
    } else {
        let selected = popup.selected.min(popup.playlists.len().saturating_sub(1));
        let visible_rows = usize::from(sections[1].height).max(1);
        let first = selected
            .saturating_add(1)
            .saturating_sub(visible_rows)
            .min(popup.playlists.len().saturating_sub(visible_rows));
        hit_map.playlist_popup_first_index = first;
        let items = popup
            .playlists
            .iter()
            .enumerate()
            .skip(first)
            .take(visible_rows)
            .map(|(index, playlist)| {
                let selected = index == selected;
                let marker = if selected { "▶" } else { " " };
                let membership = if playlist.contains_item {
                    "[✓]"
                } else {
                    "[ ]"
                };
                ListItem::new(format!("{marker} {membership} {}", playlist.name)).style(
                    if selected {
                        theme.selected
                    } else if playlist.contains_item {
                        theme.accent
                    } else {
                        theme.base
                    },
                )
            })
            .collect::<Vec<_>>();
        frame.render_widget(List::new(items), sections[1]);
    }
    if let Some(error) = popup.validation_error.as_deref() {
        frame.render_widget(
            Paragraph::new(format!("Error: {error}"))
                .style(Style::default().fg(Color::Red))
                .wrap(Wrap { trim: false }),
            sections[2],
        );
    }
    render_playlist_popup_buttons(
        frame,
        sections[3],
        [
            (
                button("Enter", "Add/remove", show_hotkeys),
                UiAction::ToggleSelectedPlaylistMembership,
            ),
            (
                button("n", "New playlist", show_hotkeys),
                UiAction::BeginNewPlaylist,
            ),
            (
                button("Esc", "Close", show_hotkeys),
                UiAction::DismissPlaylistPopup,
            ),
        ],
        theme,
        hit_map,
    );
}

fn render_playlist_editor(
    frame: &mut Frame<'_>,
    area: Rect,
    popup: &PlaylistPopupView,
    show_hotkeys: bool,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Min(2),
            Constraint::Length(1),
        ])
        .split(area);
    let explanation = match popup.mode {
        PlaylistPopupMode::Create => "Create a playlist and add the selected item.",
        PlaylistPopupMode::Edit => "Edit display fields; stable playlist identity is unchanged.",
        PlaylistPopupMode::Choose => unreachable!("chooser uses its dedicated renderer"),
    };
    frame.render_widget(Paragraph::new(explanation).style(theme.base), sections[0]);

    let name_count = popup.editor_name.len();
    let name_focused = popup.editor_field == PlaylistEditorField::Name;
    frame.render_widget(
        Paragraph::new(if popup.editor_name.is_empty() {
            "Required"
        } else {
            popup.editor_name.as_str()
        })
        .style(if popup.editor_name.is_empty() {
            theme.muted
        } else {
            theme.base
        })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(if name_focused {
                    theme.accent
                } else {
                    theme.border
                })
                .title(format!(
                    " {}Name bytes ({name_count}/{}) ",
                    if name_focused { "▶ " } else { "" },
                    popup.name_limit
                )),
        ),
        sections[1],
    );
    hit_map
        .playlist_popup_fields
        .push((PlaylistEditorField::Name, sections[1]));

    let description_count = popup.editor_description.chars().count();
    let description_focused = popup.editor_field == PlaylistEditorField::Description;
    frame.render_widget(
        Paragraph::new(if popup.editor_description.is_empty() {
            "Optional"
        } else {
            popup.editor_description.as_str()
        })
        .style(if popup.editor_description.is_empty() {
            theme.muted
        } else {
            theme.base
        })
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(if description_focused {
                    theme.accent
                } else {
                    theme.border
                })
                .title(format!(
                    " {}Description ({description_count}/{}) ",
                    if description_focused { "▶ " } else { "" },
                    popup.description_limit
                )),
        ),
        sections[2],
    );
    hit_map
        .playlist_popup_fields
        .push((PlaylistEditorField::Description, sections[2]));

    let mut note = "Tab or ↑/↓ switches fields.".to_owned();
    if let Some(error) = popup.validation_error.as_deref() {
        note.push_str(&format!("\nError: {error}"));
    }
    frame.render_widget(
        Paragraph::new(note)
            .style(if popup.validation_error.is_some() {
                Style::default().fg(Color::Red)
            } else {
                theme.muted
            })
            .wrap(Wrap { trim: false }),
        sections[3],
    );

    let submit = match popup.mode {
        PlaylistPopupMode::Create => (
            button("Enter", "Create and add", show_hotkeys),
            UiAction::CreatePlaylistAndAdd,
        ),
        PlaylistPopupMode::Edit => (
            button("Enter", "Save changes", show_hotkeys),
            UiAction::UpdatePlaylist,
        ),
        PlaylistPopupMode::Choose => unreachable!("chooser uses its dedicated renderer"),
    };
    render_playlist_popup_buttons(
        frame,
        sections[4],
        [
            submit,
            (
                button("Esc", "Back/close", show_hotkeys),
                UiAction::DismissPlaylistPopup,
            ),
        ],
        theme,
        hit_map,
    );
}

/// Renders the playback queue.
///
/// The queue is the one piece of state Youta has always maintained and never
/// shown: `a` and `Ctrl+n` have always been able to fill it, and nothing could
/// display, reorder, or empty it afterwards.
fn render_queue_popup(
    frame: &mut Frame<'_>,
    popup: &QueuePopupView,
    show_hotkeys: bool,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let area = centered_sized_rect(82, 20, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(" Playback queue ", theme), area);
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.is_empty() {
        return;
    }
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(inner);
    let position = match popup.current {
        Some(current) => format!(
            "Playing {} of {}",
            current.saturating_add(1),
            popup.items.len()
        ),
        None => format!(
            "{} queued; the queue has been played through",
            popup.items.len()
        ),
    };
    let repeat = if popup.repeat_one {
        " · repeating the current item"
    } else {
        ""
    };
    frame.render_widget(
        Paragraph::new(format!(
            "{position}{repeat}\nEnter plays the selected entry from here."
        ))
        .style(theme.base)
        .wrap(Wrap { trim: false }),
        sections[0],
    );

    hit_map.queue_popup_rows = sections[1];
    let visible_rows = usize::from(sections[1].height).max(1);
    let selected = popup.selected.min(popup.items.len().saturating_sub(1));
    let first = selected.saturating_add(1).saturating_sub(visible_rows).min(
        popup
            .items
            .len()
            .saturating_sub(visible_rows.min(popup.items.len())),
    );
    hit_map.queue_popup_first_index = first;
    let width = usize::from(sections[1].width);
    let items = popup
        .items
        .iter()
        .enumerate()
        .skip(first)
        .take(visible_rows)
        .map(|(index, item)| {
            let is_selected = index == selected;
            let is_current = popup.current == Some(index);
            let marker = if is_current {
                "▶"
            } else if is_selected {
                "›"
            } else {
                " "
            };
            let mut label = format!("{marker} {}", item.title);
            if !item.subtitle.is_empty() {
                label.push_str(" · ");
                label.push_str(&item.subtitle);
            }
            if !item.length.is_empty() {
                label.push_str(" · ");
                label.push_str(&item.length);
            }
            ListItem::new(truncate_terminal_text(&label, width)).style(if is_selected {
                theme.selected
            } else if is_current {
                theme.accent
            } else {
                theme.base
            })
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items), sections[1]);

    let mut buttons = vec![(
        button("Enter", "Play", show_hotkeys),
        UiAction::ActivateQueuePopupRow(selected),
    )];
    // Removing the entry that is playing is refused by the controller, so the
    // control that would ask for it is not offered.
    if popup.current != Some(selected) {
        buttons.push((
            button("x", "Remove", show_hotkeys),
            UiAction::RemoveQueuePopupRow(selected),
        ));
    }
    buttons.push((button("C", "Clear", show_hotkeys), UiAction::ClearQueue));
    buttons.push((
        button("Esc", "Close", show_hotkeys),
        UiAction::DismissQueuePopup,
    ));
    render_queue_popup_buttons(frame, sections[2], buttons, theme, hit_map);
}

/// Lays the queue controls out on one centered row and records their hit areas.
fn render_queue_popup_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    buttons: Vec<(String, UiAction)>,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let controls = buttons
        .iter()
        .map(|(label, _)| label.as_str())
        .collect::<Vec<_>>()
        .join("   ");
    frame.render_widget(
        Paragraph::new(controls.as_str())
            .alignment(Alignment::Center)
            .style(theme.accent),
        area,
    );
    let mut x = centered_line_x(area, terminal_text_width(&controls));
    for (label, action) in buttons {
        let width = terminal_text_width(&label).min(area.right().saturating_sub(x));
        if width > 0 {
            hit_map
                .queue_popup_buttons
                .push((action, Rect::new(x, area.y, width, area.height.min(1))));
        }
        x = x
            .saturating_add(terminal_text_width(&label))
            .saturating_add(3);
    }
}

fn render_playlist_popup_buttons<const N: usize>(
    frame: &mut Frame<'_>,
    area: Rect,
    buttons: [(String, UiAction); N],
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let controls = buttons
        .iter()
        .map(|(label, _)| label.as_str())
        .collect::<Vec<_>>()
        .join("   ");
    frame.render_widget(
        Paragraph::new(controls.as_str())
            .alignment(Alignment::Center)
            .style(theme.accent),
        area,
    );
    let mut x = centered_line_x(area, terminal_text_width(&controls));
    for (label, action) in buttons {
        let width = terminal_text_width(&label).min(area.right().saturating_sub(x));
        if width > 0 {
            hit_map
                .playlist_popup_buttons
                .push((action, Rect::new(x, area.y, width, area.height.min(1))));
        }
        x = x
            .saturating_add(terminal_text_width(&label))
            .saturating_add(3);
    }
}

/// Grapheme-safe visual lines and insertion-row position for one note draft.
#[derive(Debug, PartialEq, Eq)]
struct WrappedPrivateNote {
    lines: Vec<String>,
    cursor_row: usize,
}

/// Wraps a private note by terminal-cell width without splitting graphemes.
///
/// Explicit line breaks preserve empty lines. The visible insertion marker is
/// included in the measured output so cursor following matches what the user
/// sees at the right edge of a line.
fn wrap_private_note(body: &str, requested_cursor: usize, width: u16) -> WrappedPrivateNote {
    const CURSOR_MARKER: &str = "▏";

    let width = usize::from(width.max(1));
    let requested_cursor = requested_cursor.min(body.len());
    let cursor = if requested_cursor == body.len() {
        requested_cursor
    } else {
        body.grapheme_indices(true)
            .map(|(index, _)| index)
            .take_while(|index| *index <= requested_cursor)
            .last()
            .unwrap_or_default()
    };
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_width = 0_usize;
    let mut cursor_row = 0_usize;
    let mut cursor_inserted = false;

    let push_line = |lines: &mut Vec<String>, line: &mut String, line_width: &mut usize| {
        lines.push(std::mem::take(line));
        *line_width = 0;
    };
    let push_grapheme =
        |grapheme: &str, lines: &mut Vec<String>, line: &mut String, line_width: &mut usize| {
            let grapheme_width = usize::from(terminal_text_width(grapheme));
            if *line_width > 0 && line_width.saturating_add(grapheme_width) > width {
                push_line(lines, line, line_width);
            }
            line.push_str(grapheme);
            *line_width = line_width.saturating_add(grapheme_width);
        };
    let insert_cursor = |lines: &mut Vec<String>,
                         line: &mut String,
                         line_width: &mut usize,
                         cursor_row: &mut usize| {
        push_grapheme(CURSOR_MARKER, lines, line, line_width);
        *cursor_row = lines.len();
    };

    for (index, grapheme) in body.grapheme_indices(true) {
        if !cursor_inserted && index == cursor {
            insert_cursor(&mut lines, &mut line, &mut line_width, &mut cursor_row);
            cursor_inserted = true;
        }
        if grapheme == "\n" {
            push_line(&mut lines, &mut line, &mut line_width);
        } else {
            push_grapheme(grapheme, &mut lines, &mut line, &mut line_width);
        }
    }
    if !cursor_inserted {
        insert_cursor(&mut lines, &mut line, &mut line_width, &mut cursor_row);
    }
    lines.push(line);

    WrappedPrivateNote { lines, cursor_row }
}

fn render_private_note_popup(
    frame: &mut Frame<'_>,
    popup: &PrivateNotePopupView,
    show_hotkeys: bool,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let area = centered_sized_rect(88, 24, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(" Private note ", theme), area);
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.is_empty() {
        return;
    }
    let notice_height = if popup.confirming_delete { 2 } else { 1 };
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(8),
            Constraint::Length(notice_height),
            Constraint::Length(if popup.validation_error.is_some() {
                2
            } else {
                0
            }),
            Constraint::Length(1),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(format!(
            "{} note for: {}",
            if popup.existing { "Editing" } else { "New" },
            popup.target_label
        ))
        .style(theme.base)
        .wrap(Wrap { trim: false }),
        sections[0],
    );

    let text_block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.accent)
        .title(format!(" Text · {} / 16384 bytes ", popup.body.len()));
    let text_inner = text_block.inner(sections[1]);
    frame.render_widget(text_block, sections[1]);
    if text_inner.width > 0 && text_inner.height > 0 {
        let visible_lines = usize::from(text_inner.height);
        let initial = wrap_private_note(&popup.body, popup.cursor_byte, text_inner.width);
        let overflow = initial.lines.len() > visible_lines;
        let (text_area, scrollbar_area) = if overflow && text_inner.width > 1 {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(text_inner);
            (columns[0], columns[1])
        } else {
            (text_inner, Rect::default())
        };
        let wrapped = if text_area.width == text_inner.width {
            initial
        } else {
            wrap_private_note(&popup.body, popup.cursor_byte, text_area.width)
        };
        let maximum_offset = wrapped.lines.len().saturating_sub(visible_lines);
        let mut offset = popup.scroll_offset.min(maximum_offset);
        if popup.follow_cursor {
            if wrapped.cursor_row < offset {
                offset = wrapped.cursor_row;
            } else if wrapped.cursor_row >= offset.saturating_add(visible_lines) {
                offset = wrapped
                    .cursor_row
                    .saturating_add(1)
                    .saturating_sub(visible_lines);
            }
        }
        let visible = wrapped
            .lines
            .iter()
            .skip(offset)
            .take(visible_lines)
            .cloned()
            .map(Line::raw)
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(visible).style(theme.base), text_area);
        if wrapped.lines.len() > visible_lines
            && scrollbar_area.width > 0
            && scrollbar_area.height > 0
        {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .track_style(theme.muted)
                .thumb_symbol("█")
                .thumb_style(theme.accent);
            let mut scrollbar_state = ScrollbarState::new(maximum_offset.saturating_add(1))
                .position(offset)
                .viewport_content_length(visible_lines);
            frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
        }
        hit_map.private_note_text_area = text_area;
        hit_map.private_note_scroll_offset = offset;
        hit_map.private_note_scroll_maximum = maximum_offset;
    }

    let notice = if popup.confirming_delete {
        format!(
            "Delete this note permanently? Press Delete or Enter again to confirm.\nStored in: {}",
            popup.storage_path
        )
    } else {
        format!("Stored in: {}", popup.storage_path)
    };
    frame.render_widget(
        Paragraph::new(notice)
            .style(if popup.confirming_delete {
                Style::default().fg(Color::Red)
            } else {
                theme.muted
            })
            .wrap(Wrap { trim: false }),
        sections[2],
    );
    if let Some(error) = popup.validation_error.as_deref() {
        frame.render_widget(
            Paragraph::new(format!("Error: {error}"))
                .style(Style::default().fg(Color::Red))
                .wrap(Wrap { trim: false }),
            sections[3],
        );
    }

    let buttons = if popup.confirming_delete {
        vec![
            (
                button("Delete/Enter", "Confirm delete", show_hotkeys),
                UiAction::RequestPrivateNoteDelete,
            ),
            (
                button("Esc", "Cancel", show_hotkeys),
                UiAction::DismissPrivateNotePopup,
            ),
        ]
    } else {
        let mut buttons = vec![(
            button("Ctrl+S", "Save", show_hotkeys),
            UiAction::SavePrivateNote,
        )];
        if popup.existing {
            buttons.push((
                button("Delete", "Delete note", show_hotkeys),
                UiAction::RequestPrivateNoteDelete,
            ));
        }
        buttons.push((
            button("Esc", "Cancel", show_hotkeys),
            UiAction::DismissPrivateNotePopup,
        ));
        buttons
    };
    let controls = buttons
        .iter()
        .map(|(label, _)| label.as_str())
        .collect::<Vec<_>>()
        .join("   ");
    frame.render_widget(
        Paragraph::new(controls.as_str())
            .alignment(Alignment::Center)
            .style(theme.accent),
        sections[4],
    );
    let mut x = centered_line_x(sections[4], terminal_text_width(&controls));
    for (label, action) in buttons {
        let width = terminal_text_width(&label).min(sections[4].right().saturating_sub(x));
        if width > 0 {
            hit_map
                .private_note_buttons
                .push((action, Rect::new(x, sections[4].y, width, 1)));
        }
        x = x
            .saturating_add(terminal_text_width(&label))
            .saturating_add(3);
    }
}

fn render_preferences_popup(
    frame: &mut Frame<'_>,
    preferences: &PreferencesPopupView,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let area = centered_rect(76, 94, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(" Youta preferences ", theme), area);
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.is_empty() {
        return;
    }
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new("Subscriptions layout. Choose with d/s or ←/→, then press Enter to save.")
            .style(theme.base)
            .wrap(Wrap { trim: false }),
        sections[0],
    );

    let choices = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(sections[1]);
    let options = [
        (
            SubscriptionsLayout::DrillDown,
            "[d] Drill-down",
            "sources → videos + Details",
        ),
        (
            SubscriptionsLayout::Split,
            "[s] Split",
            "sources and videos together",
        ),
    ];
    for ((layout, label, description), choice_area) in options.into_iter().zip(choices.iter()) {
        let selected = layout == preferences.subscriptions_layout;
        frame.render_widget(
            Paragraph::new(format!("{label}\n{description}"))
                .style(if selected { theme.selected } else { theme.base })
                .alignment(Alignment::Center)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(if selected { theme.accent } else { theme.border }),
                ),
            *choice_area,
        );
        hit_map
            .preferences_buttons
            .push((UiAction::SetSubscriptionsLayout(layout), *choice_area));
    }

    let advertisement_label = format!(
        "[a] Skip sections named Реклама: {}",
        if preferences.skip_advertisement_chapters {
            "on"
        } else {
            "off"
        }
    );
    frame.render_widget(
        Paragraph::new(advertisement_label.clone())
            .style(if preferences.skip_advertisement_chapters {
                theme.selected
            } else {
                theme.base
            })
            .alignment(Alignment::Center),
        sections[2],
    );
    hit_map.preferences_buttons.push((
        UiAction::ToggleSkipAdvertisementChapters,
        Rect::new(
            centered_line_x(sections[2], terminal_text_width(&advertisement_label)),
            sections[2].y,
            terminal_text_width(&advertisement_label).min(sections[2].width),
            1,
        ),
    ));

    let youtube_prewarm_label = format!(
        "[y] Prepare selected YouTube audio: {}",
        if preferences.youtube_prewarm {
            "on"
        } else {
            "off"
        }
    );
    frame.render_widget(
        Paragraph::new(youtube_prewarm_label.clone())
            .style(if preferences.youtube_prewarm {
                theme.selected
            } else {
                theme.base
            })
            .alignment(Alignment::Center),
        sections[3],
    );
    hit_map.preferences_buttons.push((
        UiAction::ToggleYouTubePrewarm,
        Rect::new(
            centered_line_x(sections[3], terminal_text_width(&youtube_prewarm_label)),
            sections[3].y,
            terminal_text_width(&youtube_prewarm_label).min(sections[3].width),
            1,
        ),
    ));

    let youtube_thumbnail_label = if cfg!(feature = "images") {
        format!(
            "[t] YouTube thumbnails: {}",
            preferences.youtube_thumbnail_size.label()
        )
    } else {
        "YouTube thumbnails: unavailable in this build".to_owned()
    };
    frame.render_widget(
        Paragraph::new(youtube_thumbnail_label.clone())
            .style(if cfg!(feature = "images") {
                theme.selected
            } else {
                theme.muted
            })
            .alignment(Alignment::Center),
        sections[4],
    );
    if cfg!(feature = "images") {
        hit_map.preferences_buttons.push((
            UiAction::CycleYouTubeThumbnailSize,
            Rect::new(
                centered_line_x(sections[4], terminal_text_width(&youtube_thumbnail_label)),
                sections[4].y,
                terminal_text_width(&youtube_thumbnail_label).min(sections[4].width),
                1,
            ),
        ));
    }

    let folder_size_label = format!(
        "[f] Show Local folder sizes: {}",
        if preferences.show_local_folder_sizes {
            "on"
        } else {
            "off"
        }
    );
    frame.render_widget(
        Paragraph::new(folder_size_label.clone())
            .style(if preferences.show_local_folder_sizes {
                theme.selected
            } else {
                theme.base
            })
            .alignment(Alignment::Center),
        sections[5],
    );
    hit_map.preferences_buttons.push((
        UiAction::ToggleLocalFolderSizes,
        Rect::new(
            centered_line_x(sections[5], terminal_text_width(&folder_size_label)),
            sections[5].y,
            terminal_text_width(&folder_size_label).min(sections[5].width),
            1,
        ),
    ));

    let tty_images_label = if cfg!(feature = "images") {
        format!(
            "[i] Show images in TTY: {}",
            if preferences.show_images_in_tty {
                "on"
            } else {
                "off"
            }
        )
    } else {
        "[i] Show images in TTY: unavailable in this build".to_owned()
    };
    frame.render_widget(
        Paragraph::new(tty_images_label.clone())
            .style(if !cfg!(feature = "images") {
                theme.muted
            } else if preferences.show_images_in_tty {
                theme.selected
            } else {
                theme.base
            })
            .alignment(Alignment::Center),
        sections[6],
    );
    if cfg!(feature = "images") {
        hit_map.preferences_buttons.push((
            UiAction::ToggleTtyImages,
            Rect::new(
                centered_line_x(sections[6], terminal_text_width(&tty_images_label)),
                sections[6].y,
                terminal_text_width(&tty_images_label).min(sections[6].width),
                1,
            ),
        ));
    }

    let bandcamp_format_label = if cfg!(feature = "bandcamp") {
        format!(
            "[b] Bandcamp audio: {}",
            preferences.bandcamp_audio_format.label()
        )
    } else {
        "Bandcamp audio: unavailable in this build".to_owned()
    };
    frame.render_widget(
        Paragraph::new(bandcamp_format_label.clone())
            .style(if cfg!(feature = "bandcamp") {
                theme.selected
            } else {
                theme.muted
            })
            .alignment(Alignment::Center),
        sections[7],
    );
    if cfg!(feature = "bandcamp") {
        hit_map.preferences_buttons.push((
            UiAction::CycleBandcampAudioFormat,
            Rect::new(
                centered_line_x(sections[7], terminal_text_width(&bandcamp_format_label)),
                sections[7].y,
                terminal_text_width(&bandcamp_format_label).min(sections[7].width),
                1,
            ),
        ));
    }

    let mut notes = format!(
        "Drill-down is the low-width default. Split is useful on wide terminals.\nYouTube preparation keeps one short-lived result in RAM; folder sizes are measured lazily.\nAutomatic YouTube thumbnails use 480×360 through 1366 px, 640×480 through 1920 px, and 1280×720 above it; explicit sizes never fall back.\nTTY images use the pixelated half-block fallback; graphical terminal images are independent.\nBandcamp resolves the selected encoding only after an explicit playback action.\nWill save UI and playback preferences in:\n{}",
        preferences.config_path
    );
    if let Some(variable) = preferences.environment_override.as_deref() {
        notes.push_str(&format!(
            "\n\nLocked by environment: {variable}\nChange or remove it before saving."
        ));
    }
    if let Some(error) = preferences.validation_error.as_deref() {
        notes.push_str(&format!("\n\nError: {error}"));
    }
    frame.render_widget(
        Paragraph::new(notes)
            .style(
                if preferences.validation_error.is_some()
                    || preferences.environment_override.is_some()
                {
                    Style::default().fg(Color::Red)
                } else {
                    theme.muted
                },
            )
            .wrap(Wrap { trim: false }),
        sections[8],
    );

    let buttons = [
        ("[Enter] Save", UiAction::SubmitPreferences),
        ("[Esc] Cancel", UiAction::DismissPreferences),
    ];
    let controls = buttons
        .iter()
        .map(|(label, _)| *label)
        .collect::<Vec<_>>()
        .join("   ");
    frame.render_widget(
        Paragraph::new(controls.as_str())
            .alignment(Alignment::Center)
            .style(theme.accent),
        sections[9],
    );
    let total_width = u16::try_from(Span::raw(&controls).width()).unwrap_or(u16::MAX);
    let mut x = centered_line_x(sections[9], total_width);
    for (label, action) in buttons {
        let width = terminal_text_width(label);
        hit_map
            .preferences_buttons
            .push((action, Rect::new(x, sections[9].y, width, 1)));
        x = x.saturating_add(width).saturating_add(3);
    }
}

fn render_local_file_popup(
    frame: &mut Frame<'_>,
    popup: &LocalFilePopupView,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    if let LocalFilePopupView::Move {
        source_names,
        destination,
        directories,
        selected,
        pending,
        error,
    } = popup
    {
        render_local_move_popup(
            frame,
            source_names,
            destination,
            directories,
            *selected,
            *pending,
            error.as_deref(),
            theme,
            hit_map,
        );
        return;
    }
    let (message, error, confirm_label, confirm_action) = match popup {
        LocalFilePopupView::Rename { error, .. } => (
            "New basename:".to_owned(),
            error.as_deref(),
            "[Enter] Rename",
            UiAction::SubmitLocalRename,
        ),
        LocalFilePopupView::Trash { name, path, error } => (
            format!(
                "Move “{name}” to recoverable system Trash?\nFrom: {path}\nDestination: recoverable system Trash (chosen by the operating system)"
            ),
            error.as_deref(),
            "[Enter] Move to Trash",
            UiAction::ConfirmLocalTrash,
        ),
        LocalFilePopupView::DownloadedTrash { name, path, error } => (
            format!(
                "Move downloaded item “{name}” to recoverable system Trash?\nFrom: {path}\nDestination: recoverable system Trash (chosen by the operating system)"
            ),
            error.as_deref(),
            "[Enter] Move to Trash",
            UiAction::ConfirmDownloadedTrash,
        ),
        LocalFilePopupView::Move { .. } => unreachable!("handled above"),
    };
    let (title, area, message) = match popup {
        LocalFilePopupView::Rename { .. } => (
            " Local entry ",
            centered_rect(66, 28, frame.area()),
            message,
        ),
        LocalFilePopupView::Trash { .. } | LocalFilePopupView::DownloadedTrash { .. } => {
            let width = frame.area().width.saturating_sub(4).clamp(1, 96);
            let message_width = width.saturating_sub(6).max(1);
            let wrapped_message = wrap_text_lines(&message, message_width);
            let message_height = u16::try_from(wrapped_message.len()).unwrap_or(u16::MAX);
            let height = message_height
                .saturating_add(6)
                .clamp(1, frame.area().height.saturating_sub(2).max(1));
            (
                " Move to trash? ",
                centered_sized_rect(width, height, frame.area()),
                wrapped_message.join("\n"),
            )
        }
        LocalFilePopupView::Move { .. } => unreachable!("handled above"),
    };
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(title, theme), area);
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.is_empty() {
        return;
    }
    let sections = Layout::vertical([
        Constraint::Min(2),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(message)
            .style(theme.base)
            .wrap(Wrap { trim: false }),
        sections[0],
    );
    if let LocalFilePopupView::Rename {
        value, cursor_byte, ..
    } = popup
        && let Some(field_area) = rename_field_area(area)
    {
        render_local_rename_field(frame, field_area, value, *cursor_byte, theme);
    }
    if let Some(error) = error {
        frame.render_widget(
            Paragraph::new(error).style(Style::default().fg(Color::Red)),
            sections[1],
        );
    }
    let cancel_label = "[Esc] Cancel";
    let controls = format!("{confirm_label}   {cancel_label}");
    frame.render_widget(
        Paragraph::new(controls.as_str())
            .alignment(Alignment::Center)
            .style(theme.accent),
        sections[2],
    );
    let controls_width = terminal_text_width(&controls);
    let start = centered_line_x(sections[2], controls_width);
    hit_map.local_file_buttons.push((
        confirm_action,
        Rect::new(start, sections[2].y, terminal_text_width(confirm_label), 1),
    ));
    hit_map.local_file_buttons.push((
        UiAction::DismissLocalFilePopup,
        Rect::new(
            start
                .saturating_add(terminal_text_width(confirm_label))
                .saturating_add(3),
            sections[2].y,
            terminal_text_width(cancel_label),
            1,
        ),
    ));
}

#[allow(
    clippy::too_many_arguments,
    reason = "the Move popup keeps controller-owned destination state explicit"
)]
fn render_local_move_popup(
    frame: &mut Frame<'_>,
    source_names: &[String],
    destination: &str,
    directories: &[LocalMoveDestinationView],
    selected: usize,
    pending: bool,
    error: Option<&str>,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let width = frame.area().width.saturating_sub(4).clamp(1, 112);
    let height = frame.area().height.saturating_sub(4).clamp(1, 34);
    let area = centered_sized_rect(width, height, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(" Move ", theme), area);
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.is_empty() {
        return;
    }

    let source_summary = if source_names.is_empty() {
        "Nothing selected".to_owned()
    } else {
        format!(
            "Moving {} entr{}: {}",
            source_names.len(),
            if source_names.len() == 1 { "y" } else { "ies" },
            source_names.join(", ")
        )
    };
    let source_rows = u16::try_from(wrap_text_lines(&source_summary, inner.width).len())
        .unwrap_or(u16::MAX)
        .clamp(1, 3);
    let sections = Layout::vertical([
        Constraint::Length(source_rows),
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(u16::from(error.is_some())),
        Constraint::Length(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(source_summary)
            .style(theme.base)
            .wrap(Wrap { trim: false }),
        sections[0],
    );
    frame.render_widget(
        Paragraph::new(format!("Destination:\n{destination}"))
            .style(theme.muted)
            .wrap(Wrap { trim: false }),
        sections[1],
    );

    let selected = selected.min(directories.len().saturating_sub(1));
    let visible_rows = usize::from(sections[2].height).max(usize::from(!sections[2].is_empty()));
    let first_index = selected
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(directories.len().saturating_sub(visible_rows));
    let rows = directories
        .iter()
        .enumerate()
        .skip(first_index)
        .take(visible_rows)
        .map(|(index, directory)| {
            let style = if index == selected {
                theme.selected.fg(Color::Black)
            } else {
                theme.base
            };
            ListItem::new(format!("  {}/", directory.name)).style(style)
        })
        .collect::<Vec<_>>();
    if pending && directories.is_empty() {
        frame.render_widget(
            Paragraph::new("Reading destination folders…").style(theme.muted),
            sections[2],
        );
    } else if directories.is_empty() {
        frame.render_widget(
            Paragraph::new("No child folders; move into the displayed destination or go back.")
                .style(theme.muted)
                .wrap(Wrap { trim: false }),
            sections[2],
        );
    } else {
        frame.render_widget(List::new(rows), sections[2]);
        hit_map.local_move_rows = sections[2];
        hit_map.local_move_first_index = first_index;
    }

    if let Some(error) = error {
        frame.render_widget(
            Paragraph::new(error).style(Style::default().fg(Color::Red)),
            sections[3],
        );
    }
    let controls = "[Enter] Open folder   [M] Move here   [Esc] Cancel";
    frame.render_widget(
        Paragraph::new(controls)
            .alignment(Alignment::Center)
            .style(theme.accent),
        sections[4],
    );
    let start = centered_line_x(sections[4], terminal_text_width(controls));
    let open_label = "[Enter] Open folder";
    let move_label = "[M] Move here";
    let cancel_label = "[Esc] Cancel";
    let open_width = terminal_text_width(open_label);
    let move_width = terminal_text_width(move_label);
    hit_map.local_file_buttons.push((
        UiAction::ActivateLocalMoveDestination,
        Rect::new(start, sections[4].y, open_width, 1),
    ));
    hit_map.local_file_buttons.push((
        UiAction::ConfirmLocalMoveHere,
        Rect::new(
            start.saturating_add(open_width).saturating_add(3),
            sections[4].y,
            move_width,
            1,
        ),
    ));
    hit_map.local_file_buttons.push((
        UiAction::DismissLocalFilePopup,
        Rect::new(
            start
                .saturating_add(open_width)
                .saturating_add(move_width)
                .saturating_add(6),
            sections[4].y,
            terminal_text_width(cancel_label),
            1,
        ),
    ));
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RenameFieldViewport {
    /// First byte rendered in the horizontally scrolled field.
    start_byte: usize,
    /// Display-cell width hidden to the left of the field.
    scroll_width: u16,
    /// Visible display-cell column of the insertion point.
    cursor_column: u16,
}

/// Returns the one-line basename field inside a rename popup.
fn rename_field_area(popup_area: Rect) -> Option<Rect> {
    let inner = popup_area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    (inner.width > 0 && inner.height >= 2)
        .then(|| Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1))
}

/// Chooses a grapheme-aligned horizontal viewport that keeps the cursor visible.
fn rename_field_viewport(value: &str, cursor_byte: usize, field_width: u16) -> RenameFieldViewport {
    if field_width == 0 {
        return RenameFieldViewport::default();
    }
    let cursor_byte = rename_cursor_boundary(value, cursor_byte);
    let cursor_width = terminal_text_width(&value[..cursor_byte]);
    let desired_scroll = cursor_width.saturating_sub(field_width.saturating_sub(1));
    let mut viewport = RenameFieldViewport {
        cursor_column: cursor_width,
        ..RenameFieldViewport::default()
    };
    if desired_scroll == 0 {
        return viewport;
    }
    for (start_byte, grapheme) in value[..cursor_byte].grapheme_indices(true) {
        viewport.start_byte = start_byte.saturating_add(grapheme.len());
        viewport.scroll_width = viewport
            .scroll_width
            .saturating_add(terminal_text_width(grapheme));
        if viewport.scroll_width >= desired_scroll {
            break;
        }
    }
    viewport.cursor_column = cursor_width.saturating_sub(viewport.scroll_width);
    viewport
}

/// Renders the basename unchanged in a one-line, cursor-following viewport.
fn render_local_rename_field(
    frame: &mut Frame<'_>,
    area: Rect,
    value: &str,
    cursor_byte: usize,
    theme: &Theme,
) {
    let viewport = rename_field_viewport(value, cursor_byte, area.width);
    frame.render_widget(
        Paragraph::new(&value[viewport.start_byte..]).style(theme.base),
        area,
    );
}

/// Places the native terminal cursor over the visible rename insertion point.
///
/// A higher-priority error popup or the keyboard-controlled virtual pointer
/// suppresses this layer. Omitting it from the next frame makes Ratatui hide
/// the terminal cursor automatically.
fn render_local_rename_cursor(frame: &mut Frame<'_>, view: &ViewModel, enabled: bool) {
    if !enabled || view.error_popup.is_some() {
        return;
    }
    let Some(LocalFilePopupView::Rename {
        value, cursor_byte, ..
    }) = view.local_file_popup.as_ref()
    else {
        return;
    };
    let popup_area = centered_rect(66, 28, frame.area());
    let Some(field_area) = rename_field_area(popup_area) else {
        return;
    };
    let viewport = rename_field_viewport(value, *cursor_byte, field_area.width);
    frame.set_cursor_position((
        field_area.x.saturating_add(viewport.cursor_column),
        field_area.y,
    ));
}

/// Clamps a possibly stale rename cursor to the nearest preceding grapheme
/// boundary so rendering never splits UTF-8 or a visible character cluster.
fn rename_cursor_boundary(value: &str, requested: usize) -> usize {
    let requested = requested.min(value.len());
    if requested == value.len() {
        return requested;
    }
    value
        .grapheme_indices(true)
        .map(|(index, _)| index)
        .take_while(|index| *index <= requested)
        .last()
        .unwrap_or_default()
}

fn masked_setup_value(value: &str, width: usize) -> String {
    let length = value.chars().count();
    if length <= width {
        return "*".repeat(length);
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    format!("{}…", "*".repeat(width - 1))
}

fn truncate_setup_value(value: &str, width: usize) -> String {
    let mut characters = value.chars();
    let prefix = characters.by_ref().take(width).collect::<String>();
    if characters.next().is_none() {
        return prefix;
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    format!("{}…", prefix.chars().take(width - 1).collect::<String>())
}

/// Compact action rendered immediately after an internally navigable video URL.
const DESCRIPTION_VIDEO_ACTION_SYMBOL: &str = "↪";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WrappedDescriptionToken {
    /// A contiguous UTF-8 source slice.
    Source { start_byte: usize, end_byte: usize },
    /// An injected action referring to [`DetailView::video_links`].
    VideoAction { link_index: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WrappedSourceLine {
    start_byte: usize,
    end_byte: usize,
    tokens: Vec<WrappedDescriptionToken>,
}

#[derive(Debug)]
struct WrappedSourceLineBuilder {
    fallback_byte: usize,
    width: usize,
    tokens: Vec<WrappedDescriptionToken>,
}

impl WrappedSourceLineBuilder {
    fn new(fallback_byte: usize) -> Self {
        Self {
            fallback_byte,
            width: 0,
            tokens: Vec::new(),
        }
    }

    fn push_source(&mut self, start_byte: usize, end_byte: usize, width: usize) {
        if let Some(WrappedDescriptionToken::Source {
            end_byte: previous_end,
            ..
        }) = self.tokens.last_mut()
            && *previous_end == start_byte
        {
            *previous_end = end_byte;
        } else {
            self.tokens.push(WrappedDescriptionToken::Source {
                start_byte,
                end_byte,
            });
        }
        self.width = self.width.saturating_add(width);
    }

    fn push_video_action(&mut self, link_index: usize, width: usize) {
        self.tokens
            .push(WrappedDescriptionToken::VideoAction { link_index });
        self.width = self.width.saturating_add(width);
    }

    fn finish(self) -> WrappedSourceLine {
        let source_range = self.tokens.iter().filter_map(|token| match token {
            WrappedDescriptionToken::Source {
                start_byte,
                end_byte,
            } => Some((*start_byte, *end_byte)),
            WrappedDescriptionToken::VideoAction { .. } => None,
        });
        let mut start_byte = None;
        let mut end_byte = None;
        for (start, end) in source_range {
            start_byte.get_or_insert(start);
            end_byte = Some(end);
        }
        WrappedSourceLine {
            start_byte: start_byte.unwrap_or(self.fallback_byte),
            end_byte: end_byte.unwrap_or(self.fallback_byte),
            tokens: self.tokens,
        }
    }
}

/// Wraps source text and injected URL actions without losing source byte ranges.
fn wrap_description_source(
    description: &str,
    width: usize,
    video_links: &[DetailVideoLinkView],
) -> Vec<WrappedSourceLine> {
    let width = width.max(1);
    let action_width = usize::from(terminal_text_width(DESCRIPTION_VIDEO_ACTION_SYMBOL));
    let mut action_indexes = video_links
        .iter()
        .enumerate()
        .filter(|(_, link)| {
            link.start_byte < link.end_byte
                && link.end_byte <= description.len()
                && description.is_char_boundary(link.start_byte)
                && description.is_char_boundary(link.end_byte)
        })
        .map(|(index, link)| (link.end_byte, index))
        .collect::<Vec<_>>();
    action_indexes.sort_unstable();

    let mut wrapped = Vec::new();
    let mut source_line_start = 0_usize;
    let mut next_action = 0_usize;
    for raw_line in description.split('\n') {
        let visible_line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let source_line_end = source_line_start.saturating_add(visible_line.len());
        while next_action < action_indexes.len()
            && action_indexes[next_action].0 < source_line_start
        {
            next_action = next_action.saturating_add(1);
        }

        let mut builder = WrappedSourceLineBuilder::new(source_line_start);
        for (byte_offset, character) in visible_line.char_indices() {
            let absolute_byte = source_line_start.saturating_add(byte_offset);
            while next_action < action_indexes.len()
                && action_indexes[next_action].0 == absolute_byte
            {
                let (_, link_index) = action_indexes[next_action];
                if builder.width > 0 && builder.width.saturating_add(action_width) > width {
                    wrapped.push(builder.finish());
                    builder = WrappedSourceLineBuilder::new(absolute_byte);
                }
                builder.push_video_action(link_index, action_width);
                next_action = next_action.saturating_add(1);
            }

            let character_end = absolute_byte.saturating_add(character.len_utf8());
            let character_width = Span::raw(character.to_string()).width();
            if builder.width > 0 && builder.width.saturating_add(character_width) > width {
                wrapped.push(builder.finish());
                builder = WrappedSourceLineBuilder::new(absolute_byte);
            }
            builder.push_source(absolute_byte, character_end, character_width);
        }
        while next_action < action_indexes.len() && action_indexes[next_action].0 == source_line_end
        {
            let (_, link_index) = action_indexes[next_action];
            if builder.width > 0 && builder.width.saturating_add(action_width) > width {
                wrapped.push(builder.finish());
                builder = WrappedSourceLineBuilder::new(source_line_end);
            }
            builder.push_video_action(link_index, action_width);
            next_action = next_action.saturating_add(1);
        }
        wrapped.push(builder.finish());
        source_line_start = source_line_start
            .saturating_add(raw_line.len())
            .saturating_add(1);
    }
    if wrapped.is_empty() {
        wrapped.push(WrappedSourceLine {
            start_byte: 0,
            end_byte: 0,
            tokens: Vec::new(),
        });
    }
    wrapped
}

fn terminal_text_width(value: &str) -> u16 {
    u16::try_from(Span::raw(value).width()).unwrap_or(u16::MAX)
}

fn wrap_diagnostic_report(report: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut wrapped = Vec::new();
    for raw_line in report.split('\n') {
        let raw_line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if raw_line.is_empty() {
            wrapped.push(String::new());
            continue;
        }
        let characters = raw_line.chars().collect::<Vec<_>>();
        for chunk in characters.chunks(width) {
            wrapped.push(chunk.iter().collect());
        }
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
}

/// Returns the number of model rows occupying one currently rendered list page.
///
/// A non-empty sub-row rectangle still represents one selectable row. An empty
/// rectangle produces no page action, which avoids surprising jumps in
/// terminals too small to render the list.
fn visible_main_list_page_rows(hit_map: &HitMap) -> Option<usize> {
    if hit_map.rows.height == 0 {
        return None;
    }
    let row_height = hit_map.rows_row_height.max(1);
    Some(usize::from((hit_map.rows.height / row_height).max(1)))
}

/// Translates one Crossterm key event into the shared vocabulary.
///
/// Returns [`None`] for keys the shared map has no name for, such as media and
/// keypad keys, which the terminal front-end has never bound.
fn key_press(key: KeyEvent) -> Option<KeyPress> {
    // Some terminals report both the press and release for one physical key.
    // Ignoring releases prevents a confirmation key from acting twice.
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let named = match key.code {
        KeyCode::Char(character) => Key::Char(character),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::F(number) => Key::F(number),
        _ => return None,
    };
    Some(KeyPress {
        key: named,
        ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
        alt: key.modifiers.contains(KeyModifiers::ALT),
        shift: key.modifiers.contains(KeyModifiers::SHIFT),
    })
}

/// Reports the rendered popup scroll state the shared map needs for paging.
fn popup_geometry(hit_map: &HitMap) -> PopupGeometry {
    PopupGeometry {
        project_history: ScrollGeometry {
            offset: hit_map.project_history_scroll_offset,
            maximum: hit_map.project_history_scroll_maximum,
            page_lines: hit_map.project_history_page_lines,
        },
        video_comments: ScrollGeometry {
            offset: hit_map.video_comments_scroll_offset,
            maximum: hit_map.video_comments_scroll_maximum,
            page_lines: hit_map.video_comments_page_lines,
        },
    }
}

/// Crossterm-flavoured shims so the renderer's popup tests stay unchanged.
#[cfg(test)]
fn project_history_key_action(
    key: KeyEvent,
    offset: usize,
    maximum: usize,
    page_lines: usize,
) -> Option<UiAction> {
    crate::keymap::project_history_key_action(key_press(key)?, offset, maximum, page_lines)
}

#[cfg(test)]
fn video_comments_key_action(
    key: KeyEvent,
    offset: usize,
    maximum: usize,
    page_lines: usize,
) -> Option<UiAction> {
    crate::keymap::video_comments_key_action(key_press(key)?, offset, maximum, page_lines)
}

#[cfg(test)]
fn key_action(key: KeyEvent, view: &ViewModel) -> Option<UiAction> {
    key_action_with_page_rows(key, view, None, None)
}

/// Maps one key using the current rendered main-list page capacity.
///
/// The mapping itself lives in [`crate::keymap`] so the window applies the
/// same modal precedence. Only the translation from Crossterm is local.
fn key_action_with_page_rows(
    key: KeyEvent,
    view: &ViewModel,
    page_rows: Option<usize>,
    hit_map: Option<&HitMap>,
) -> Option<UiAction> {
    crate::keymap::key_action(
        key_press(key)?,
        view,
        page_rows,
        hit_map.map(popup_geometry),
    )
}

fn mouse_action(mouse: MouseEvent, hit_map: &HitMap, view: &ViewModel) -> Option<UiAction> {
    mouse_action_unfiltered(mouse, hit_map, view)
        .filter(|action| view.external_opener_available || !action.requires_external_opener())
}

/// Maps one pointer event before applying terminal-capability policy.
fn mouse_action_unfiltered(
    mouse: MouseEvent,
    hit_map: &HitMap,
    view: &ViewModel,
) -> Option<UiAction> {
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        // Terminals conventionally reserve Shift-drag for native text
        // selection even while mouse reporting is enabled. Never turn the
        // corresponding events into Youta actions if a terminal forwards them.
        return None;
    }
    if view.error_popup.is_some() {
        return match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => hit_map
                .error_buttons
                .iter()
                .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                .map(|(action, _)| action.clone()),
            MouseEventKind::ScrollDown => {
                Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Lines(3)))
            }
            MouseEventKind::ScrollUp => {
                Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Lines(-3)))
            }
            _ => None,
        };
    }
    if view.project_history_popup.is_some() {
        return match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => hit_map
                .project_history_buttons
                .iter()
                .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                .map(|(action, _)| action.clone()),
            MouseEventKind::ScrollDown
                if contains(hit_map.project_history_text_area, mouse.column, mouse.row) =>
            {
                Some(UiAction::SetProjectHistoryScroll(
                    hit_map
                        .project_history_scroll_offset
                        .saturating_add(3)
                        .min(hit_map.project_history_scroll_maximum),
                ))
            }
            MouseEventKind::ScrollUp
                if contains(hit_map.project_history_text_area, mouse.column, mouse.row) =>
            {
                Some(UiAction::SetProjectHistoryScroll(
                    hit_map.project_history_scroll_offset.saturating_sub(3),
                ))
            }
            _ => None,
        };
    }
    #[cfg(feature = "qr")]
    {
        if view.video_qr_popup.is_some() {
            return match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => hit_map
                    .video_qr_buttons
                    .iter()
                    .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                    .map(|(action, _)| action.clone()),
                _ => None,
            };
        }
    }
    if view.video_comments_popup.is_some() {
        return match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => hit_map
                .video_comments_buttons
                .iter()
                .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                .map(|(action, _)| action.clone()),
            MouseEventKind::ScrollDown
                if contains(hit_map.video_comments_text_area, mouse.column, mouse.row) =>
            {
                Some(UiAction::SetVideoCommentsScroll(
                    hit_map
                        .video_comments_scroll_offset
                        .saturating_add(3)
                        .min(hit_map.video_comments_scroll_maximum),
                ))
            }
            MouseEventKind::ScrollUp
                if contains(hit_map.video_comments_text_area, mouse.column, mouse.row) =>
            {
                Some(UiAction::SetVideoCommentsScroll(
                    hit_map.video_comments_scroll_offset.saturating_sub(3),
                ))
            }
            _ => None,
        };
    }
    if view.private_note_popup.is_some() {
        return match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => hit_map
                .private_note_buttons
                .iter()
                .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                .map(|(action, _)| action.clone()),
            MouseEventKind::ScrollDown
                if contains(hit_map.private_note_text_area, mouse.column, mouse.row) =>
            {
                Some(UiAction::SetPrivateNoteScroll(
                    hit_map
                        .private_note_scroll_offset
                        .saturating_add(3)
                        .min(hit_map.private_note_scroll_maximum),
                ))
            }
            MouseEventKind::ScrollUp
                if contains(hit_map.private_note_text_area, mouse.column, mouse.row) =>
            {
                Some(UiAction::SetPrivateNoteScroll(
                    hit_map.private_note_scroll_offset.saturating_sub(3),
                ))
            }
            _ => None,
        };
    }
    if let Some(popup) = view.queue_popup.as_ref() {
        return match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if contains(hit_map.queue_popup_rows, mouse.column, mouse.row) {
                    let index = hit_map.queue_popup_first_index.saturating_add(usize::from(
                        mouse.row.saturating_sub(hit_map.queue_popup_rows.y),
                    ));
                    (index < popup.items.len()).then_some(UiAction::SelectQueuePopupRow(index))
                } else {
                    hit_map
                        .queue_popup_buttons
                        .iter()
                        .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                        .map(|(action, _)| action.clone())
                }
            }
            MouseEventKind::ScrollDown => Some(UiAction::MoveQueuePopupSelection(1)),
            MouseEventKind::ScrollUp => Some(UiAction::MoveQueuePopupSelection(-1)),
            _ => None,
        };
    }
    if let Some(popup) = view.playlist_popup.as_ref() {
        return match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((field, _)) = hit_map
                    .playlist_popup_fields
                    .iter()
                    .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                {
                    Some(UiAction::SelectPlaylistEditorField(*field))
                } else if contains(hit_map.playlist_popup_rows, mouse.column, mouse.row) {
                    let index = hit_map
                        .playlist_popup_first_index
                        .saturating_add(usize::from(
                            mouse.row.saturating_sub(hit_map.playlist_popup_rows.y),
                        ));
                    (index < popup.playlists.len())
                        .then_some(UiAction::SelectPlaylistPopupRow(index))
                } else {
                    hit_map
                        .playlist_popup_buttons
                        .iter()
                        .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                        .map(|(action, _)| action.clone())
                }
            }
            MouseEventKind::ScrollDown if popup.mode == PlaylistPopupMode::Choose => {
                Some(UiAction::MovePlaylistPopupSelection(1))
            }
            MouseEventKind::ScrollUp if popup.mode == PlaylistPopupMode::Choose => {
                Some(UiAction::MovePlaylistPopupSelection(-1))
            }
            _ => None,
        };
    }
    if view.local_file_popup.is_some() {
        return match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if matches!(
                    view.local_file_popup.as_ref(),
                    Some(LocalFilePopupView::Move { .. })
                ) && contains(hit_map.local_move_rows, mouse.column, mouse.row)
                {
                    let index = hit_map.local_move_first_index.saturating_add(usize::from(
                        mouse.row.saturating_sub(hit_map.local_move_rows.y),
                    ));
                    Some(UiAction::SelectLocalMoveDestination(index))
                } else {
                    hit_map
                        .local_file_buttons
                        .iter()
                        .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                        .map(|(action, _)| action.clone())
                }
            }
            MouseEventKind::ScrollDown
                if matches!(
                    view.local_file_popup.as_ref(),
                    Some(LocalFilePopupView::Move { .. })
                ) =>
            {
                Some(UiAction::MoveLocalMoveDestination(1))
            }
            MouseEventKind::ScrollUp
                if matches!(
                    view.local_file_popup.as_ref(),
                    Some(LocalFilePopupView::Move { .. })
                ) =>
            {
                Some(UiAction::MoveLocalMoveDestination(-1))
            }
            _ => None,
        };
    }
    if view.rss_subscription_popup.is_some() {
        return match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if hit_map
                    .rss_subscription_field
                    .is_some_and(|area| contains(area, mouse.column, mouse.row))
                {
                    Some(UiAction::OpenRssSubscriptionPopup)
                } else {
                    hit_map
                        .rss_subscription_buttons
                        .iter()
                        .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                        .map(|(action, _)| action.clone())
                }
            }
            _ => None,
        };
    }
    if view.yandex_music_setup_popup.is_some() {
        return match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if hit_map
                    .yandex_music_setup_field
                    .is_some_and(|area| contains(area, mouse.column, mouse.row))
                {
                    None
                } else {
                    hit_map
                        .yandex_music_setup_buttons
                        .iter()
                        .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                        .map(|(action, _)| action.clone())
                }
            }
            _ => None,
        };
    }
    if view.youtube_setup_popup.is_some() {
        return match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((field, _)) = hit_map
                    .youtube_setup_fields
                    .iter()
                    .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                {
                    Some(UiAction::SelectYouTubeSetupField(*field))
                } else {
                    hit_map
                        .youtube_setup_buttons
                        .iter()
                        .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                        .map(|(action, _)| action.clone())
                }
            }
            _ => None,
        };
    }
    if view.preferences_popup.is_some() {
        return match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => hit_map
                .preferences_buttons
                .iter()
                .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                .map(|(action, _)| action.clone()),
            _ => None,
        };
    }
    if let Some(area) = hit_map.thumbnail_overlay_area {
        return match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if contains(area, mouse.column, mouse.row) => {
                Some(UiAction::ToggleThumbnailExpansion)
            }
            _ => None,
        };
    }
    if view.text_selection_mode {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(position) =
                    hit_map.details_text_position(mouse.column, mouse.row, false)
                {
                    return Some(UiAction::BeginDetailsTextSelection(position));
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if view
                    .details_text_selection
                    .is_some_and(|selection| selection.dragging)
                {
                    return hit_map
                        .details_text_position(mouse.column, mouse.row, true)
                        .map(UiAction::UpdateDetailsTextSelection);
                }
                return None;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let selection = view
                    .details_text_selection
                    .filter(|selection| selection.dragging)?;
                let focus = hit_map.details_text_position(mouse.column, mouse.row, true)?;
                let finished = DetailsTextSelection {
                    focus,
                    dragging: false,
                    ..selection
                };
                return Some(UiAction::FinishDetailsTextSelection {
                    focus,
                    text: hit_map.selected_details_text(finished),
                });
            }
            _ => {}
        }
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            for (screen, area) in &hit_map.tabs {
                if contains(*area, mouse.column, mouse.row) {
                    return Some(UiAction::ShowScreen(*screen));
                }
            }
            for (action, area) in &hit_map.buttons {
                if contains(*area, mouse.column, mouse.row) {
                    return Some(action.clone());
                }
            }
            if hit_map
                .now_playing
                .is_some_and(|area| contains(area, mouse.column, mouse.row))
            {
                return Some(UiAction::ShowNowPlaying);
            }
            for (action, area) in &hit_map.description_video_actions {
                if contains(*area, mouse.column, mouse.row) {
                    return Some(action.clone());
                }
            }
            for (action, area) in &hit_map.detail_buttons {
                if contains(*area, mouse.column, mouse.row) {
                    return Some(action.clone());
                }
            }
            if hit_map
                .thumbnail_area
                .is_some_and(|area| contains(area, mouse.column, mouse.row))
            {
                return Some(UiAction::ToggleThumbnailExpansion);
            }
            for (action, area) in &hit_map.subscription_source_buttons {
                if contains(*area, mouse.column, mouse.row) {
                    return Some(action.clone());
                }
            }
            for (index, area) in &hit_map.detail_links {
                if contains(*area, mouse.column, mouse.row) {
                    return Some(UiAction::ActivateDetailLink(*index));
                }
            }
            if let Some(target) = hit_map
                .waveform_seek
                .as_ref()
                .filter(|target| contains(target.area, mouse.column, mouse.row))
            {
                let offset = u64::from(mouse.column.saturating_sub(target.area.x));
                let last_column = u64::from(target.area.width.saturating_sub(1));
                let duration_nanos = target.duration.as_nanos();
                let last_instant_nanos = duration_nanos.saturating_sub(1);
                let nanoseconds = u128::from(offset)
                    .saturating_mul(duration_nanos)
                    .checked_div(u128::from(last_column.max(1)))
                    .unwrap_or_default()
                    .min(last_instant_nanos);
                let seconds = u64::try_from(
                    nanoseconds
                        .checked_div(Duration::from_secs(1).as_nanos())
                        .unwrap_or_default(),
                )
                .unwrap_or(u64::MAX);
                return Some(UiAction::ActivateWaveformTimecode {
                    media_id: target.media_id.clone(),
                    generation: target.generation,
                    seconds,
                });
            }
            if contains(hit_map.details_panel, mouse.column, mouse.row) {
                return Some(UiAction::SetDetailsFocus(true));
            }
            if view.playback.seeking_available() && !view.playback.live {
                for (action, area) in &hit_map.seek_markers {
                    if contains(*area, mouse.column, mouse.row) {
                        return Some(action.clone());
                    }
                }
            }
            if view.playback.seeking_available()
                && contains(hit_map.seek_bar, mouse.column, mouse.row)
                && hit_map.seek_bar.width > 1
            {
                let offset = mouse.column.saturating_sub(hit_map.seek_bar.x);
                let percent =
                    f64::from(offset) / f64::from(hit_map.seek_bar.width.saturating_sub(1)) * 100.0;
                return Some(UiAction::SeekPercent(percent.clamp(0.0, 100.0)));
            }
            if contains(hit_map.rows, mouse.column, mouse.row) {
                let relative_row = mouse.row.saturating_sub(hit_map.rows.y);
                let row_height = if hit_map.rows_row_height == 0 {
                    2
                } else {
                    hit_map.rows_row_height
                };
                let index = hit_map
                    .rows_first_index
                    .saturating_add(usize::from(relative_row / row_height));
                if index < view.rows.len() {
                    return Some(UiAction::SelectRow(index));
                }
            }
            if contains(hit_map.subscription_source_rows, mouse.column, mouse.row) {
                let relative_row = mouse.row.saturating_sub(hit_map.subscription_source_rows.y);
                let index = hit_map
                    .subscription_source_first_index
                    .saturating_add(usize::from(relative_row / 2));
                if index < view.subscriptions.sources.len() {
                    return Some(UiAction::SelectSubscriptionSource(index));
                }
            }
            if contains(hit_map.subscription_item_rows, mouse.column, mouse.row) {
                let relative_row = mouse.row.saturating_sub(hit_map.subscription_item_rows.y);
                let index = hit_map
                    .subscription_item_first_index
                    .saturating_add(usize::from(relative_row / 2));
                if index < view.subscriptions.items.len() {
                    return Some(UiAction::SelectSubscriptionItem(index));
                }
            }
            None
        }
        MouseEventKind::ScrollDown => {
            if contains(hit_map.details_panel, mouse.column, mouse.row) {
                Some(UiAction::SetDetailsScroll(
                    hit_map
                        .details_scroll_offset
                        .saturating_add(3)
                        .min(hit_map.details_scroll_maximum),
                ))
            } else if hit_map
                .detail_links
                .iter()
                .any(|(_, area)| contains(*area, mouse.column, mouse.row))
            {
                Some(UiAction::MoveDetailLink(1))
            } else {
                Some(UiAction::MoveSelection(1))
            }
        }
        MouseEventKind::ScrollUp => {
            if contains(hit_map.details_panel, mouse.column, mouse.row) {
                Some(UiAction::SetDetailsScroll(
                    hit_map.details_scroll_offset.saturating_sub(3),
                ))
            } else if hit_map
                .detail_links
                .iter()
                .any(|(_, area)| contains(*area, mouse.column, mouse.row))
            {
                Some(UiAction::MoveDetailLink(-1))
            } else {
                Some(UiAction::MoveSelection(-1))
            }
        }
        _ => None,
    }
}

fn contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x && x < area.right() && y >= area.y && y < area.bottom()
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// Centers a popup with explicit dimensions, clamped to the terminal area.
fn centered_sized_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    )
}

/// Wraps text at an exact terminal-cell width, including unbroken paths.
fn wrap_text_lines(value: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut wrapped = Vec::new();
    for source_line in value.split('\n') {
        let mut line = String::new();
        let mut line_width = 0_usize;
        for character in source_line.chars() {
            let character_width = Span::raw(character.to_string()).width();
            if !line.is_empty() && line_width.saturating_add(character_width) > width {
                wrapped.push(std::mem::take(&mut line));
                line_width = 0;
            }
            line.push(character);
            line_width = line_width.saturating_add(character_width);
        }
        wrapped.push(line);
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
}

/// Renders one compact, borderless main-pane heading and returns its content area.
///
/// Popup panels intentionally continue to use [`panel_block`] so diagnostics and
/// configuration dialogs remain visually distinct from the primary workspace.
/// An empty title does not reserve a row, allowing top-tab context to stand alone.
fn render_main_panel_heading(frame: &mut Frame<'_>, area: Rect, title: &str, style: Style) -> Rect {
    let heading_height = u16::from(area.height > 0 && !title.is_empty());
    if area.width > 0 && heading_height > 0 {
        frame.render_widget(
            Paragraph::new(title).style(style),
            Rect::new(area.x, area.y, area.width, heading_height),
        );
    }
    Rect::new(
        area.x,
        area.y.saturating_add(heading_height),
        area.width,
        area.height.saturating_sub(heading_height),
    )
}

fn panel_block<'a>(title: &'a str, theme: &Theme) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border)
        .title(title)
}

fn button(key: &str, label: &str, show_hotkey: bool) -> String {
    if show_hotkey {
        format!("[{key}] {label}")
    } else {
        label.to_owned()
    }
}

fn source_style(source: &str, theme: &Theme) -> Style {
    let lower = source.to_ascii_lowercase();
    if lower.contains("youtube") || lower.contains("invidious") {
        Style::default().fg(Color::Red)
    } else if lower.contains("podcast") || lower.contains("rss") {
        Style::default().fg(Color::Green)
    } else if lower.contains("local") {
        Style::default().fg(Color::Cyan)
    } else if lower.contains("peertube") {
        Style::default().fg(Color::Magenta)
    } else if lower.contains("radio") {
        Style::default().fg(Color::Yellow)
    } else if lower.contains("mod")
        || lower.contains("tracker")
        || lower.contains("scene.org")
        || lower.contains("aminet")
        || lower.contains("demozoo")
    {
        Style::default().fg(Color::LightBlue)
    } else {
        theme.accent
    }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn download_ratio(downloaded_bytes: u64, total_bytes: u64) -> f64 {
    const RATIO_SCALE: u32 = 10_000;

    if total_bytes == 0 {
        return 0.0;
    }
    let scaled = u128::from(downloaded_bytes.min(total_bytes))
        .saturating_mul(u128::from(RATIO_SCALE))
        / u128::from(total_bytes);
    f64::from(u32::try_from(scaled).unwrap_or(RATIO_SCALE)) / f64::from(RATIO_SCALE)
}

fn human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format_binary_unit(bytes, GIB, "GiB")
    } else if bytes >= MIB {
        format_binary_unit(bytes, MIB, "MiB")
    } else if bytes >= KIB {
        format_binary_unit(bytes, KIB, "KiB")
    } else {
        format!("{bytes} B")
    }
}

fn format_binary_unit(bytes: u64, unit: u64, suffix: &str) -> String {
    let tenths = (u128::from(bytes) * 10 + u128::from(unit / 2)) / u128::from(unit);
    format!("{}.{} {suffix}", tenths / 10, tenths % 10)
}

fn trim_speed(speed: f64) -> String {
    let mut result = format!("{speed:.1}");
    if result.ends_with(".0") {
        result.truncate(result.len() - 2);
    }
    result
}

struct Theme {
    base: Style,
    border: Style,
    heading: Style,
    selected: Style,
    accent: Style,
    /// Restrained terminal-palette pink used by the playing description chapter.
    active_chapter: Style,
    vertical_video: Style,
    /// Dimmed vertical-video title after playback is accepted by the backend.
    vertical_video_started: Style,
    muted: Style,
    cached: Style,
    progress: Style,
}

impl Theme {
    /// Builds a theme whose text styles are stable on the active terminal.
    ///
    /// A Linux virtual console has no dependable true-color or italic text,
    /// so its vertical-video accent uses the closest named ANSI color. The
    /// video-title renderer uses weight and named colors instead of italics.
    fn for_terminal(funny_mode: bool, physical_linux_console: bool) -> Self {
        let mut theme = Self::new(funny_mode);
        if physical_linux_console && !funny_mode {
            theme.vertical_video = Style::default().fg(Color::LightMagenta);
            theme.vertical_video_started = Style::default().fg(Color::Magenta);
        }
        theme
    }

    fn new(funny_mode: bool) -> Self {
        if funny_mode {
            Self {
                base: Style::default().fg(Color::LightGreen),
                border: Style::default().fg(Color::Yellow),
                heading: Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
                selected: Style::default()
                    .fg(Color::Black)
                    .bg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
                accent: Style::default().fg(Color::LightMagenta),
                active_chapter: Style::default().fg(Color::LightMagenta),
                vertical_video: Style::default().fg(Color::LightCyan),
                vertical_video_started: Style::default().fg(Color::Cyan),
                muted: Style::default().fg(Color::DarkGray),
                cached: Style::default().fg(Color::DarkGray).bg(Color::Reset),
                progress: Style::default().fg(Color::LightMagenta).bg(Color::Black),
            }
        } else {
            Self {
                base: Style::default(),
                border: Style::default().fg(Color::DarkGray),
                heading: Style::default().add_modifier(Modifier::BOLD),
                selected: Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                accent: Style::default().fg(Color::Cyan),
                // ANSI magenta follows the terminal's configured palette and
                // remains available without true-color support.
                active_chapter: Style::default().fg(Color::Magenta),
                vertical_video: Style::default().fg(Color::Rgb(255, 105, 180)),
                vertical_video_started: Style::default()
                    .fg(Color::Rgb(255, 105, 180))
                    .add_modifier(Modifier::DIM),
                muted: Style::default().fg(Color::DarkGray),
                cached: Style::default().fg(Color::DarkGray).bg(Color::Reset),
                progress: Style::default().fg(Color::Cyan),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::config::{BandcampAudioFormat, YouTubeThumbnailSize};
    use crate::domain::{MediaKind, SourceKind};
    use crate::waveform::PeakPyramid;

    /// Cloneable byte sink used to inspect Crossterm's emitted SGR ordering.
    #[derive(Clone, Default)]
    struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct MockThumbnailRenderer {
        enabled: bool,
        synchronized: Vec<(Option<url::Url>, Rect)>,
        synchronized_local_videos: Vec<(LocalVideoThumbnailView, Rect)>,
        prefetch_batches: Vec<Vec<url::Url>>,
        obscure_count: usize,
        clear_count: usize,
        pending: bool,
        immediate_redraw: bool,
        rendered_artwork: bool,
        prepared_artwork_size: Option<Size>,
        rendered_areas: Vec<Rect>,
        poll_results: VecDeque<bool>,
        poll_count: usize,
        tty_image_preferences: Vec<bool>,
    }

    impl ThumbnailRenderer for MockThumbnailRenderer {
        fn set_tty_images_enabled(&mut self, enabled: bool) -> bool {
            self.tty_image_preferences.push(enabled);
            true
        }

        fn poll(&mut self) -> bool {
            self.poll_count = self.poll_count.saturating_add(1);
            let changed = self.poll_results.pop_front().unwrap_or(false);
            if changed {
                self.pending = false;
            }
            changed
        }

        fn is_enabled(&self) -> bool {
            self.enabled
        }

        fn is_pending(&self) -> bool {
            self.pending
        }

        fn needs_immediate_redraw(&self) -> bool {
            self.immediate_redraw
        }

        fn has_rendered_artwork(&self) -> bool {
            self.rendered_artwork
        }

        fn prepared_artwork_area(&self, available: Rect) -> Option<Rect> {
            self.prepared_artwork_size.map(|size| {
                Rect::new(
                    available.x,
                    available.y,
                    size.width.min(available.width),
                    size.height.min(available.height),
                )
            })
        }

        fn synchronize(&mut self, source: Option<&url::Url>, area: Rect) -> bool {
            self.synchronized.push((source.cloned(), area));
            true
        }

        fn synchronize_local_video(
            &mut self,
            source: &LocalVideoThumbnailView,
            area: Rect,
        ) -> bool {
            self.synchronized_local_videos.push((source.clone(), area));
            true
        }

        fn synchronize_prefetch(&mut self, sources: &[url::Url]) -> bool {
            self.prefetch_batches.push(sources.to_vec());
            true
        }

        fn obscure(&mut self) -> bool {
            self.obscure_count = self.obscure_count.saturating_add(1);
            false
        }

        fn clear(&mut self) -> bool {
            self.clear_count = self.clear_count.saturating_add(1);
            true
        }

        fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
            self.rendered_areas.push(area);
            frame.render_widget(
                Paragraph::new("THUMBNAIL IMAGE")
                    .style(theme.accent)
                    .alignment(Alignment::Center),
                area,
            );
        }
    }

    #[test]
    fn view_model_hides_chapter_timestamps_by_default() {
        assert!(!ViewModel::default().show_chapter_timestamps);
    }

    #[test]
    fn view_model_applies_tty_image_preference_to_the_live_renderer() {
        let mut renderer = MockThumbnailRenderer::default();
        let hidden = ViewModel {
            show_images_in_tty: false,
            ..ViewModel::default()
        };

        assert!(synchronize_tty_image_preference(&hidden, &mut renderer));
        assert_eq!(renderer.tty_image_preferences, [false]);

        let visible = ViewModel::default();
        assert!(synchronize_tty_image_preference(&visible, &mut renderer));
        assert_eq!(renderer.tty_image_preferences, [false, true]);
    }

    #[cfg(feature = "images")]
    #[test]
    fn live_tty_image_toggle_restores_the_suspended_halfblock_manager() {
        use crate::thumbnails::tests as thumbnail_tests;

        let directory = tempfile::tempdir().expect("temporary cache parent");
        let cache_directory = directory.path().join("thumbnail-cache");
        let manager = thumbnail_tests::halfblock_manager_for_tui(Some(cache_directory.clone()));
        let mut renderer = TerminalThumbnailRenderer::new_with_runtime_policy(
            manager,
            ThumbnailMode::Auto,
            Some(cache_directory.clone()),
            PathBuf::from("ffmpeg"),
            true,
            true,
        );

        assert_eq!(
            renderer.manager.capability(),
            ThumbnailCapability::Supported(ThumbnailProtocol::Halfblocks)
        );
        assert!(renderer.set_tty_images_enabled(false));
        assert_eq!(renderer.manager.capability(), ThumbnailCapability::Disabled);
        assert!(!renderer.is_enabled());
        assert!(renderer.suspended_tty_manager.is_some());
        assert!(!renderer.set_tty_images_enabled(false));

        assert!(renderer.set_tty_images_enabled(true));
        assert_eq!(
            renderer.manager.capability(),
            ThumbnailCapability::Supported(ThumbnailProtocol::Halfblocks)
        );
        assert!(renderer.is_enabled());
        assert!(renderer.suspended_tty_manager.is_none());
        assert_eq!(renderer.mode, ThumbnailMode::Auto);
        assert_eq!(
            renderer.cache_directory.as_deref(),
            Some(cache_directory.as_path())
        );
        assert!(
            !cache_directory.exists(),
            "toggling an idle manager must neither initialize the cache nor perform network work"
        );
        assert!(!renderer.set_tty_images_enabled(true));
    }

    #[test]
    fn f8_virtual_cursor_moves_clamps_and_clicks_existing_hitboxes() {
        let mut cursor = VirtualCursor {
            bounds: Rect::new(2, 1, 5, 3),
            ..VirtualCursor::default()
        };
        assert_eq!(
            cursor.handle_key(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE)),
            VirtualCursorKey::Consumed
        );
        assert_eq!((cursor.column, cursor.row), (4, 2));
        assert_eq!(
            cursor.handle_key(KeyEvent::new_with_kind(
                KeyCode::F(8),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )),
            VirtualCursorKey::Consumed
        );
        assert!(cursor.active, "an F8 key release must not toggle twice");

        for _ in 0..10 {
            assert_eq!(
                cursor.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
                VirtualCursorKey::Consumed
            );
            assert_eq!(
                cursor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
                VirtualCursorKey::Consumed
            );
        }
        assert_eq!((cursor.column, cursor.row), (2, 1));

        let click = match cursor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            VirtualCursorKey::Click(mouse) => mouse,
            outcome => panic!("expected a virtual click, got {outcome:?}"),
        };
        let hit_map = HitMap {
            buttons: vec![(UiAction::ToggleHelp, Rect::new(2, 1, 1, 1))],
            ..HitMap::default()
        };
        assert_eq!(
            mouse_action(click, &hit_map, &ViewModel::default()),
            Some(UiAction::ToggleHelp)
        );

        assert_eq!(
            cursor.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            VirtualCursorKey::Consumed
        );
        assert!(!cursor.active);
        assert_eq!(
            cursor.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            VirtualCursorKey::PassThrough
        );
    }

    fn activate_virtual_cursor_and_take_gpm_notice(
        cursor: &mut VirtualCursor,
        availability: ConsolePointerAvailability,
    ) -> Option<GpmUnavailableNotice> {
        let was_active = cursor.active;
        assert_eq!(
            cursor.handle_key(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE)),
            VirtualCursorKey::Consumed
        );
        cursor.take_gpm_unavailable_notice(was_active, availability)
    }

    #[test]
    fn terminal_input_reports_compiled_gpm_support_exactly() {
        assert_eq!(
            TerminalInput::gpm_supported(),
            cfg!(all(feature = "gpm", target_os = "linux"))
        );
    }

    #[test]
    fn gpm_reconnect_requires_explicit_f8_supported_physical_console_disconnect() {
        let disconnected_console = ConsolePointerAvailability {
            physical_linux_console: true,
            gpm_supported: true,
            gpm_connected: false,
            openrc_managed: false,
        };
        assert!(gpm_reconnect_needed(true, disconnected_console));
        assert!(!gpm_reconnect_needed(false, disconnected_console));
        assert!(!gpm_reconnect_needed(
            true,
            ConsolePointerAvailability {
                gpm_connected: true,
                ..disconnected_console
            }
        ));
        assert!(!gpm_reconnect_needed(
            true,
            ConsolePointerAvailability {
                physical_linux_console: false,
                ..disconnected_console
            }
        ));
        assert!(!gpm_reconnect_needed(
            true,
            ConsolePointerAvailability {
                gpm_supported: false,
                ..disconnected_console
            }
        ));
    }

    #[test]
    fn optional_input_retry_factory_replaces_disconnect_and_retains_connection() {
        let mut input: Option<&'static str> = None;
        let mut factory_calls = 0;

        assert!(!retry_optional_input_with(&mut input, || {
            factory_calls += 1;
            None
        }));
        assert!(input.is_none());
        assert_eq!(factory_calls, 1);

        assert!(retry_optional_input_with(&mut input, || {
            factory_calls += 1;
            Some("connected")
        },));
        assert_eq!(input, Some("connected"));
        assert_eq!(factory_calls, 2);

        assert!(!retry_optional_input_with(&mut input, || {
            factory_calls += 1;
            Some("forced connection")
        }));
        assert_eq!(
            input,
            Some("connected"),
            "an existing GPM slot must not be replaced"
        );
        assert_eq!(
            factory_calls, 2,
            "the reconnect factory must not run while connected"
        );
    }

    #[test]
    fn f8_keeps_keyboard_pointer_silent_when_gpm_is_connected() {
        let mut cursor = VirtualCursor::default();

        assert_eq!(
            activate_virtual_cursor_and_take_gpm_notice(
                &mut cursor,
                ConsolePointerAvailability {
                    physical_linux_console: true,
                    gpm_supported: true,
                    gpm_connected: true,
                    openrc_managed: true,
                },
            ),
            None
        );
        assert!(cursor.active, "F8 must still activate the keyboard pointer");
    }

    #[test]
    fn f8_reports_unavailable_gpm_as_openrc_managed() {
        let mut cursor = VirtualCursor::default();

        assert_eq!(
            activate_virtual_cursor_and_take_gpm_notice(
                &mut cursor,
                ConsolePointerAvailability {
                    physical_linux_console: true,
                    gpm_supported: true,
                    gpm_connected: false,
                    openrc_managed: true,
                },
            ),
            Some(GpmUnavailableNotice {
                gpm_supported: true,
                openrc_managed: true,
            })
        );
    }

    #[test]
    fn f8_reports_unavailable_gpm_without_an_unrelated_service_command() {
        let mut cursor = VirtualCursor::default();

        assert_eq!(
            activate_virtual_cursor_and_take_gpm_notice(
                &mut cursor,
                ConsolePointerAvailability {
                    physical_linux_console: true,
                    gpm_supported: true,
                    gpm_connected: false,
                    openrc_managed: false,
                },
            ),
            Some(GpmUnavailableNotice {
                gpm_supported: true,
                openrc_managed: false,
            })
        );
    }

    #[test]
    fn f8_reports_a_build_without_gpm_without_openrc_advice() {
        let mut cursor = VirtualCursor::default();

        assert_eq!(
            activate_virtual_cursor_and_take_gpm_notice(
                &mut cursor,
                ConsolePointerAvailability {
                    physical_linux_console: true,
                    gpm_supported: false,
                    gpm_connected: false,
                    openrc_managed: true,
                },
            ),
            Some(GpmUnavailableNotice {
                gpm_supported: false,
                openrc_managed: false,
            })
        );
    }

    #[test]
    fn f8_does_not_report_gpm_on_a_pty() {
        let mut cursor = VirtualCursor::default();

        assert_eq!(
            activate_virtual_cursor_and_take_gpm_notice(
                &mut cursor,
                ConsolePointerAvailability {
                    physical_linux_console: false,
                    gpm_supported: true,
                    gpm_connected: false,
                    openrc_managed: true,
                },
            ),
            None
        );
        assert!(cursor.active, "F8 must retain its normal PTY behavior");
    }

    #[test]
    fn f8_reports_unavailable_gpm_only_once_per_run() {
        let mut cursor = VirtualCursor::default();
        let unavailable = ConsolePointerAvailability {
            physical_linux_console: true,
            gpm_supported: true,
            gpm_connected: false,
            openrc_managed: true,
        };

        assert!(activate_virtual_cursor_and_take_gpm_notice(&mut cursor, unavailable).is_some());
        assert_eq!(
            cursor.handle_key(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE)),
            VirtualCursorKey::Consumed
        );
        assert!(!cursor.active);
        assert_eq!(
            activate_virtual_cursor_and_take_gpm_notice(&mut cursor, unavailable),
            None
        );
        assert!(cursor.active);
    }

    #[test]
    fn virtual_cursor_renders_a_reversed_square_over_an_empty_cell() {
        let backend = TestBackend::new(7, 3);
        let mut terminal = Terminal::new(backend).expect("create virtual-cursor terminal");
        let mut cursor = VirtualCursor {
            active: true,
            column: 3,
            row: 1,
            bounds: Rect::new(0, 0, 7, 3),
            ..VirtualCursor::default()
        };

        terminal
            .draw(|frame| cursor.render(frame))
            .expect("render virtual cursor");

        let cell = &terminal.backend().buffer()[(3, 1)];
        assert_eq!(cell.symbol(), "■");
        assert!(cell.modifier.contains(Modifier::BOLD));
        assert!(cell.modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn active_virtual_cursor_follows_physical_mouse_cells_within_the_terminal() {
        let mut cursor = VirtualCursor {
            active: false,
            column: 2,
            row: 1,
            bounds: Rect::new(1, 1, 6, 3),
            ..VirtualCursor::default()
        };
        let moved = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 5,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };

        cursor.follow_mouse(&moved);
        assert_eq!((cursor.column, cursor.row), (2, 1));

        cursor.active = true;
        cursor.follow_mouse(&moved);
        assert_eq!((cursor.column, cursor.row), (5, 2));

        cursor.follow_mouse(&MouseEvent {
            column: u16::MAX,
            row: u16::MAX,
            ..moved
        });
        assert_eq!((cursor.column, cursor.row), (6, 3));
    }

    /// Collects the test backend's current cells for whole-frame assertions.
    fn rendered_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    /// Builds the structured yt-dlp failure used by keyboard and renderer tests.
    fn yt_dlp_forbidden_error(gentoo: bool) -> ErrorPopupView {
        ErrorPopupView {
            title: "Playback failed".to_owned(),
            report: "COPY_ONLY_DIAGNOSTIC_REPORT".to_owned(),
            yt_dlp_forbidden: Some(YtDlpForbiddenView {
                project_url: YT_DLP_PROJECT_URL.to_owned(),
                installed: YtDlpVersionLookupView::Available {
                    version: "2026.07.04".to_owned(),
                    released_on: Some("2026-07-04".to_owned()),
                },
                github_latest: YtDlpVersionLookupView::Loading,
                gentoo: gentoo.then(|| YtDlpGentooVersionView {
                    arch: "amd64".to_owned(),
                    package_url: GENTOO_YT_DLP_PACKAGE_URL.to_owned(),
                    latest_stable: YtDlpVersionLookupView::Unavailable {
                        reason: "package metadata unavailable".to_owned(),
                    },
                }),
            }),
            ..ErrorPopupView::default()
        }
    }

    /// Verifies the intentionally small, source-independent footer action set.
    fn assert_minimal_footer_actions(hit_map: &HitMap) {
        let expected = [
            UiAction::BeginSearch,
            UiAction::ToggleAutoplay,
            UiAction::MoveSelection(-1),
            UiAction::MoveSelection(1),
            UiAction::ChangeVolume(5),
            UiAction::ChangeVolume(-5),
            UiAction::OpenPreferences,
            UiAction::ToggleHelp,
        ];

        assert_eq!(hit_map.buttons.len(), expected.len());
        for action in expected {
            assert!(
                hit_map
                    .buttons
                    .iter()
                    .any(|(candidate, target)| candidate == &action && target.width > 0),
                "missing minimal footer action {action:?}"
            );
        }
    }

    #[test]
    fn terminal_startup_sets_youta_title_before_entering_the_ui() {
        let mut output = Vec::new();

        write_terminal_startup(&mut output).expect("write terminal startup sequence");

        assert!(
            output.starts_with(b"\x1b]0;Youta\x07"),
            "terminal title must be the first startup command: {output:?}"
        );
        assert!(
            output
                .windows(b"\x1b[?1049h".len())
                .any(|window| window == b"\x1b[?1049h"),
            "startup must still enter the alternate screen: {output:?}"
        );
        let alternate_screen = output
            .windows(b"\x1b[?1049h".len())
            .position(|window| window == b"\x1b[?1049h")
            .expect("alternate-screen command");
        let reset_style = output
            .windows(b"\x1b[0m".len())
            .position(|window| window == b"\x1b[0m")
            .expect("startup style reset");
        let clear_screen = output
            .windows(b"\x1b[2J".len())
            .position(|window| window == b"\x1b[2J")
            .expect("clear-screen command");
        let cursor_home = output
            .windows(b"\x1b[1;1H".len())
            .position(|window| window == b"\x1b[1;1H")
            .expect("cursor-home command");
        assert!(
            alternate_screen < reset_style
                && reset_style < clear_screen
                && clear_screen < cursor_home,
            "startup must reset style, clear the active UI buffer, and move to its origin: {output:?}"
        );
        assert!(
            output
                .windows(b"\x1b[?1006l".len())
                .all(|window| window != b"\x1b[?1006l"),
            "Details selection must keep SGR mouse reporting enabled: {output:?}"
        );
    }

    #[test]
    fn active_search_uses_the_existing_playing_redraw_cadence() {
        let settings = UiSettings {
            idle_tick: Duration::from_secs(2),
            playing_tick: Duration::from_millis(250),
            ..UiSettings::default()
        };
        let mut view = ViewModel::default();

        assert_eq!(event_wait(&view, &settings), Duration::from_secs(2));
        view.search_activity = Some(SearchActivity::YouTube);
        assert_eq!(event_wait(&view, &settings), Duration::from_millis(250));
        view.search_activity = None;
        view.subscriptions.loading = true;
        assert_eq!(event_wait(&view, &settings), Duration::from_millis(250));
        view.subscriptions.loading = false;
        view.playback_starting = true;
        assert_eq!(event_wait(&view, &settings), Duration::from_millis(250));
        view.playback_starting = false;
        view.playback.paused = false;
        assert_eq!(event_wait(&view, &settings), Duration::from_millis(250));

        let zero_settings = UiSettings {
            idle_tick: Duration::ZERO,
            playing_tick: Duration::ZERO,
            ..UiSettings::default()
        };
        assert_eq!(
            event_wait(&ViewModel::default(), &zero_settings),
            Duration::from_millis(1)
        );
    }

    #[test]
    fn pending_local_folder_open_uses_the_interactive_response_budget() {
        let settings = UiSettings {
            idle_tick: Duration::from_secs(2),
            playing_tick: Duration::from_millis(250),
            ..UiSettings::default()
        };
        let mut view = ViewModel {
            local_browse_pending: true,
            ..ViewModel::default()
        };

        assert_eq!(
            event_wait(&view, &settings),
            LOCAL_BROWSE_RESPONSE_POLL_INTERVAL
        );

        view.local_browse_pending = false;
        assert_eq!(event_wait(&view, &settings), Duration::from_secs(2));
    }

    #[test]
    fn pending_local_artwork_uses_the_interactive_response_budget() {
        let settings = UiSettings {
            idle_tick: Duration::from_secs(2),
            playing_tick: Duration::from_millis(250),
            ..UiSettings::default()
        };
        let view = ViewModel {
            local_artwork_pending: true,
            ..ViewModel::default()
        };

        assert_eq!(
            event_wait(&view, &settings),
            LOCAL_BROWSE_RESPONSE_POLL_INTERVAL
        );
    }

    #[test]
    fn pending_local_waveform_uses_the_interactive_response_budget() {
        let settings = UiSettings {
            idle_tick: Duration::from_secs(2),
            playing_tick: Duration::from_millis(250),
            ..UiSettings::default()
        };
        let view = ViewModel {
            waveform: WaveformView::Loading {
                media_id: MediaId::new(SourceKind::Local, "/music/track.mp3"),
            },
            ..ViewModel::default()
        };

        assert_eq!(
            event_wait(&view, &settings),
            LOCAL_BROWSE_RESPONSE_POLL_INTERVAL
        );
    }

    #[test]
    fn thumbnail_wait_probes_cache_hits_early_and_redraws_on_completion() {
        let mut thumbnails = MockThumbnailRenderer {
            pending: true,
            poll_results: VecDeque::from([false, true]),
            ..MockThumbnailRenderer::default()
        };
        let mut waits = Vec::new();

        let outcome =
            wait_for_event_or_thumbnail(Duration::from_secs(2), Some(&mut thumbnails), |wait| {
                waits.push(wait);
                Ok(false)
            })
            .expect("wait for cached thumbnail");

        assert_eq!(outcome, WaitOutcome::ThumbnailRedraw);
        assert_eq!(waits, [Duration::from_millis(25)]);
        assert_eq!(thumbnails.poll_count, 2);
    }

    #[test]
    fn thumbnail_wait_keeps_idle_and_long_network_work_low_frequency() {
        let mut idle = MockThumbnailRenderer::default();
        let mut idle_waits = Vec::new();
        assert_eq!(
            wait_for_event_or_thumbnail(Duration::from_secs(2), Some(&mut idle), |wait| {
                idle_waits.push(wait);
                Ok(false)
            })
            .expect("idle terminal wait"),
            WaitOutcome::Timeout
        );
        assert_eq!(idle_waits, [Duration::from_secs(2)]);
        assert_eq!(idle.poll_count, 0);

        let mut loading = MockThumbnailRenderer {
            pending: true,
            ..MockThumbnailRenderer::default()
        };
        let mut loading_waits = Vec::new();
        assert_eq!(
            wait_for_event_or_thumbnail(Duration::from_secs(1), Some(&mut loading), |wait| {
                loading_waits.push(wait);
                Ok(false)
            },)
            .expect("network thumbnail wait"),
            WaitOutcome::Timeout
        );
        assert_eq!(
            loading_waits,
            [
                Duration::from_millis(25),
                Duration::from_millis(25),
                Duration::from_millis(50),
                Duration::from_millis(100),
                Duration::from_millis(250),
                Duration::from_millis(250),
                Duration::from_millis(250),
                Duration::from_millis(50),
            ]
        );
    }

    #[test]
    fn thumbnail_wait_prioritizes_terminal_input_and_clear_followup_frames() {
        let mut loading = MockThumbnailRenderer {
            pending: true,
            ..MockThumbnailRenderer::default()
        };
        let mut event_waits = Vec::new();
        assert_eq!(
            wait_for_event_or_thumbnail(Duration::from_secs(1), Some(&mut loading), |wait| {
                event_waits.push(wait);
                Ok(true)
            },)
            .expect("terminal event wait"),
            WaitOutcome::TerminalEvent
        );
        assert_eq!(event_waits, [Duration::from_millis(25)]);

        let mut followup = MockThumbnailRenderer {
            immediate_redraw: true,
            ..MockThumbnailRenderer::default()
        };
        assert_eq!(
            wait_for_event_or_thumbnail(Duration::from_secs(1), Some(&mut followup), |_| panic!(
                "a required followup frame must not block"
            ),)
            .expect("immediate thumbnail followup"),
            WaitOutcome::ThumbnailRedraw
        );
    }

    #[test]
    fn search_panel_title_cycles_ascii_frames_only_on_the_matching_screen() {
        let mut view = ViewModel {
            search_query: "ambient".to_owned(),
            search_activity: Some(SearchActivity::YouTube),
            ..ViewModel::default()
        };

        for (frame, symbol) in ['|', '/', '-', '\\'].into_iter().enumerate() {
            view.search_animation_frame = frame;
            assert_eq!(search_panel_title(&view), format!(" {symbol} ambient "));
        }

        view.screen = Screen::TrackerMusic;
        assert_eq!(search_panel_title(&view), " ambient ");
        view.search_activity = Some(SearchActivity::TrackerArchives);
        view.search_animation_frame = 0;
        assert_eq!(search_panel_title(&view), " | ambient ");
        view.search_activity = None;
        assert_eq!(search_panel_title(&view), " ambient ");
        view.screen = Screen::Bandcamp;
        view.search_activity = Some(SearchActivity::Bandcamp);
        assert_eq!(search_panel_title(&view), " | ambient ");
        view.screen = Screen::ApplePodcasts;
        view.search_activity = Some(SearchActivity::ApplePodcasts);
        assert_eq!(search_panel_title(&view), " | ambient ");
        view.screen = Screen::LibriVox;
        view.search_activity = Some(SearchActivity::LibriVox);
        assert_eq!(search_panel_title(&view), " | ambient ");
    }

    #[test]
    fn top_left_panel_titles_do_not_repeat_the_active_tab() {
        let mut view = ViewModel::default();
        assert_eq!(search_panel_title(&view), " Video search ");
        view.search_kind = SearchKind::Channels;
        assert_eq!(search_panel_title(&view), " Channel search ");

        for screen in [
            Screen::YouTubeMusic,
            Screen::Bandcamp,
            Screen::ApplePodcasts,
            Screen::LibriVox,
            Screen::TrackerMusic,
        ] {
            view.screen = screen;
            view.search_query.clear();
            assert_eq!(
                search_panel_title(&view),
                " Search ",
                "{screen:?} must not repeat its source name"
            );
            view.search_query = "fixture query".to_owned();
            assert_eq!(
                search_panel_title(&view),
                " fixture query ",
                "{screen:?} results must show the query alone"
            );
        }

        view.screen = Screen::Search;
        assert_eq!(search_panel_title(&view), " fixture query ");
        view.screen = Screen::Radio;
        assert_eq!(
            search_panel_title(&view),
            " Filter: fixture query ",
            "Radio must expose its accepted local filter without repeating the tab name"
        );
        view.search_cursor_byte = view.search_query.len();
        view.search_editing = true;
        assert_eq!(
            search_panel_title(&view),
            " Filter: fixture query▏ ",
            "Radio's live editor must describe itself as a filter"
        );
        view.search_editing = false;
        for screen in [
            Screen::Subscriptions,
            Screen::Local,
            Screen::Playlists,
            Screen::Downloaded,
            Screen::History,
            Screen::Statistics,
        ] {
            view.screen = screen;
            view.local_path = "/fixture/music".to_owned();
            view.search_query = "stale YouTube query".to_owned();
            assert_eq!(
                search_panel_title(&view),
                "",
                "{screen:?} must rely on its active tab label despite stale search state"
            );
        }
    }

    #[test]
    fn yandex_music_search_title_and_help_expose_the_selected_scope() {
        let mut view = ViewModel {
            screen: Screen::YandexMusic,
            ..ViewModel::default()
        };
        view.yandex_music_route = YandexMusicRouteView::Recommendations;
        view.search_query = "retained search draft".to_owned();
        assert_eq!(
            search_panel_title(&view),
            " My Wave ",
            "recommendations must not be mislabeled with a retained search query"
        );

        view.yandex_music_route = YandexMusicRouteView::Search;
        for (kind, title) in [
            (YandexMusicSearchKind::All, "All"),
            (YandexMusicSearchKind::Music, "Music"),
            (YandexMusicSearchKind::Podcasts, "Podcasts"),
            (YandexMusicSearchKind::Audiobooks, "Audiobooks"),
        ] {
            view.yandex_music_search_kind = kind;
            view.search_query.clear();
            assert_eq!(search_panel_title(&view), format!(" {title} search "));

            view.search_query = "fixture query".to_owned();
            assert_eq!(
                search_panel_title(&view),
                format!(" {title} · fixture query ")
            );

            view.search_editing = true;
            view.search_cursor_byte = view.search_query.len();
            assert_eq!(
                search_panel_title(&view),
                format!(" {title} search: fixture query▏ ")
            );
            view.search_editing = false;
        }

        view.yandex_music_route = YandexMusicRouteView::Album;
        assert_eq!(search_panel_title(&view), " Album ");

        view.yandex_music_route = YandexMusicRouteView::Artist;
        assert_eq!(search_panel_title(&view), " Artist ");
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &view),
            Some(UiAction::GoBack),
            "Esc must return from an internally opened artist"
        );

        assert_eq!(
            search_kind_help(&view),
            "  v all/music/podcasts/audiobooks search"
        );
        view.screen = Screen::Search;
        assert_eq!(
            search_kind_help(&view),
            "  v video/channel search     N relevance/newest     C CC-only videos"
        );
    }

    #[test]
    fn yandex_music_search_defaults_to_the_complete_catalogue() {
        assert_eq!(YandexMusicSearchKind::default().label(), "all");
    }

    #[test]
    fn yandex_music_details_render_source_specific_actions_with_exact_hit_targets() {
        let mut view = ViewModel {
            screen: Screen::YandexMusic,
            external_opener_available: true,
            details: Some(DetailView {
                media_id: Some(MediaId::new(SourceKind::YandexMusic, "303")),
                title: "Fixture Track".to_owned(),
                length: "3:03".to_owned(),
                links: vec![DetailLinkView {
                    url: "https://music.yandex.com/album/404/track/303".to_owned(),
                    presentation: DetailLinkPresentation::UrlOnly,
                    ..DetailLinkView::default()
                }],
                ..DetailView::default()
            }),
            yandex_music_actions: YandexMusicActionsView {
                track_selected: true,
                artist_available: true,
                album_available: true,
                album_open: true,
                twenty_recommendations_available: true,
                reaction: YandexMusicReactionView::Neutral,
            },
            ..ViewModel::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(180, 34)).expect("Yandex Music terminal");
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw Yandex Music actions");
        let rendered = rendered_text(&terminal);
        for label in [
            "[o] xdg-open track",
            "[L] Like",
            "[X] Dislike",
            "[g] Open artist",
            "[b] Open album",
            "[Shift+D] Download album",
            "[R] Download 20 recommendations",
        ] {
            assert!(
                rendered.contains(label),
                "Yandex Music details omitted {label:?}:\n{rendered}"
            );
        }
        assert!(!rendered.contains("xdg-open video"));
        assert!(
            !rendered.contains("Length: 3:03"),
            "Yandex Music duration is already visible in the selected row and player"
        );
        for action in [
            UiAction::OpenInBrowser,
            UiAction::ToggleYandexMusicLike,
            UiAction::ToggleYandexMusicDislike,
            UiAction::OpenYandexMusicArtist,
            UiAction::OpenYandexMusicAlbum,
            UiAction::DownloadYandexMusicAlbum,
            UiAction::DownloadTwentyYandexMusicRecommendations,
        ] {
            let target = hit_map
                .detail_buttons
                .iter()
                .find_map(|(candidate, target)| (candidate == &action).then_some(*target))
                .unwrap_or_else(|| panic!("missing hit target for {action:?}"));
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: target.x,
                        row: target.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                Some(action)
            );
        }
        assert!(!rendered.contains("OAuth token"));
        let like_row = hit_map
            .detail_buttons
            .iter()
            .find_map(|(action, area)| {
                (*action == UiAction::ToggleYandexMusicLike).then_some(area.y)
            })
            .expect("Like row");
        let dislike_row = hit_map
            .detail_buttons
            .iter()
            .find_map(|(action, area)| {
                (*action == UiAction::ToggleYandexMusicDislike).then_some(area.y)
            })
            .expect("Dislike row");
        assert!(
            dislike_row > like_row,
            "Dislike must remain after Like in visual order"
        );
        let artist_row = hit_map
            .detail_buttons
            .iter()
            .find_map(|(action, area)| {
                (*action == UiAction::OpenYandexMusicArtist).then_some(area.y)
            })
            .expect("Open artist row");
        let album_row = hit_map
            .detail_buttons
            .iter()
            .find_map(|(action, area)| {
                (*action == UiAction::OpenYandexMusicAlbum).then_some(area.y)
            })
            .expect("Open album row");
        assert!(
            artist_row < album_row,
            "Open artist must remain before Open album in visual order"
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &view),
            Some(UiAction::OpenYandexMusicArtist)
        );

        view.yandex_music_actions = YandexMusicActionsView {
            album_available: true,
            ..YandexMusicActionsView::default()
        };
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw Yandex Music album actions");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("[o] xdg-open item"));
        assert!(!rendered.contains("xdg-open track"));
        assert!(!rendered.contains("[g] Open artist"));
        assert_ne!(
            key_action(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &view),
            Some(UiAction::OpenYandexMusicArtist),
            "the artist action must require an exact selected artist"
        );
    }

    #[test]
    fn yandex_artist_album_and_source_links_have_spaced_exact_mouse_targets() {
        let source_url = "https://music.yandex.ru/album/404/track/303";
        let mut view = ViewModel {
            screen: Screen::YandexMusic,
            external_opener_available: true,
            details: Some(DetailView {
                links: vec![
                    DetailLinkView {
                        prefix: "Artist: ".to_owned(),
                        label: "First Artist".to_owned(),
                        url: "https://music.yandex.ru/artist/101".to_owned(),
                        internal_target: Some(DetailLinkInternalTarget::YandexMusicArtist(
                            "101".to_owned(),
                        )),
                        presentation: DetailLinkPresentation::LabelOnlySpaced,
                        ..DetailLinkView::default()
                    },
                    DetailLinkView {
                        prefix: "Album: ".to_owned(),
                        label: "Fixture Album".to_owned(),
                        url: "https://music.yandex.ru/album/404".to_owned(),
                        internal_target: Some(DetailLinkInternalTarget::YandexMusicAlbum(
                            "404".to_owned(),
                        )),
                        presentation: DetailLinkPresentation::LabelOnlySpaced,
                        ..DetailLinkView::default()
                    },
                    DetailLinkView {
                        label: String::new(),
                        url: source_url.to_owned(),
                        presentation: DetailLinkPresentation::UrlOnlySpaced,
                        ..DetailLinkView::default()
                    },
                ],
                description: "Type: Music".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(160, 30)).expect("Yandex link terminal");
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw Yandex links");

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Artist: First Artist"));
        assert!(rendered.contains("Album: Fixture Album"));
        assert!(!rendered.contains("Yandex Music —"));
        assert!(!rendered.contains("Artist: First Artist —"));
        assert!(!rendered.contains("Album: Fixture Album —"));
        assert!(rendered.contains(source_url));
        assert!(rendered.contains("Type: Music"));
        assert_eq!(rendered.matches('↪').count(), 2);
        let targets = hit_map
            .detail_links
            .iter()
            .map(|(index, area)| (*index, *area))
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 3);
        assert_eq!(targets[1].1.y, targets[0].1.y.saturating_add(2));
        assert_eq!(targets[2].1.y, targets[1].1.y.saturating_add(2));
        assert_eq!(targets[0].1.width, terminal_text_width("First Artist"));
        assert_eq!(targets[1].1.width, terminal_text_width("Fixture Album"));
        assert_eq!(targets[2].1.width, terminal_text_width(source_url));
        assert_eq!(
            targets[0].1.x,
            hit_map
                .details_panel
                .x
                .saturating_add(terminal_text_width("Artist: ")),
            "the plain Artist prefix must not belong to the external target"
        );
        assert_eq!(
            targets[1].1.x,
            hit_map
                .details_panel
                .x
                .saturating_add(terminal_text_width("Album: ")),
            "the plain Album prefix must not belong to the external target"
        );
        let click = |column, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        for (index, area) in targets.iter().copied() {
            assert_eq!(
                mouse_action(click(area.x, area.y), &hit_map, &view),
                Some(UiAction::ActivateDetailLink(index))
            );
        }
        for (action, external_area) in [
            (
                UiAction::OpenYandexMusicArtistById("101".to_owned()),
                targets[0].1,
            ),
            (
                UiAction::OpenYandexMusicAlbumById("404".to_owned()),
                targets[1].1,
            ),
        ] {
            let marker_area = hit_map
                .detail_buttons
                .iter()
                .find_map(|(candidate, area)| (candidate == &action).then_some(*area))
                .unwrap_or_else(|| panic!("missing internal marker for {action:?}"));
            assert_eq!(marker_area.width, terminal_text_width("↪"));
            assert_eq!(marker_area.x, external_area.right().saturating_add(1));
            assert_eq!(
                mouse_action(click(marker_area.x, marker_area.y), &hit_map, &view),
                Some(action)
            );
        }
        for (prefix, target) in [("Artist", targets[0].1), ("Album", targets[1].1)] {
            assert_eq!(
                mouse_action(click(hit_map.details_panel.x, target.y), &hit_map, &view,),
                Some(UiAction::SetDetailsFocus(true)),
                "the {prefix} prefix must remain plain, non-clickable text"
            );
        }
        assert_eq!(
            mouse_action(
                click(targets[0].1.x, targets[0].1.y.saturating_add(1)),
                &hit_map,
                &view,
            ),
            Some(UiAction::SetDetailsFocus(true)),
            "the separator row must not activate either adjacent link"
        );

        let type_row = hit_map
            .detail_text_rows
            .iter()
            .find(|row| row.cells.concat().contains("Type: Music"))
            .expect("Type details row");
        assert_eq!(
            type_row.y,
            targets[2].1.y.saturating_add(2),
            "the source URL must retain one blank row before Type"
        );

        view.external_opener_available = false;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw internal Yandex links without a browser");
        assert!(hit_map.detail_links.is_empty());
        for action in [
            UiAction::OpenYandexMusicArtistById("101".to_owned()),
            UiAction::OpenYandexMusicAlbumById("404".to_owned()),
        ] {
            assert!(
                hit_map
                    .detail_buttons
                    .iter()
                    .any(|(candidate, _)| candidate == &action),
                "internal navigation must remain clickable without an external opener"
            );
        }
    }

    #[test]
    fn yandex_wikidata_disclosure_follows_its_artist_before_the_blank_row() {
        let view = ViewModel {
            screen: Screen::YandexMusic,
            external_opener_available: true,
            details: Some(DetailView {
                links: vec![
                    DetailLinkView {
                        prefix: "Artist: ".to_owned(),
                        label: "Fixture Artist".to_owned(),
                        url: "https://music.yandex.ru/artist/101".to_owned(),
                        internal_target: Some(DetailLinkInternalTarget::YandexMusicArtist(
                            "101".to_owned(),
                        )),
                        presentation: DetailLinkPresentation::LabelOnlySpaced,
                        ..DetailLinkView::default()
                    },
                    DetailLinkView {
                        label: "Fixture artist item (Q101)".to_owned(),
                        url: "https://www.wikidata.org/wiki/Q101".to_owned(),
                        wikidata_item_id: Some("Q101".to_owned()),
                        presentation: DetailLinkPresentation::LabelAndUrlSpaced,
                        ..DetailLinkView::default()
                    },
                    DetailLinkView {
                        prefix: "Album: ".to_owned(),
                        label: "Fixture Album".to_owned(),
                        url: "https://music.yandex.ru/album/404".to_owned(),
                        internal_target: Some(DetailLinkInternalTarget::YandexMusicAlbum(
                            "404".to_owned(),
                        )),
                        presentation: DetailLinkPresentation::LabelOnlySpaced,
                        ..DetailLinkView::default()
                    },
                ],
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(140, 20)).expect("Yandex Wikidata TUI");
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw grouped Yandex Wikidata");

        let row_for = |index| {
            hit_map
                .detail_links
                .iter()
                .find_map(|(candidate, area)| (*candidate == index).then_some(area.y))
                .unwrap_or_else(|| panic!("missing Details link {index}"))
        };
        let artist_row = row_for(0);
        let wikidata_row = row_for(1);
        let album_row = row_for(2);
        assert_eq!(wikidata_row, artist_row.saturating_add(1));
        assert_eq!(album_row, wikidata_row.saturating_add(2));
    }

    #[test]
    fn youtube_search_editor_renders_and_moves_its_cursor_without_seeking() {
        let mut view = ViewModel {
            screen: Screen::Search,
            search_editing: true,
            search_query: "alpha omega".to_owned(),
            search_cursor_byte: "alpha ".len(),
            playback: PlaybackStatus {
                idle: false,
                duration: Some(Duration::from_secs(120)),
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };
        assert_eq!(search_panel_title(&view), " Search: alpha ▏omega ");
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &view),
            Some(UiAction::MoveSearchCursor(-1))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &view),
            Some(UiAction::MoveSearchCursor(1))
        );

        view.search_query = "A👩‍💻B".to_owned();
        view.search_cursor_byte = "A👩".len();
        assert!(
            search_panel_title(&view).contains("A▏👩‍💻B"),
            "a synthetic non-boundary cursor must clamp before the complete UTF-8 scalar"
        );
    }

    #[test]
    fn headingless_top_left_screens_reclaim_the_first_content_row() {
        for screen in [
            Screen::Radio,
            Screen::Local,
            Screen::Playlists,
            Screen::Downloaded,
            Screen::History,
            Screen::Statistics,
        ] {
            let backend = TestBackend::new(100, 12);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let view = ViewModel {
                screen,
                local_path: "/fixture/music".to_owned(),
                rows: vec![RowView {
                    title: format!("{screen:?} fixture"),
                    compact: true,
                    ..RowView::default()
                }],
                ..ViewModel::default()
            };
            let mut hit_map = HitMap::default();

            terminal
                .draw(|frame| {
                    render_body(
                        frame,
                        frame.area(),
                        &view,
                        true,
                        DEFAULT_THUMBNAIL_HEIGHT,
                        &Theme::new(false),
                        &mut hit_map,
                        None,
                    );
                })
                .expect("draw headingless screen");

            assert_eq!(
                hit_map.rows.y, 0,
                "{screen:?} must not reserve a duplicate heading row"
            );
            assert!(
                rendered_text(&terminal).contains(&format!("{screen:?} fixture")),
                "{screen:?} first row must remain visible"
            );
        }
    }

    #[test]
    fn paused_marker_follows_playing_media_instead_of_selection() {
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let playing = MediaId::new(crate::domain::SourceKind::YouTube, "playing");
        let selected = MediaId::new(crate::domain::SourceKind::YouTube, "selected");
        let mut view = ViewModel {
            rows: vec![
                RowView {
                    media_id: Some(playing.clone()),
                    title: "Playing row".to_owned(),
                    source: "YouTube".to_owned(),
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(selected),
                    title: "Selected row".to_owned(),
                    source: "YouTube".to_owned(),
                    ..RowView::default()
                },
            ],
            selected: 1,
            playing_media_id: Some(playing),
            playback: PlaybackStatus {
                idle: false,
                paused: true,
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_body(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    &Theme::new(false),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw independent playing and selected rows");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 1)].symbol(), "⏸");
        assert_eq!(buffer[(0, 1)].fg, Color::Cyan);
        assert_eq!(buffer[(0, 3)].symbol(), " ");
        assert_eq!(buffer[(0, 3)].bg, Color::Cyan);
        assert_eq!(
            buffer
                .content()
                .iter()
                .filter(|cell| cell.symbol() == "⏸")
                .count(),
            1
        );

        view.playback.paused = false;
        terminal
            .draw(|frame| {
                render_body(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    &Theme::new(false),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw playing-state marker");
        assert_eq!(terminal.backend().buffer()[(0, 1)].symbol(), "▶");
    }

    #[test]
    fn duplicate_history_identity_marks_only_the_newest_matching_row_as_playing() {
        let backend = TestBackend::new(100, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let playing = MediaId::new(SourceKind::YouTube, "duplicate-history-item");
        let view = ViewModel {
            screen: Screen::History,
            rows: vec![
                RowView {
                    media_id: Some(playing.clone()),
                    title: "Newest History occurrence".to_owned(),
                    source: "YouTube".to_owned(),
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(playing.clone()),
                    title: "Older History occurrence".to_owned(),
                    source: "YouTube".to_owned(),
                    ..RowView::default()
                },
            ],
            playing_media_id: Some(playing),
            playback: PlaybackStatus {
                idle: false,
                paused: false,
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_body(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    &Theme::new(false),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw duplicate History identities");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(hit_map.rows.x, hit_map.rows.y)].symbol(), "▶");
        assert_ne!(
            buffer[(hit_map.rows.x, hit_map.rows.y.saturating_add(2))].symbol(),
            "▶"
        );
        assert_eq!(
            buffer
                .content()
                .iter()
                .filter(|cell| cell.symbol() == "▶")
                .count(),
            1
        );
    }

    #[test]
    fn vertical_video_titles_keep_playing_and_selection_visibility() {
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let playing = MediaId::new(SourceKind::YouTube, "playing-vertical");
        let view = ViewModel {
            rows: vec![
                RowView {
                    media_id: Some(playing.clone()),
                    title: "Playing vertical".to_owned(),
                    source: "YouTube".to_owned(),
                    vertical: true,
                    watched_percent: 50,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "idle-vertical")),
                    title: "Vertical idle".to_owned(),
                    source: "YouTube".to_owned(),
                    vertical: true,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "selected-vertical")),
                    title: "Selected vertical".to_owned(),
                    source: "YouTube".to_owned(),
                    vertical: true,
                    watched_percent: 95,
                    ..RowView::default()
                },
            ],
            selected: 2,
            playing_media_id: Some(playing),
            playback: PlaybackStatus {
                idle: false,
                paused: false,
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_body(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    &Theme::new(false),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw vertical-video colors");
        let buffer = terminal.backend().buffer();
        let playing_title = &buffer[(4, 1)];
        assert_eq!(playing_title.symbol(), "P");
        assert_eq!(playing_title.fg, Color::Rgb(255, 105, 180));
        assert!(!playing_title.modifier.contains(Modifier::BOLD));
        assert!(playing_title.modifier.contains(Modifier::DIM));
        assert!(!playing_title.modifier.contains(Modifier::ITALIC));
        let idle_title = &buffer[(4, 3)];
        assert_eq!(idle_title.symbol(), "V");
        assert_eq!(idle_title.fg, Color::Rgb(255, 105, 180));
        assert!(idle_title.modifier.contains(Modifier::BOLD));
        assert!(!idle_title.modifier.contains(Modifier::DIM));
        assert!(!idle_title.modifier.contains(Modifier::ITALIC));
        let selected_title = &buffer[(4, 5)];
        assert_eq!(selected_title.symbol(), "S");
        assert_eq!(selected_title.fg, Color::Black);
        assert_eq!(selected_title.bg, Color::Cyan);
        assert!(selected_title.modifier.contains(Modifier::BOLD));
        assert!(!selected_title.modifier.contains(Modifier::DIM));
        assert!(!selected_title.modifier.contains(Modifier::ITALIC));
        assert_eq!(buffer[(0, 1)].symbol(), "▶");
    }

    #[test]
    fn youtube_titles_replace_watched_markers_with_weight_and_brightness() {
        let backend = TestBackend::new(120, 16);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            rows: vec![
                RowView {
                    title: "Selected channel source".to_owned(),
                    source: "YouTube channel".to_owned(),
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "regular-unplayed")),
                    title: "Regular unplayed".to_owned(),
                    source: "YouTube".to_owned(),
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "regular-started")),
                    title: "Regular started".to_owned(),
                    source: "YouTube".to_owned(),
                    playback_started: true,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "short-unplayed")),
                    title: "Short unplayed".to_owned(),
                    source: "YouTube".to_owned(),
                    vertical: true,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "short-started")),
                    title: "Short started".to_owned(),
                    source: "YouTube".to_owned(),
                    playback_started: true,
                    vertical: true,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "music-started")),
                    title: "Music started".to_owned(),
                    source: "YouTube Music".to_owned(),
                    watched_percent: 50,
                    playback_started: true,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "restored-started")),
                    title: "Restored without video metadata".to_owned(),
                    source: SourceKind::YouTube.to_string(),
                    watched_percent: 50,
                    playback_started: true,
                    ..RowView::default()
                },
            ],
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_body(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    &Theme::new(false),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw YouTube title playback states");
        let buffer = terminal.backend().buffer();
        let regular_unplayed = &buffer[(4, 3)];
        let regular_started = &buffer[(4, 5)];
        let short_unplayed = &buffer[(4, 7)];
        let short_started = &buffer[(4, 9)];

        assert_eq!(regular_unplayed.symbol(), "R");
        assert!(regular_unplayed.modifier.contains(Modifier::BOLD));
        assert_eq!(regular_started.symbol(), "R");
        assert_eq!(regular_started.fg, Color::DarkGray);
        assert!(!regular_started.modifier.contains(Modifier::BOLD));
        assert!(!regular_started.modifier.contains(Modifier::ITALIC));
        assert_eq!(short_unplayed.symbol(), "S");
        assert_eq!(short_unplayed.fg, Color::Rgb(255, 105, 180));
        assert!(short_unplayed.modifier.contains(Modifier::BOLD));
        assert!(!short_unplayed.modifier.contains(Modifier::DIM));
        assert_eq!(short_started.symbol(), "S");
        assert_eq!(short_started.fg, Color::Rgb(255, 105, 180));
        assert!(!short_started.modifier.contains(Modifier::BOLD));
        assert!(short_started.modifier.contains(Modifier::DIM));
        assert!(!short_started.modifier.contains(Modifier::ITALIC));
        for row in [3_u16, 5, 7, 9] {
            assert!(
                (0..4).all(|column| !matches!(buffer[(column, row)].symbol(), "●" | "◐" | "○")),
                "YouTube video row {row} must not reserve a watched-state circle"
            );
        }
        assert_eq!(buffer[(4, 11)].symbol(), "◐");
        assert_eq!(buffer[(6, 11)].symbol(), "M");
        assert!(buffer[(6, 11)].modifier.contains(Modifier::ITALIC));
        assert_eq!(buffer[(4, 13)].symbol(), "◐");
        assert_eq!(buffer[(6, 13)].symbol(), "R");
        assert!(buffer[(6, 13)].modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn active_chapter_colors_use_terminal_palette_pinks() {
        assert_eq!(
            Theme::new(false).active_chapter.fg,
            Some(Color::Magenta),
            "the normal theme must use adaptable ANSI magenta, not fixed RGB"
        );
        assert_eq!(
            Theme::new(true).active_chapter.fg,
            Some(Color::LightMagenta),
            "the DOS theme keeps its brighter terminal-palette accent"
        );
        assert_eq!(
            Theme::for_terminal(false, true).vertical_video.fg,
            Some(Color::LightMagenta),
            "the physical Linux console must not receive a true-color text style"
        );
        assert_eq!(
            Theme::for_terminal(false, true).vertical_video_started.fg,
            Some(Color::Magenta),
            "a started Short needs a visibly darker Linux-console pink"
        );
        assert!(
            !Theme::for_terminal(false, true)
                .vertical_video_started
                .add_modifier
                .contains(Modifier::DIM),
            "the Linux console strips DIM, so its started Short style must use color"
        );
    }

    #[test]
    fn selected_row_has_no_play_marker_when_nothing_is_playing() {
        let backend = TestBackend::new(100, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            rows: vec![RowView {
                media_id: Some(MediaId::new(crate::domain::SourceKind::YouTube, "selected")),
                title: "Selected row".to_owned(),
                source: "YouTube".to_owned(),
                ..RowView::default()
            }],
            selected: 0,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_body(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    &Theme::new(false),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw selected idle row");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 1)].symbol(), " ");
        assert_eq!(buffer[(0, 1)].bg, Color::Cyan);
        assert!(buffer.content().iter().all(|cell| cell.symbol() != "▶"));
    }

    #[test]
    fn list_rows_show_subscription_and_video_title_state_independently() {
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            rows: vec![
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "unwatched")),
                    title: "Unsubscribed unwatched row".to_owned(),
                    source: "YouTube".to_owned(),
                    subscribed: false,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "partial")),
                    title: "Unsubscribed partial row".to_owned(),
                    source: "YouTube".to_owned(),
                    watched_percent: 90,
                    subscribed: false,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "watched")),
                    title: "Subscribed watched row".to_owned(),
                    source: "YouTube".to_owned(),
                    watched_percent: 91,
                    subscribed: true,
                    ..RowView::default()
                },
                RowView {
                    title: "Subscribed channel source".to_owned(),
                    source: "YouTube channel".to_owned(),
                    subscribed: true,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::Radio, "live")),
                    title: "Live item without watched state".to_owned(),
                    source: "Radio".to_owned(),
                    watched_percent: 50,
                    hide_watched_marker: true,
                    ..RowView::default()
                },
            ],
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_body(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    &Theme::new(false),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw subscription markers");
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(
            !rendered.contains('◇'),
            "unsubscribed rows must not show a hollow subscription marker"
        );
        assert_eq!(
            buffer[(2, 1)].symbol(),
            " ",
            "the unsubscribed row keeps alignment without a visible marker"
        );
        assert_eq!(
            buffer[(4, 1)].symbol(),
            "U",
            "an unplayed video title reclaims the watched-marker column"
        );
        assert!(buffer[(4, 1)].modifier.contains(Modifier::BOLD));
        assert_eq!(
            buffer[(4, 3)].symbol(),
            "U",
            "a partially watched video title starts in the reclaimed column"
        );
        assert_eq!(buffer[(4, 3)].fg, Color::DarkGray);
        assert!(!buffer[(4, 3)].modifier.contains(Modifier::BOLD));
        assert_eq!(
            buffer[(2, 5)].symbol(),
            "◆",
            "a locally subscribed row keeps the solid subscription marker"
        );
        assert_eq!(
            buffer[(4, 5)].symbol(),
            "S",
            "a completed video title follows its subscription marker directly"
        );
        assert_eq!(buffer[(4, 5)].fg, Color::DarkGray);
        assert!(!buffer[(4, 5)].modifier.contains(Modifier::BOLD));
        assert_eq!(
            buffer[(4, 7)].symbol(),
            "S",
            "a non-playable channel title follows the subscription column directly"
        );
        assert_eq!(
            buffer[(4, 9)].symbol(),
            "L",
            "a live item suppresses watched-state spacing as well as its marker"
        );
        assert!(
            !buffer[(4, 1)].modifier.contains(Modifier::ITALIC),
            "unplayed titles remain roman"
        );
        assert!(
            !buffer[(4, 3)].modifier.contains(Modifier::ITALIC),
            "partially watched video titles use brightness rather than italics"
        );
        assert!(
            !buffer[(4, 5)].modifier.contains(Modifier::ITALIC),
            "completed video titles use brightness rather than italics"
        );
        assert!(
            !buffer[(4, 9)].modifier.contains(Modifier::ITALIC),
            "suppressed live progress cannot italicize a title"
        );
        assert!(
            !rendered.contains(['●', '◐', '○']),
            "YouTube video rows must not expose watched-state circles"
        );
        assert!(
            rendered.contains(" 90%"),
            "exactly 90 percent remains partial and visible"
        );
        assert!(
            rendered.contains(" 91%"),
            "completed rows retain their exact watched percentage"
        );
    }

    #[test]
    fn two_line_progress_follows_duration_without_using_title_width() {
        let width = 80;
        let backend = TestBackend::new(width, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let full_width_title = "T".repeat(usize::from(width.saturating_sub(4)));
        let rows = vec![
            RowView {
                media_id: Some(MediaId::new(SourceKind::YouTube, "duration-last")),
                title: full_width_title,
                subtitle: "Channel · 12:34".to_owned(),
                source: "YouTube".to_owned(),
                watched_percent: 42,
                ..RowView::default()
            },
            RowView {
                media_id: Some(MediaId::new(SourceKind::Local, "duration-middle")),
                title: "Local duration in the middle".to_owned(),
                subtitle: "Artist · 3:21 · Opus".to_owned(),
                source: "Local".to_owned(),
                watched_percent: 57,
                ..RowView::default()
            },
            RowView {
                media_id: Some(MediaId::new(SourceKind::YouTube, "no-duration")),
                title: "No duration".to_owned(),
                subtitle: "Channel · date unavailable".to_owned(),
                source: "YouTube".to_owned(),
                watched_percent: 63,
                ..RowView::default()
            },
            RowView {
                media_id: Some(MediaId::new(SourceKind::YouTube, "zero-progress")),
                title: "Zero progress".to_owned(),
                subtitle: "Channel · 0:42".to_owned(),
                source: "YouTube".to_owned(),
                ..RowView::default()
            },
            RowView {
                media_id: Some(MediaId::new(SourceKind::Radio, "hidden-progress")),
                title: "Hidden progress".to_owned(),
                subtitle: "Stream · 1:00".to_owned(),
                source: "Radio".to_owned(),
                watched_percent: 88,
                hide_watched_marker: true,
                ..RowView::default()
            },
        ];

        terminal
            .draw(|frame| {
                render_row_list(
                    frame,
                    frame.area(),
                    "",
                    &rows,
                    true,
                    0,
                    None,
                    false,
                    None,
                    true,
                    Theme::new(false).heading,
                    &Theme::new(false),
                );
            })
            .expect("draw two-line progress metadata");
        let buffer = terminal.backend().buffer();
        let line = |row| {
            (0..width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        };

        assert_eq!(
            buffer[(width - 1, 0)].symbol(),
            "T",
            "progress must not consume the primary title line"
        );
        assert!(!line(0).contains('%'));
        assert_eq!(
            line(1),
            "    YouTube · Channel · 12:34 42%",
            "YouTube progress follows its duration-last metadata"
        );
        assert_eq!(
            line(3),
            "    Local · Artist · 3:21 57% · Opus",
            "Local progress follows a duration without reordering later technical metadata"
        );
        assert_eq!(
            line(5),
            "    YouTube · Channel · date unavailable 63%",
            "metadata without a duration receives progress at the end"
        );
        assert!(!line(7).contains('%'));
        assert!(!line(9).contains('%'));
    }

    #[test]
    fn compact_progress_stays_on_one_line_after_the_subtitle() {
        let backend = TestBackend::new(80, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let rows = vec![RowView {
            media_id: Some(MediaId::new(SourceKind::ApplePodcasts, "compact")),
            title: "Compact episode".to_owned(),
            subtitle: "Publisher · 4:03".to_owned(),
            source: "Apple Podcasts".to_owned(),
            watched_percent: 35,
            compact: true,
            ..RowView::default()
        }];

        terminal
            .draw(|frame| {
                render_row_list(
                    frame,
                    frame.area(),
                    "",
                    &rows,
                    true,
                    0,
                    None,
                    false,
                    None,
                    true,
                    Theme::new(false).heading,
                    &Theme::new(false),
                );
            })
            .expect("draw compact progress metadata");
        let rendered = rendered_text(&terminal);

        assert!(rendered.contains("◐ Compact episode · Publisher · 4:03 35%"));
        assert!(!rendered.contains("Compact episode 35% · Publisher"));
    }

    #[test]
    fn accepted_zero_position_start_dims_title_without_zero_percent() {
        let backend = TestBackend::new(100, 8);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            rows: vec![
                RowView {
                    title: "Selected channel source".to_owned(),
                    source: "YouTube channel".to_owned(),
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "accepted-start")),
                    title: "Accepted at zero".to_owned(),
                    source: "YouTube".to_owned(),
                    playback_started: true,
                    ..RowView::default()
                },
            ],
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_body(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    &Theme::new(false),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw accepted zero-position start");
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert_eq!(buffer[(4, 3)].symbol(), "A");
        assert_eq!(buffer[(4, 3)].fg, Color::DarkGray);
        assert!(!buffer[(4, 3)].modifier.contains(Modifier::BOLD));
        assert!(!buffer[(4, 3)].modifier.contains(Modifier::ITALIC));
        assert!(!rendered.contains(['●', '◐', '○']));
        assert!(
            !rendered.contains("   0%"),
            "accepted playback must not fabricate a visible percentage"
        );
    }

    #[test]
    fn physical_linux_console_uses_weight_and_named_colors_for_video_states() {
        let backend = TestBackend::new(100, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            physical_linux_console: true,
            rows: vec![
                RowView {
                    title: "Selected channel source".to_owned(),
                    source: "YouTube channel".to_owned(),
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "tty-started")),
                    title: "Started on a Linux console".to_owned(),
                    source: "YouTube".to_owned(),
                    playback_started: true,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "tty-short-unplayed")),
                    title: "Unplayed Short on a Linux console".to_owned(),
                    source: "YouTube".to_owned(),
                    vertical: true,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "tty-short-started")),
                    title: "Started Short on a Linux console".to_owned(),
                    source: "YouTube".to_owned(),
                    playback_started: true,
                    vertical: true,
                    ..RowView::default()
                },
            ],
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_body(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    &Theme::for_terminal(false, true),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw physical-console playback state");
        let buffer = terminal.backend().buffer();

        let regular_started = &buffer[(4, 3)];
        assert_eq!(regular_started.symbol(), "S");
        assert_eq!(regular_started.fg, Color::DarkGray);
        assert!(!regular_started.modifier.contains(Modifier::BOLD));
        assert!(
            !regular_started.modifier.contains(Modifier::ITALIC),
            "the Linux console must use title brightness instead of unsupported italics"
        );
        let short_unplayed = &buffer[(4, 5)];
        assert_eq!(short_unplayed.symbol(), "U");
        assert_eq!(short_unplayed.fg, Color::LightMagenta);
        assert!(short_unplayed.modifier.contains(Modifier::BOLD));
        let short_started = &buffer[(4, 7)];
        assert_eq!(short_started.symbol(), "S");
        assert_eq!(short_started.fg, Color::Magenta);
        assert!(!short_started.modifier.contains(Modifier::BOLD));
        assert!(!short_started.modifier.contains(Modifier::DIM));
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| { !matches!(cell.symbol(), "●" | "◐" | "○") })
        );
    }

    #[test]
    fn main_panels_are_borderless_and_details_reclaim_the_generic_heading_row() {
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            rows: vec![RowView {
                title: "Borderless result".to_owned(),
                source: "YouTube".to_owned(),
                ..RowView::default()
            }],
            details: Some(DetailView {
                description: "Borderless details content".to_owned(),
                ..DetailView::default()
            }),
            details_focused: true,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_body(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    &Theme::new(false),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw borderless main panels");
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        for border in ['┌', '┐', '└', '┘', '─', '│'] {
            assert!(
                !rendered.contains(border),
                "main panes must not render the {border} box-border glyph"
            );
        }
        assert!(rendered.contains("Video search"));
        assert!(!rendered.contains("YouTube video search"));
        assert!(rendered.contains("Borderless result"));
        assert!(rendered.contains("Borderless details content"));
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .all(|(action, _)| action != &UiAction::ToggleTextSelectionMode),
            "text selection must not consume right-panel space"
        );
        assert_eq!(hit_map.rows.y, 1);
        assert_eq!(hit_map.rows.bottom(), buffer.area.bottom());
        assert_eq!(hit_map.rows.right(), hit_map.details_panel.x);
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: hit_map.rows.x,
                    row: hit_map.rows.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SelectRow(0))
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: hit_map.details_panel.x,
                    row: hit_map.details_panel.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SetDetailsFocus(true))
        );
    }

    #[test]
    fn popup_panels_keep_their_box_borders() {
        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| render_help(frame, &ViewModel::default(), &Theme::new(false)))
            .expect("draw bordered help popup");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("Youta help"));
        assert!(
            rendered.contains("w waveform"),
            "the reserved roadmap shortcut must remain discoverable"
        );
        assert!(rendered.contains("Details: Alt+←/→ history"));
        assert!(rendered.contains("Alt+↑/↓ (Linux TTY: Alt+u/d)"));
        assert!(rendered.contains("Backspace back"));
        if cfg!(feature = "local-browser") {
            assert!(rendered.contains("Local: Esc parent"));
            assert!(rendered.contains("PageUp/Down page"));
        } else {
            assert!(!rendered.contains("Local: Esc parent"));
            assert!(!rendered.contains("PageUp/Down page"));
        }
        assert!(rendered.contains("Playlists: e edit selected playlist     Esc or Backspace up"));
        assert!(rendered.contains("l toggle todo"));
        assert!(rendered.contains("P choose playlist"));
        assert!(rendered.contains("t Details-only text selection"));
        assert!(rendered.contains(&format!("Youta v{}", env!("CARGO_PKG_VERSION"))));
        assert!(rendered.contains(env!("CARGO_PKG_REPOSITORY")));
        assert!(!rendered.contains("M/F6 MOD/tracker music"));
        assert!(rendered.contains("↪ internal video"));
        assert!(rendered.contains("F8 pointer"));
        assert!(rendered.contains("F9 recent commits and installation details"));
        #[cfg(feature = "qr")]
        assert!(rendered.contains("Q selected YouTube video QR code"));
        #[cfg(not(feature = "qr"))]
        assert!(!rendered.contains("Q selected YouTube video QR code"));
        assert!(rendered.contains("physical mouse input requires a running GPM daemon"));
        for border in ['┌', '┐', '└', '┘'] {
            assert!(
                rendered.contains(border),
                "popup panels must retain the {border} border glyph"
            );
        }
    }

    #[test]
    fn key_map_separates_seek_controls_from_details_history() {
        let view = ViewModel {
            playback: PlaybackStatus {
                idle: false,
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };
        let alt_left = KeyEvent::new(KeyCode::Left, KeyModifiers::ALT);
        let alt_right = KeyEvent::new(KeyCode::Right, KeyModifiers::ALT);
        assert_eq!(key_action(alt_left, &view), Some(UiAction::GoBack));
        assert_eq!(key_action(alt_right, &view), Some(UiAction::GoForward));
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &view,),
            Some(UiAction::GoBack)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &view),
            Some(UiAction::SeekRelative(-5))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &view),
            Some(UiAction::SeekRelative(5))
        );
        let five = KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE);
        assert_eq!(key_action(five, &view), Some(UiAction::SeekPercent(50.0)));
        let mut live = ViewModel {
            playback: PlaybackStatus {
                idle: false,
                live: true,
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &live),
            None
        );
        assert_eq!(key_action(five, &live), None);
        live.playback.live_seekable_range = Some(crate::playback::BufferedRange {
            start: Duration::from_secs(1_000),
            end: Duration::from_secs(1_120),
        });
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &live),
            Some(UiAction::SeekRelative(-5))
        );
        assert_eq!(key_action(five, &live), Some(UiAction::SeekPercent(50.0)));
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT),
                &view
            ),
            Some(UiAction::ToggleAutoplay)
        );
    }

    #[test]
    fn live_playback_does_not_map_keyboard_navigation_to_seeking() {
        let view = ViewModel {
            playback: PlaybackStatus {
                live: true,
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };

        for code in [
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Char('0'),
            KeyCode::Char('5'),
            KeyCode::Char('9'),
            KeyCode::Char('['),
            KeyCode::Char(']'),
        ] {
            assert_eq!(
                key_action(KeyEvent::new(code, KeyModifiers::NONE), &view),
                None,
                "{code:?} must not seek an endless live stream"
            );
        }
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT), &view),
            Some(UiAction::GoBack),
            "live playback must retain non-seek navigation"
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &view),
            Some(UiAction::ChangeVolume(5))
        );
    }

    #[test]
    fn focused_details_use_page_keys_without_stealing_volume_or_row_keys() {
        let focused = ViewModel {
            details_focused: true,
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), &focused),
            Some(UiAction::ScrollDetails(DetailsScroll::Pages(-1)))
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                &focused
            ),
            Some(UiAction::ScrollDetails(DetailsScroll::Pages(1)))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &focused),
            Some(UiAction::ChangeVolume(5))
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
                &focused
            ),
            Some(UiAction::MoveSelection(1))
        );

        let unfocused = ViewModel::default();
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                &unfocused
            ),
            None
        );
    }

    #[test]
    fn alt_arrow_keys_scroll_visible_details_without_requiring_focus() {
        let details = ViewModel {
            details: Some(DetailView::default()),
            right_panel_mode: RightPanelMode::Details,
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT), &details),
            Some(UiAction::ScrollDetails(DetailsScroll::Lines(-1)))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT), &details),
            Some(UiAction::ScrollDetails(DetailsScroll::Lines(1)))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &details),
            Some(UiAction::ChangeVolume(5)),
            "plain arrows must remain volume controls"
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &details),
            Some(UiAction::ChangeVolume(-5)),
            "plain arrows must remain volume controls"
        );

        let no_details = ViewModel::default();
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT), &no_details),
            None,
            "Alt+Down must not change volume when Details is unavailable"
        );

        let channel = ViewModel {
            details: Some(DetailView::default()),
            right_panel_mode: RightPanelMode::Channel,
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT), &channel),
            None,
            "the shortcut must not change volume while the description is hidden"
        );
    }

    #[test]
    fn alt_letter_tty_fallback_scrolls_details_without_falling_through() {
        let details = ViewModel {
            details: Some(DetailView::default()),
            right_panel_mode: RightPanelMode::Details,
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('u'), KeyModifiers::ALT),
                &details
            ),
            Some(UiAction::ScrollDetails(DetailsScroll::Lines(-1)))
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT),
                &details
            ),
            Some(UiAction::ScrollDetails(DetailsScroll::Lines(1)))
        );

        for unavailable in [
            ViewModel::default(),
            ViewModel {
                details: Some(DetailView::default()),
                right_panel_mode: RightPanelMode::Channel,
                ..ViewModel::default()
            },
        ] {
            assert_eq!(
                key_action(
                    KeyEvent::new(KeyCode::Char('u'), KeyModifiers::ALT),
                    &unavailable
                ),
                None
            );
            assert_eq!(
                key_action(
                    KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT),
                    &unavailable
                ),
                None,
                "the Linux-TTY scroll fallback must not become Download"
            );
        }
    }

    #[test]
    fn local_and_radio_page_keys_use_visible_capacity_and_keep_details_precedence() {
        let local = ViewModel {
            screen: Screen::Local,
            ..ViewModel::default()
        };
        assert_eq!(
            key_action_with_page_rows(
                KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
                &local,
                Some(7),
                None,
            ),
            Some(UiAction::MoveSelection(-7))
        );
        assert_eq!(
            key_action_with_page_rows(
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                &local,
                Some(7),
                None,
            ),
            Some(UiAction::MoveSelection(7))
        );
        assert_eq!(
            key_action_with_page_rows(
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                &local,
                None,
                None,
            ),
            None,
            "an invisible Local list must not guess a page size"
        );

        let focused_local = ViewModel {
            details_focused: true,
            ..local
        };
        assert_eq!(
            key_action_with_page_rows(
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                &focused_local,
                Some(7),
                None,
            ),
            Some(UiAction::ScrollDetails(DetailsScroll::Pages(1))),
            "focused Details must retain PageDown before Local list paging"
        );

        let radio = ViewModel {
            screen: Screen::Radio,
            ..ViewModel::default()
        };
        assert_eq!(
            key_action_with_page_rows(
                KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
                &radio,
                Some(11),
                None,
            ),
            Some(UiAction::MoveSelection(-11))
        );
        assert_eq!(
            key_action_with_page_rows(
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                &radio,
                Some(11),
                None,
            ),
            Some(UiAction::MoveSelection(11))
        );

        let youtube = ViewModel::default();
        assert_eq!(
            key_action_with_page_rows(
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                &youtube,
                Some(11),
                None,
            ),
            None,
            "the focused fix must not assign new paging semantics to unrelated screens"
        );
    }

    #[test]
    fn visible_local_page_capacity_tracks_compact_rows_and_tiny_terminals() {
        let mut hit_map = HitMap {
            rows: Rect::new(0, 0, 40, 9),
            rows_row_height: 1,
            ..HitMap::default()
        };
        assert_eq!(visible_main_list_page_rows(&hit_map), Some(9));

        hit_map.rows_row_height = 2;
        assert_eq!(visible_main_list_page_rows(&hit_map), Some(4));

        hit_map.rows.height = 1;
        assert_eq!(visible_main_list_page_rows(&hit_map), Some(1));

        hit_map.rows.height = 0;
        assert_eq!(visible_main_list_page_rows(&hit_map), None);
    }

    #[test]
    fn local_escape_opens_parent_after_higher_priority_escape_modes() {
        let local = ViewModel {
            screen: Screen::Local,
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &local),
            Some(UiAction::OpenLocalParent)
        );

        let focused = ViewModel {
            details_focused: true,
            ..local.clone()
        };
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &focused),
            Some(UiAction::SetDetailsFocus(false))
        );

        let editing = ViewModel {
            search_editing: true,
            ..local
        };
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &editing),
            Some(UiAction::CancelSearch)
        );
    }

    #[test]
    fn text_selection_mode_reserves_keys_for_copying_and_returning_to_controls() {
        let available = ViewModel {
            details: Some(DetailView::default()),
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
                &available
            ),
            Some(UiAction::ToggleTextSelectionMode)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
                &ViewModel::default()
            ),
            None,
            "text selection cannot start without Details content"
        );

        let active = ViewModel {
            text_selection_mode: true,
            ..available
        };
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &active),
            Some(UiAction::ToggleTextSelectionMode)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT),
                &active
            ),
            Some(UiAction::ToggleChapterTimestamps)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &active
            ),
            Some(UiAction::Quit)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ),
                &active,
            ),
            None,
            "a forwarded terminal Copy chord must never quit Youta"
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
                &active
            ),
            None,
            "copy mode must not dispatch unrelated Youta controls"
        );
    }

    #[test]
    fn diagnostic_popup_captures_keyboard_and_maps_all_report_controls() {
        let view = ViewModel {
            search_editing: true,
            help_open: true,
            error_popup: Some(ErrorPopupView {
                title: "Playback failed".to_owned(),
                report: "complete report".to_owned(),
                gh_available: true,
                ..ErrorPopupView::default()
            }),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &view),
            Some(UiAction::DismissErrorPopup)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &view
            ),
            Some(UiAction::CopyErrorReport)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &view),
            Some(UiAction::RequestGitHubIssueSubmission)
        );
        assert_eq!(
            key_action(
                KeyEvent::new_with_kind(
                    KeyCode::Char('g'),
                    KeyModifiers::NONE,
                    KeyEventKind::Release,
                ),
                &view,
            ),
            None,
            "a key release must not request publication"
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &view),
            Some(UiAction::CopyAndOpenGitHubIssue)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &view),
            Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Lines(1)))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), &view),
            Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Pages(-1)))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &view),
            Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Home))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &view),
            Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::End))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &view),
            None,
            "the popup must not leak the normal quit action"
        );
    }

    #[test]
    fn github_issue_submission_confirmation_owns_enter_and_escape() {
        let confirming = ViewModel {
            error_popup: Some(ErrorPopupView {
                title: "Playback failed".to_owned(),
                report: "complete report".to_owned(),
                gh_available: true,
                github_issue_submission: GitHubIssueSubmissionView::Confirming,
                ..ErrorPopupView::default()
            }),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &confirming
            ),
            Some(UiAction::ConfirmGitHubIssueSubmission)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
                &confirming
            ),
            None,
            "the request hotkey must not also confirm publication"
        );
        assert_eq!(
            key_action(
                KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release,),
                &confirming,
            ),
            None,
            "an Enter release must not confirm publication"
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &confirming),
            Some(UiAction::CancelGitHubIssueSubmission)
        );

        let submitted = ViewModel {
            external_opener_available: true,
            error_popup: Some(ErrorPopupView {
                github_issue_submission: GitHubIssueSubmissionView::Submitted {
                    url: "https://github.com/vitaly-zdanevich/youta/issues/123".to_owned(),
                },
                ..ErrorPopupView::default()
            }),
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
                &submitted
            ),
            Some(UiAction::OpenGitHubIssueSubmissionTarget)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
                &submitted
            ),
            None,
            "a submitted diagnostic must not create a duplicate issue"
        );

        let failed = ViewModel {
            error_popup: Some(ErrorPopupView {
                gh_available: true,
                github_issue_submission: GitHubIssueSubmissionView::Failed {
                    message: "gh rejected the request".to_owned(),
                },
                ..ErrorPopupView::default()
            }),
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
                &failed
            ),
            Some(UiAction::RequestGitHubIssueSubmission)
        );

        let submitting = ViewModel {
            error_popup: Some(ErrorPopupView {
                github_issue_submission: GitHubIssueSubmissionView::Submitting,
                ..ErrorPopupView::default()
            }),
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &submitting),
            None,
            "closing must not imply that an in-flight remote submission was cancelled"
        );
    }

    #[test]
    fn diagnostic_popup_uses_browser_fallback_when_github_cli_is_unavailable() {
        let view = ViewModel {
            error_popup: Some(ErrorPopupView {
                title: "Error".to_owned(),
                report: "report".to_owned(),
                gh_available: false,
                ..ErrorPopupView::default()
            }),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &view),
            Some(UiAction::CopyAndOpenGitHubIssue)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &view),
            None
        );
    }

    #[test]
    fn yt_dlp_forbidden_popup_reserves_its_link_shortcuts() {
        let view = ViewModel {
            external_opener_available: true,
            error_popup: Some(yt_dlp_forbidden_error(true)),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE), &view),
            Some(UiAction::OpenYtDlpProject)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), &view),
            Some(UiAction::OpenGentooYtDlpPackage)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &view),
            None,
            "generic issue actions must stay out of the specialized popup"
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &view),
            None
        );

        let without_gentoo = ViewModel {
            error_popup: Some(yt_dlp_forbidden_error(false)),
            ..view.clone()
        };
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
                &without_gentoo,
            ),
            None
        );

        let generic = ViewModel {
            error_popup: Some(ErrorPopupView::default()),
            ..view
        };
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
                &generic
            ),
            None
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
                &generic
            ),
            None
        );
    }

    #[test]
    fn rename_popup_maps_arrows_to_cursor_movement_without_seeking() {
        let view = ViewModel {
            local_file_popup: Some(LocalFilePopupView::Rename {
                value: "трек.flac".to_owned(),
                cursor_byte: "трек".len(),
                error: None,
            }),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &view),
            Some(UiAction::MoveLocalRenameCursor(-1))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &view),
            Some(UiAction::MoveLocalRenameCursor(1))
        );
    }

    #[test]
    fn rename_popup_keeps_ascii_filename_contiguous_at_each_cursor_offset() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            local_file_popup: Some(LocalFilePopupView::Rename {
                value: "5.jpg".to_owned(),
                cursor_byte: 0,
                error: None,
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        let popup_area = centered_rect(66, 28, Rect::new(0, 0, 100, 30));
        let field_area = rename_field_area(popup_area).expect("rename field");

        for cursor_byte in 0..="5.jpg".len() {
            let Some(LocalFilePopupView::Rename {
                cursor_byte: current,
                ..
            }) = view.local_file_popup.as_mut()
            else {
                panic!("rename popup");
            };
            *current = cursor_byte;
            terminal
                .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
                .expect("draw rename popup");

            let rendered = rendered_text(&terminal);
            assert!(rendered.contains("5.jpg"));
            assert!(!rendered.contains('▏'));
            assert!(terminal.backend().cursor_visible());
            let cursor = terminal.backend().cursor_position();
            assert_eq!(cursor.x, field_area.x.saturating_add(cursor_byte as u16));
            assert_eq!(cursor.y, field_area.y);
        }
    }

    #[test]
    fn rename_popup_cursor_uses_wide_grapheme_display_cells_and_scrolls_whole_graphemes() {
        let value = "界👩‍💻.jpg";
        let after_wide_graphemes = "界👩‍💻".len();
        let viewport = rename_field_viewport(value, value.len(), 5);
        assert_eq!(&value[viewport.start_byte..], ".jpg");
        assert_eq!(viewport.scroll_width, 4);
        assert_eq!(viewport.cursor_column, 4);

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            local_file_popup: Some(LocalFilePopupView::Rename {
                value: value.to_owned(),
                cursor_byte: after_wide_graphemes,
                error: None,
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw rename popup");

        let popup_area = centered_rect(66, 28, Rect::new(0, 0, 100, 30));
        let field_area = rename_field_area(popup_area).expect("rename field");
        let cursor = terminal.backend().cursor_position();
        assert_eq!(cursor.x, field_area.x.saturating_add(4));
        assert_eq!(cursor.y, field_area.y);
    }

    #[test]
    fn rename_cursor_hides_on_the_next_non_rename_frame() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            local_file_popup: Some(LocalFilePopupView::Rename {
                value: "5.jpg".to_owned(),
                cursor_byte: 1,
                error: None,
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw rename popup");
        assert!(terminal.backend().cursor_visible());

        view.local_file_popup = None;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw non-rename frame");
        assert!(!terminal.backend().cursor_visible());
    }

    #[test]
    fn virtual_pointer_suppresses_the_native_rename_cursor() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            local_file_popup: Some(LocalFilePopupView::Rename {
                value: "5.jpg".to_owned(),
                cursor_byte: 1,
                error: None,
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        let mut virtual_cursor = VirtualCursor {
            active: true,
            ..VirtualCursor::default()
        };

        terminal
            .draw(|frame| {
                render_frame(frame, &view, &UiSettings::default(), &mut hit_map, None);
                render_local_rename_cursor(frame, &view, !virtual_cursor.active);
                virtual_cursor.render(frame);
            })
            .expect("draw rename popup with virtual pointer");

        assert!(!terminal.backend().cursor_visible());
    }

    #[test]
    fn youtube_setup_popup_captures_keyboard_and_edits_only_through_setup_actions() {
        let mut view = ViewModel {
            search_editing: true,
            help_open: true,
            youtube_setup_popup: Some(YouTubeSetupPopupView {
                selected_field: YouTubeSetupField::ApiKey,
                ..YouTubeSetupPopupView::default()
            }),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE), &view),
            Some(UiAction::AppendYouTubeSetupCharacter('A'))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &view),
            Some(UiAction::DeleteYouTubeSetupCharacter)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &view),
            Some(UiAction::SelectYouTubeSetupField(
                YouTubeSetupField::InvidiousUrl
            ))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &view),
            Some(UiAction::SubmitYouTubeSetup)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), &view),
            Some(UiAction::OpenYouTubeApiKeyGuide)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE), &view),
            Some(UiAction::OpenGoogleCloudCredentials)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE), &view),
            Some(UiAction::OpenInvidiousInstances)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &view),
            Some(UiAction::DismissYouTubeSetup)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &view
            ),
            None,
            "the modal must not leak the normal quit action"
        );

        view.youtube_setup_popup
            .as_mut()
            .expect("setup popup")
            .selected_field = YouTubeSetupField::InvidiousUrl;
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &view),
            Some(UiAction::SelectYouTubeSetupField(YouTubeSetupField::ApiKey))
        );
    }

    #[test]
    fn yandex_music_setup_popup_captures_and_maps_secret_editor_keys() {
        let view = ViewModel {
            search_editing: true,
            help_open: true,
            yandex_music_setup_popup: Some(YandexMusicSetupPopupView::default()),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE), &view),
            Some(UiAction::AppendYandexMusicTokenCharacter('A'))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &view),
            Some(UiAction::DeleteYandexMusicTokenCharacter)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &view),
            Some(UiAction::SubmitYandexMusicSetup)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), &view),
            Some(UiAction::OpenYandexOAuthGuide)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &view),
            Some(UiAction::DismissYandexMusicSetup)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &view
            ),
            None,
            "the modal must not leak the normal quit action"
        );
    }

    #[test]
    fn yandex_music_tab_does_not_reserve_a_token_editor_hotkey() {
        let view = ViewModel {
            screen: Screen::YandexMusic,
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::NONE), &view),
            None
        );
    }

    #[test]
    fn control_w_deletes_words_only_in_the_seven_text_editors() {
        let chord = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        let editors = [
            (
                ViewModel {
                    search_editing: true,
                    ..ViewModel::default()
                },
                UiAction::DeleteSearchWord,
            ),
            (
                ViewModel {
                    youtube_setup_popup: Some(YouTubeSetupPopupView::default()),
                    ..ViewModel::default()
                },
                UiAction::DeleteYouTubeSetupWord,
            ),
            (
                ViewModel {
                    yandex_music_setup_popup: Some(YandexMusicSetupPopupView::default()),
                    ..ViewModel::default()
                },
                UiAction::DeleteYandexMusicTokenWord,
            ),
            (
                ViewModel {
                    rss_subscription_popup: Some(RssSubscriptionPopupView::default()),
                    ..ViewModel::default()
                },
                UiAction::DeleteRssSubscriptionWord,
            ),
            (
                ViewModel {
                    playlist_popup: Some(PlaylistPopupView {
                        mode: PlaylistPopupMode::Create,
                        ..PlaylistPopupView::default()
                    }),
                    ..ViewModel::default()
                },
                UiAction::DeletePlaylistEditorWord,
            ),
            (
                ViewModel {
                    private_note_popup: Some(PrivateNotePopupView::default()),
                    ..ViewModel::default()
                },
                UiAction::DeletePrivateNoteWord,
            ),
            (
                ViewModel {
                    local_file_popup: Some(LocalFilePopupView::Rename {
                        value: "fixture.flac".to_owned(),
                        cursor_byte: "fixture.flac".len(),
                        error: None,
                    }),
                    ..ViewModel::default()
                },
                UiAction::DeleteLocalRenameWord,
            ),
        ];

        for (view, expected) in editors {
            assert_eq!(key_action(chord, &view), Some(expected));
        }
        assert_eq!(
            key_action(chord, &ViewModel::default()),
            None,
            "Ctrl-W outside an editor must not toggle the waveform"
        );

        let chooser = ViewModel {
            playlist_popup: Some(PlaylistPopupView::default()),
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(chord, &chooser),
            None,
            "the playlist chooser is not a text editor"
        );
    }

    #[test]
    fn waveform_shortcut_toggles_outside_editors() {
        let shortcut = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE);
        for right_panel_mode in [RightPanelMode::Details, RightPanelMode::Channel] {
            for waveform_visible in [false, true] {
                let view = ViewModel {
                    right_panel_mode,
                    waveform_visible,
                    ..ViewModel::default()
                };
                assert_eq!(
                    key_action(shortcut, &view),
                    Some(UiAction::ToggleWaveform),
                    "the controller validates whether the selected item is eligible"
                );
            }
        }
    }

    const TEST_WAVEFORM_GENERATION: u64 = 73;

    /// Builds a ready waveform with one finest-level peak per supplied value.
    fn ready_waveform(media_id: MediaId, duration: Duration, amplitudes: &[i16]) -> WaveformView {
        let peaks = amplitudes
            .iter()
            .copied()
            .map(|maximum| Peak {
                minimum: 0,
                maximum,
            })
            .collect::<Vec<_>>();
        WaveformView::Ready {
            media_id,
            generation: TEST_WAVEFORM_GENERATION,
            duration,
            pyramid: Arc::new(PeakPyramid::from_peaks(peaks, 1, amplitudes.len())),
        }
    }

    #[test]
    fn ready_waveform_renders_amplitudes_and_playback_progress() {
        let backend = TestBackend::new(8, 4);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let media_id = MediaId::new(SourceKind::Local, "/music/amplitudes.flac");
        let view = ViewModel {
            waveform: ready_waveform(
                media_id.clone(),
                Duration::from_secs(80),
                &[0, 4_096, 8_192, 12_288, 16_384, 20_480, 24_576, 28_672],
            ),
            playing_media_id: Some(media_id),
            waveform_playback_matches: true,
            playback: PlaybackStatus {
                idle: false,
                position: Duration::from_secs(40),
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        let mut theme = Theme::new(false);
        theme.progress = Style::default().fg(Color::Red);
        theme.muted = Style::default().fg(Color::Blue);

        terminal
            .draw(|frame| {
                render_waveform(frame, frame.area(), &view, &theme, &mut hit_map);
            })
            .expect("draw ready waveform");

        let waveform_area = hit_map
            .waveform_seek
            .as_ref()
            .expect("ready waveform seek target")
            .area;
        assert_eq!(waveform_area.height, WAVEFORM_ROWS);
        let row_symbols = (0..waveform_area.height)
            .map(|row| {
                (waveform_area.x..waveform_area.right())
                    .map(|column| {
                        terminal.backend().buffer()[(column, waveform_area.y.saturating_add(row))]
                            .symbol()
                            .to_owned()
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            row_symbols,
            ["       ▄", "     ▄██", "   ▄████", "▁▄██████"]
        );
        for row in 0..waveform_area.height {
            for offset in 0..waveform_area.width {
                let cell = &terminal.backend().buffer()[(
                    waveform_area.x.saturating_add(offset),
                    waveform_area.y.saturating_add(row),
                )];
                let foreground = cell.fg;
                assert_eq!(
                    foreground,
                    if offset < 4 { Color::Red } else { Color::Blue },
                    "the first half of every row must use the played style"
                );
                assert_eq!(
                    cell.modifier.contains(Modifier::BOLD),
                    offset < 4,
                    "played waveform cells must retain their emphasis"
                );
            }
        }
    }

    #[test]
    fn waveform_progress_colors_are_distinct_in_every_builtin_theme() {
        for theme in [
            Theme::new(false),
            Theme::new(true),
            Theme::for_terminal(false, true),
        ] {
            let backend = TestBackend::new(2, WAVEFORM_ROWS);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let media_id = MediaId::new(SourceKind::Local, "/music/colors.flac");
            let view = ViewModel {
                waveform: ready_waveform(media_id.clone(), Duration::from_secs(2), &[i16::MAX; 2]),
                playing_media_id: Some(media_id),
                waveform_playback_matches: true,
                playback: PlaybackStatus {
                    idle: false,
                    position: Duration::from_secs(1),
                    ..PlaybackStatus::default()
                },
                ..ViewModel::default()
            };
            let mut hit_map = HitMap::default();

            terminal
                .draw(|frame| {
                    render_waveform(frame, frame.area(), &view, &theme, &mut hit_map);
                })
                .expect("draw waveform color fixture");

            for row in 0..WAVEFORM_ROWS {
                let played = &terminal.backend().buffer()[(0, row)];
                let remaining = &terminal.backend().buffer()[(1, row)];
                assert_eq!(played.fg, theme.progress.fg.expect("played foreground"));
                assert_eq!(remaining.fg, theme.muted.fg.expect("remaining foreground"));
                assert_ne!(
                    played.fg, remaining.fg,
                    "played and remaining waveform cells must be visibly distinct"
                );
            }
        }
    }

    #[test]
    fn ready_waveform_click_seeks_its_owner_when_another_item_is_playing() {
        let backend = TestBackend::new(10, WAVEFORM_ROWS);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let waveform_owner = MediaId::new(SourceKind::Local, "/music/selected.flac");
        let playing_owner = MediaId::new(SourceKind::Local, "/music/playing.flac");
        let view = ViewModel {
            waveform: ready_waveform(
                waveform_owner.clone(),
                Duration::from_secs(100),
                &[i16::MAX; 10],
            ),
            playing_media_id: Some(playing_owner),
            playback: PlaybackStatus {
                idle: false,
                position: Duration::from_secs(75),
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_waveform(frame, frame.area(), &view, &Theme::new(false), &mut hit_map);
            })
            .expect("draw owner-aware waveform");

        let target = hit_map
            .waveform_seek
            .as_ref()
            .expect("waveform seek target")
            .clone();
        assert_eq!(target.area.height, WAVEFORM_ROWS);
        for row in target.area.y..target.area.bottom() {
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: target.area.x.saturating_add(target.area.width / 2),
                        row,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                Some(UiAction::ActivateWaveformTimecode {
                    media_id: waveform_owner.clone(),
                    generation: TEST_WAVEFORM_GENERATION,
                    seconds: 55,
                }),
                "every waveform row must seek the selected waveform owner"
            );
        }
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: target.area.right().saturating_sub(1),
                    row: target.area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ActivateWaveformTimecode {
                media_id: waveform_owner,
                generation: TEST_WAVEFORM_GENERATION,
                seconds: 99,
            }),
            "the rightmost waveform cell must map to the final valid second"
        );
    }

    #[test]
    fn waveform_uses_and_seeks_every_available_height_up_to_four_rows() {
        let media_id = MediaId::new(SourceKind::Local, "/music/height.flac");
        for height in 1..=WAVEFORM_ROWS {
            let backend = TestBackend::new(4, height);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let view = ViewModel {
                waveform: ready_waveform(media_id.clone(), Duration::from_secs(40), &[i16::MAX; 4]),
                ..ViewModel::default()
            };
            let mut hit_map = HitMap::default();

            terminal
                .draw(|frame| {
                    render_waveform(frame, frame.area(), &view, &Theme::new(false), &mut hit_map);
                })
                .expect("draw height-constrained waveform");

            let target = hit_map
                .waveform_seek
                .as_ref()
                .expect("height-constrained waveform target");
            assert_eq!(target.area.height, height);
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: target.area.x.saturating_add(2),
                        row: target.area.bottom().saturating_sub(1),
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                Some(UiAction::ActivateWaveformTimecode {
                    media_id: media_id.clone(),
                    generation: TEST_WAVEFORM_GENERATION,
                    seconds: 26,
                }),
                "every rendered waveform row must remain seekable"
            );
        }
    }

    #[test]
    fn waveform_progress_requires_an_exact_playback_identity_match() {
        let backend = TestBackend::new(8, WAVEFORM_ROWS);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let colliding_display_id = MediaId::new(SourceKind::Local, "/music/track-�.flac");
        let view = ViewModel {
            waveform: ready_waveform(
                colliding_display_id.clone(),
                Duration::from_secs(80),
                &[i16::MAX; 8],
            ),
            playing_media_id: Some(colliding_display_id),
            waveform_playback_matches: false,
            playback: PlaybackStatus {
                idle: false,
                position: Duration::from_secs(40),
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        let mut theme = Theme::new(false);
        theme.progress = Style::default().fg(Color::Red);
        theme.muted = Style::default().fg(Color::Blue);

        terminal
            .draw(|frame| {
                render_waveform(frame, frame.area(), &view, &theme, &mut hit_map);
            })
            .expect("draw identity-mismatched waveform");

        let area = hit_map
            .waveform_seek
            .as_ref()
            .expect("identity-mismatched waveform target")
            .area;
        for row in area.y..area.bottom() {
            for column in area.x..area.right() {
                assert_eq!(
                    terminal.backend().buffer()[(column, row)].fg,
                    Color::Blue,
                    "a colliding display ID must not borrow another file's progress"
                );
            }
        }
    }

    #[test]
    fn waveform_fractional_duration_clicks_use_exact_time_on_every_row() {
        for (duration, column_offset, expected_second) in [
            (Duration::from_millis(1_900), 4, 1),
            (Duration::from_millis(1_900), 7, 1),
            (Duration::from_millis(900), 7, 0),
        ] {
            let backend = TestBackend::new(8, WAVEFORM_ROWS);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let media_id = MediaId::new(SourceKind::Local, "/music/short.flac");
            let view = ViewModel {
                waveform: ready_waveform(media_id.clone(), duration, &[i16::MAX; 8]),
                ..ViewModel::default()
            };
            let mut hit_map = HitMap::default();

            terminal
                .draw(|frame| {
                    render_waveform(frame, frame.area(), &view, &Theme::new(false), &mut hit_map);
                })
                .expect("draw fractional-duration waveform");
            let target = hit_map
                .waveform_seek
                .as_ref()
                .expect("fractional waveform seek target");

            assert_eq!(target.area.height, WAVEFORM_ROWS);
            for row in target.area.y..target.area.bottom() {
                assert_eq!(
                    mouse_action(
                        MouseEvent {
                            kind: MouseEventKind::Down(MouseButton::Left),
                            column: target.area.x.saturating_add(column_offset),
                            row,
                            modifiers: KeyModifiers::NONE,
                        },
                        &hit_map,
                        &view,
                    ),
                    Some(UiAction::ActivateWaveformTimecode {
                        media_id: media_id.clone(),
                        generation: TEST_WAVEFORM_GENERATION,
                        seconds: expected_second,
                    }),
                    "fractional-duration clicks must map by exact time on every waveform row"
                );
            }
        }
    }

    #[test]
    fn non_ready_waveforms_clear_stale_seek_targets() {
        let backend = TestBackend::new(48, 4);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let media_id = MediaId::new(SourceKind::Local, "/music/pending.flac");
        let ready = ready_waveform(media_id.clone(), Duration::from_secs(60), &[i16::MAX; 8]);
        let cases = [
            (
                WaveformView::Loading {
                    media_id: media_id.clone(),
                },
                "Generating local waveform",
            ),
            (
                WaveformView::Failed {
                    media_id: media_id.clone(),
                    message: "decode failed".to_owned(),
                },
                "decode failed",
            ),
            (WaveformView::Unavailable, "playable local files"),
        ];
        let mut view = ViewModel {
            waveform: ready.clone(),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        for (state, expected_message) in cases {
            view.waveform = ready.clone();
            terminal
                .draw(|frame| {
                    render_waveform(frame, frame.area(), &view, &Theme::new(false), &mut hit_map);
                })
                .expect("prime ready waveform target");
            let stale_area = hit_map
                .waveform_seek
                .as_ref()
                .expect("primed waveform seek target")
                .area;

            view.waveform = state;
            terminal
                .draw(|frame| {
                    render_waveform(frame, frame.area(), &view, &Theme::new(false), &mut hit_map);
                })
                .expect("draw non-ready waveform state");

            assert!(
                hit_map.waveform_seek.is_none(),
                "a non-ready state must clear the previous owner's hit target"
            );
            assert!(
                rendered_text(&terminal).contains(expected_message),
                "the non-ready state must explain itself"
            );
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: stale_area.x,
                        row: stale_area.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                None,
                "clicking the stale waveform rectangle must do nothing"
            );
        }
    }

    #[test]
    fn waveform_replaces_seek_bar_without_hiding_details_or_leaving_stale_targets() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let media_id = MediaId::new(SourceKind::Local, "/music/closed.flac");
        let mut view = ViewModel {
            screen: Screen::Local,
            right_panel_mode: RightPanelMode::Details,
            waveform_visible: true,
            waveform: ready_waveform(media_id.clone(), Duration::from_secs(60), &[i16::MAX; 8]),
            details: Some(DetailView {
                media_id: Some(media_id.clone()),
                title: "Waveform fixture".to_owned(),
                description: "Details remain visible while the waveform replaces the seek bar."
                    .to_owned(),
                ..DetailView::default()
            }),
            playing_media_id: Some(media_id.clone()),
            playback: PlaybackStatus {
                idle: false,
                position: Duration::from_secs(30),
                duration: Some(Duration::from_secs(60)),
                ..PlaybackStatus::default()
            },
            playback_chapters: vec![Chapter {
                title: "Hidden chapter label".to_owned(),
                start_seconds: 0,
                end_seconds: Some(60),
            }],
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_frame(frame, &view, &UiSettings::default(), &mut hit_map, None);
            })
            .expect("draw ready waveform in place of the seek bar");
        let target = hit_map
            .waveform_seek
            .as_ref()
            .expect("waveform seek target")
            .clone();
        let rendered = rendered_text(&terminal);
        assert!(
            rendered.contains("Details remain visible"),
            "the Details pane must remain visible while the waveform is enabled: {rendered}"
        );
        assert!(
            !rendered.contains("Waveform — click to seek"),
            "the waveform is a seek track, not a right-panel mode"
        );
        assert!(
            !rendered.contains("Hidden chapter label"),
            "chapter labels must not consume rows while the waveform replaces the seek bar"
        );
        assert_eq!(target.area.height, WAVEFORM_ROWS);
        assert_eq!(hit_map.seek_bar, Rect::default());
        assert!(hit_map.seek_markers.is_empty());
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleTextSelectionMode),
            "waveform visibility must not disable Details interactions"
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT), &view),
            Some(UiAction::ScrollDetails(DetailsScroll::Lines(1))),
            "waveform visibility must not disable Details scrolling"
        );

        view.waveform_visible = false;
        terminal
            .draw(|frame| {
                render_frame(frame, &view, &UiSettings::default(), &mut hit_map, None);
            })
            .expect("restore the normal seek bar");

        assert!(hit_map.waveform_seek.is_none());
        assert_ne!(hit_map.seek_bar, Rect::default());
        assert!(
            matches!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: hit_map
                            .seek_bar
                            .x
                            .saturating_add(hit_map.seek_bar.width / 2),
                        row: hit_map.seek_bar.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                Some(UiAction::SeekPercent(_))
            ),
            "disabling the waveform must restore the normal percentage seek target"
        );
    }

    #[test]
    fn waveform_reduction_survives_narrow_resizes_without_losing_a_transient() {
        let media_id = MediaId::new(SourceKind::Local, "/music/transient.flac");
        let mut amplitudes = vec![0; 257];
        amplitudes[128] = i16::MAX;
        let view = ViewModel {
            waveform: ready_waveform(media_id.clone(), Duration::from_secs(257), &amplitudes),
            ..ViewModel::default()
        };

        for width in [1, 2, 3, 7, 31, 80] {
            let backend = TestBackend::new(width, 3);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let mut hit_map = HitMap::default();

            terminal
                .draw(|frame| {
                    render_waveform(frame, frame.area(), &view, &Theme::new(false), &mut hit_map);
                })
                .expect("draw reduced waveform");

            assert!(
                rendered_text(&terminal).contains('█'),
                "width {width} must retain the short full-scale transient"
            );
            let target = hit_map
                .waveform_seek
                .as_ref()
                .expect("resized waveform seek target");
            assert_eq!(target.area.width, width);
            assert_eq!(target.media_id, media_id);
        }

        let backend = TestBackend::new(1, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap {
            waveform_seek: Some(WaveformSeekTarget {
                area: Rect::new(0, 0, 1, 1),
                media_id,
                generation: TEST_WAVEFORM_GENERATION,
                duration: Duration::from_secs(257),
            }),
            ..HitMap::default()
        };
        terminal
            .draw(|frame| {
                render_waveform(
                    frame,
                    Rect::default(),
                    &view,
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw zero-area waveform after resize");
        assert!(
            hit_map.waveform_seek.is_none(),
            "a zero-area resize must clear the old target without indexing peaks"
        );
    }

    #[test]
    fn private_note_popup_is_modal_multiline_and_redacted_from_debug() {
        let secret = "a private line\nanother private line";
        let mut view = ViewModel {
            private_note_popup: Some(PrivateNotePopupView {
                target_label: "Fixture episode".to_owned(),
                body: secret.to_owned(),
                cursor_byte: secret.len(),
                existing: true,
                storage_path: "/tmp/youta/state/notes.toml".to_owned(),
                ..PrivateNotePopupView::default()
            }),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
                &view
            ),
            Some(UiAction::SavePrivateNote)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &view),
            Some(UiAction::InsertPrivateNoteNewline)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &view),
            Some(UiAction::MovePrivateNoteCursor(PrivateNoteCursorMotion::Up))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE), &view),
            Some(UiAction::RequestPrivateNoteDelete)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &view),
            Some(UiAction::AppendPrivateNoteCharacter('q')),
            "normal Quit must not leak through the focused note editor"
        );
        let debug = format!("{view:?}");
        assert!(!debug.contains(secret));
        assert!(debug.contains("[REDACTED]"));

        view.private_note_popup
            .as_mut()
            .expect("note popup")
            .confirming_delete = true;
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &view),
            Some(UiAction::RequestPrivateNoteDelete)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &view),
            Some(UiAction::DismissPrivateNotePopup)
        );
    }

    #[test]
    fn private_note_popup_renders_storage_and_mouse_buttons() {
        let mut terminal =
            Terminal::new(TestBackend::new(110, 32)).expect("private-note test terminal");
        let view = ViewModel {
            details: Some(DetailView {
                media_id: Some(MediaId::new(SourceKind::YouTube, "fixture")),
                title: "Fixture episode".to_owned(),
                has_private_note: true,
                ..DetailView::default()
            }),
            private_note_popup: Some(PrivateNotePopupView {
                target_label: "Fixture episode".to_owned(),
                body: "Line one\nLine two".to_owned(),
                cursor_byte: 8,
                existing: true,
                storage_path: "/tmp/youta/state/notes.toml".to_owned(),
                ..PrivateNotePopupView::default()
            }),
            private_note_available: true,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("render note popup");

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Private note"));
        assert!(rendered.contains("Line one"));
        assert!(rendered.contains("/tmp/youta/state/notes.toml"));
        assert!(!rendered.contains("Enter inserts a new line"));
        assert!(!rendered.contains("Esc closes without saving"));
        let save_area = hit_map
            .private_note_buttons
            .iter()
            .find_map(|(action, area)| (*action == UiAction::SavePrivateNote).then_some(*area))
            .expect("Save mouse target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: save_area.x,
                    row: save_area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SavePrivateNote)
        );
    }

    #[test]
    fn private_note_wrapping_preserves_unicode_graphemes_and_empty_lines() {
        let body = "界e\u{301}👩‍💻\n\n尾";
        let wrapped = wrap_private_note(body, body.len(), 2);

        assert_eq!(wrapped.lines, vec!["界", "e\u{301}", "👩‍💻", "", "尾", "▏"]);
        assert_eq!(wrapped.cursor_row, wrapped.lines.len() - 1);
        assert!(
            wrapped
                .lines
                .iter()
                .all(|line| terminal_text_width(line) <= 2)
        );
    }

    #[test]
    fn private_note_final_wrapped_unicode_row_reaches_scrollbar_bottom() {
        let wrapped_final_line = format!("{}尾", "界e\u{301}👩‍💻".repeat(24));
        let body = (0..20)
            .map(|index| format!("note line {index:02}"))
            .chain(std::iter::once(wrapped_final_line))
            .collect::<Vec<_>>()
            .join("\n");
        let mut terminal =
            Terminal::new(TestBackend::new(110, 32)).expect("private-note test terminal");
        let view = ViewModel {
            private_note_popup: Some(PrivateNotePopupView {
                target_label: "Wrapped Unicode note".to_owned(),
                cursor_byte: body.len(),
                body,
                follow_cursor: true,
                storage_path: "/tmp/youta/state/notes.toml".to_owned(),
                ..PrivateNotePopupView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("render final wrapped Unicode row");

        assert_eq!(
            hit_map.private_note_scroll_offset, hit_map.private_note_scroll_maximum,
            "following the final cursor row must render the final viewport"
        );
        assert!(
            hit_map.private_note_scroll_maximum > 0,
            "the fixture must overflow the editor viewport"
        );
        let buffer = terminal.backend().buffer();
        let scrollbar_bottom = (
            hit_map.private_note_text_area.right(),
            hit_map.private_note_text_area.bottom().saturating_sub(1),
        );
        assert_eq!(
            buffer[scrollbar_bottom].symbol(),
            "█",
            "the scrollbar thumb must reach the final track cell"
        );
        let rendered = rendered_text(&terminal);
        assert!(
            rendered.contains('尾') && rendered.contains('▏'),
            "the final Unicode content and insertion marker must remain visible"
        );
    }

    #[test]
    fn private_note_popup_follows_overflowing_cursor_and_wheel_scrolls_visible_text() {
        let body = (0..40)
            .map(|index| format!("note line {index:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut terminal =
            Terminal::new(TestBackend::new(110, 32)).expect("private-note test terminal");
        let view = ViewModel {
            private_note_popup: Some(PrivateNotePopupView {
                target_label: "Long fixture note".to_owned(),
                cursor_byte: body.len(),
                body,
                follow_cursor: true,
                storage_path: "/tmp/youta/state/notes.toml".to_owned(),
                ..PrivateNotePopupView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("render overflowing note popup");

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("note line 39"));
        assert!(!rendered.contains("note line 00"));
        assert!(rendered.contains('█'), "overflow should render a scrollbar");
        assert!(hit_map.private_note_scroll_offset > 0);
        assert!(hit_map.private_note_scroll_maximum >= hit_map.private_note_scroll_offset);

        let text_area = hit_map.private_note_text_area;
        let expected = hit_map.private_note_scroll_offset.saturating_sub(3);
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    column: text_area.x,
                    row: text_area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SetPrivateNoteScroll(expected))
        );
    }

    #[test]
    fn private_note_delete_confirmation_keeps_prompt_and_storage_path() {
        let mut terminal =
            Terminal::new(TestBackend::new(110, 32)).expect("private-note test terminal");
        let view = ViewModel {
            private_note_popup: Some(PrivateNotePopupView {
                target_label: "Fixture episode".to_owned(),
                body: "A note".to_owned(),
                cursor_byte: "A note".len(),
                existing: true,
                confirming_delete: true,
                storage_path: "/tmp/youta/state/notes.toml".to_owned(),
                ..PrivateNotePopupView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("render note deletion confirmation");

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Delete this note permanently?"));
        assert!(rendered.contains("/tmp/youta/state/notes.toml"));
    }

    #[test]
    fn details_private_note_button_changes_between_add_and_edit() {
        let mut terminal = Terminal::new(TestBackend::new(110, 32)).expect("Details test terminal");
        let mut view = ViewModel {
            details: Some(DetailView {
                media_id: Some(MediaId::new(SourceKind::YouTube, "fixture")),
                title: "Fixture episode".to_owned(),
                ..DetailView::default()
            }),
            rows: vec![RowView {
                media_id: Some(MediaId::new(SourceKind::YouTube, "fixture")),
                title: "Fixture episode".to_owned(),
                ..RowView::default()
            }],
            private_note_available: true,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("render add note");
        assert!(rendered_text(&terminal).contains("[n] Add private note"));
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .any(|(action, _)| *action == UiAction::EditPrivateNote)
        );

        view.details.as_mut().expect("Details").has_private_note = true;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("render edit note");
        assert!(rendered_text(&terminal).contains("[n] Edit private note"));
    }

    #[test]
    fn keyboard_moves_selects_and_activates_detail_links_without_replacing_list_controls() {
        let view = ViewModel {
            details: Some(DetailView {
                links: vec![
                    DetailLinkView {
                        label: "First".to_owned(),
                        url: "https://example.com/first".to_owned(),
                        ..DetailLinkView::default()
                    },
                    DetailLinkView {
                        label: "Second".to_owned(),
                        url: "https://example.com/second".to_owned(),
                        wikidata_item_id: Some("Q42".to_owned()),
                        ..DetailLinkView::default()
                    },
                ],
                ..DetailView::default()
            }),
            selected_detail_link: Some(1),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT), &view,),
            Some(UiAction::MoveDetailLink(1))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT), &view,),
            Some(UiAction::MoveDetailLink(-1))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Home, KeyModifiers::ALT), &view),
            Some(UiAction::SelectDetailLink(0))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::End, KeyModifiers::ALT), &view),
            Some(UiAction::SelectDetailLink(1))
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('W'), KeyModifiers::SHIFT),
                &view
            ),
            Some(UiAction::ToggleWikidataStatements(1))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), &view),
            Some(UiAction::ActivateDetailLink(1))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &view,),
            Some(UiAction::MoveSelection(1))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &view),
            Some(UiAction::ActivateSelection)
        );
    }

    #[test]
    fn wikidata_shortcut_targets_a_disclosure_before_mouse_selection() {
        let mut view = ViewModel {
            details: Some(DetailView {
                links: vec![
                    DetailLinkView {
                        label: "Website".to_owned(),
                        url: "https://example.com".to_owned(),
                        ..DetailLinkView::default()
                    },
                    DetailLinkView {
                        label: "First entity".to_owned(),
                        url: "https://www.wikidata.org/wiki/Q1".to_owned(),
                        wikidata_item_id: Some("Q1".to_owned()),
                        ..DetailLinkView::default()
                    },
                    DetailLinkView {
                        label: "Second entity".to_owned(),
                        url: "https://www.wikidata.org/wiki/Q2".to_owned(),
                        wikidata_item_id: Some("Q2".to_owned()),
                        ..DetailLinkView::default()
                    },
                ],
                ..DetailView::default()
            }),
            selected_detail_link: Some(0),
            ..ViewModel::default()
        };
        let shortcut = KeyEvent::new(KeyCode::Char('W'), KeyModifiers::SHIFT);

        assert_eq!(
            key_action(shortcut, &view),
            Some(UiAction::ToggleWikidataStatements(1)),
            "a non-Wikidata selection must not disable the advertised shortcut"
        );
        view.selected_detail_link = None;
        assert_eq!(
            key_action(shortcut, &view),
            Some(UiAction::ToggleWikidataStatements(1)),
            "the collapsed disclosure must work before any external link is selected"
        );
        view.selected_detail_link = Some(2);
        assert_eq!(
            key_action(shortcut, &view),
            Some(UiAction::ToggleWikidataStatements(2)),
            "an explicitly selected Wikidata row must take precedence"
        );
        view.details
            .as_mut()
            .expect("fixture details")
            .expanded_wikidata_item = Some("Q1".to_owned());
        assert_eq!(
            key_action(shortcut, &view),
            Some(UiAction::ToggleWikidataStatements(1)),
            "the visible spoiler must remain collapsible after selection moves"
        );
    }

    #[test]
    fn tab_shortcuts_cycle_every_enabled_screen_and_wrap() {
        assert_eq!(Screen::TrackerMusic.next(), Screen::Subscriptions);
        assert_eq!(Screen::Subscriptions.next(), Screen::Local);
        assert_eq!(Screen::Local.previous(), Screen::Subscriptions);

        let enabled = Screen::ALL
            .into_iter()
            .filter(|screen| screen.enabled())
            .collect::<Vec<_>>();
        for (index, screen) in enabled.iter().copied().enumerate() {
            let next = enabled[(index + 1) % enabled.len()];
            let previous = enabled[(index + enabled.len() - 1) % enabled.len()];
            let mut view = ViewModel {
                screen,
                ..ViewModel::default()
            };
            view.subscriptions.route = SubscriptionRoute::Items;
            for modifiers in [KeyModifiers::NONE, KeyModifiers::CONTROL] {
                assert_eq!(
                    key_action(KeyEvent::new(KeyCode::Tab, modifiers), &view),
                    Some(UiAction::ShowScreen(next)),
                    "Tab with {modifiers:?} must advance from {screen:?}"
                );
            }
            for key in [
                KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
                KeyEvent::new(
                    KeyCode::BackTab,
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ),
                KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
            ] {
                assert_eq!(
                    key_action(key, &view),
                    Some(UiAction::ShowScreen(previous)),
                    "{key:?} must move backward from {screen:?}"
                );
            }
        }

        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT),
                &ViewModel::default()
            ),
            Some(UiAction::ShowScreen(Screen::Subscriptions))
        );
    }

    #[test]
    fn linux_virtual_console_backtab_encoding_moves_to_previous_screen() {
        let view = ViewModel {
            screen: Screen::Local,
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Tab, KeyModifiers::ALT), &view),
            Some(UiAction::ShowScreen(Screen::Subscriptions)),
            "the Linux console's Escape+Tab Backtab encoding must move backward"
        );
    }

    #[test]
    fn radio_tab_follows_its_compile_feature() {
        assert_eq!(Screen::Radio.enabled(), cfg!(feature = "radio"));
        assert_eq!(
            Screen::ALL
                .into_iter()
                .filter(|screen| screen.enabled())
                .any(|screen| screen == Screen::Radio),
            cfg!(feature = "radio")
        );
    }

    #[test]
    fn preferences_and_channel_page_have_distinct_global_shortcuts() {
        let view = ViewModel::default();
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), &view),
            Some(UiAction::OpenPreferences)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE), &view),
            Some(UiAction::OpenPreferences)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT),
                &view
            ),
            Some(UiAction::OpenChannelInBrowser)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &view),
            Some(UiAction::OpenInBrowser)
        );
    }

    #[test]
    fn rss_subscription_keys_open_only_from_sources_and_are_captured_by_popup() {
        let mut sources = ViewModel {
            screen: Screen::Subscriptions,
            ..ViewModel::default()
        };
        let add = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(
            key_action(add, &sources),
            Some(UiAction::OpenRssSubscriptionPopup)
        );

        sources.subscriptions.route = SubscriptionRoute::Items;
        assert_eq!(
            key_action(add, &sources),
            Some(UiAction::AddToQueue),
            "the global queue shortcut remains available outside the source route"
        );
        let search = ViewModel::default();
        assert_eq!(key_action(add, &search), Some(UiAction::AddToQueue));

        let popup = ViewModel {
            screen: Screen::Subscriptions,
            rss_subscription_popup: Some(RssSubscriptionPopupView {
                url: "https://podcasts.example/feed".to_owned(),
                config_path: "/config/subscriptions.opml".to_owned(),
                validation_error: None,
            }),
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                &popup
            ),
            Some(UiAction::AppendRssSubscriptionCharacter('x'))
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                &popup
            ),
            Some(UiAction::DeleteRssSubscriptionCharacter)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &popup),
            Some(UiAction::SubmitRssSubscription)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &popup),
            Some(UiAction::DismissRssSubscriptionPopup)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
                &popup
            ),
            None,
            "the focused URL editor must capture unrelated modified keys"
        );
    }

    #[test]
    fn playlist_shortcuts_require_a_playable_item_and_preserve_modifier_meanings() {
        for screen in [
            Screen::Search,
            Screen::YouTubeMusic,
            Screen::Bandcamp,
            Screen::ApplePodcasts,
            Screen::Local,
        ] {
            let view = ViewModel {
                screen,
                playlist_item: Some(PlaylistItemView {
                    media_id: MediaId::new(SourceKind::YouTube, "playable"),
                    title: "Playable item".to_owned(),
                    in_todo: false,
                }),
                ..ViewModel::default()
            };
            assert_eq!(
                key_action(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &view),
                Some(UiAction::ToggleTodoPlaylist),
                "plain l must toggle todo on {screen:?}"
            );
            assert_eq!(
                key_action(
                    KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT),
                    &view
                ),
                Some(UiAction::OpenPlaylistPopup),
                "uppercase P must open the chooser on {screen:?}"
            );
            for modifiers in [
                KeyModifiers::CONTROL,
                KeyModifiers::ALT,
                KeyModifiers::SHIFT,
            ] {
                assert_eq!(
                    key_action(KeyEvent::new(KeyCode::Char('l'), modifiers), &view),
                    None,
                    "modified l must retain terminal/application semantics"
                );
            }
        }

        let unavailable = ViewModel::default();
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
                &unavailable
            ),
            None
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT),
                &unavailable
            ),
            None
        );
    }

    #[test]
    fn playlist_popup_keys_are_modal_and_route_create_edit_fields() {
        let browse = ViewModel {
            playlist_item: Some(PlaylistItemView {
                media_id: MediaId::new(SourceKind::YouTube, "episode"),
                title: "Episode".to_owned(),
                in_todo: false,
            }),
            playlist_popup: Some(PlaylistPopupView {
                item_title: "Episode".to_owned(),
                playlists: vec![PlaylistChoiceView {
                    playlist_id: "todo".to_owned(),
                    name: "todo".to_owned(),
                    contains_item: false,
                }],
                ..PlaylistPopupView::default()
            }),
            ..ViewModel::default()
        };
        for (key, expected) in [
            (
                KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                UiAction::MovePlaylistPopupSelection(-1),
            ),
            (
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                UiAction::MovePlaylistPopupSelection(1),
            ),
            (
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                UiAction::ToggleSelectedPlaylistMembership,
            ),
            (
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
                UiAction::BeginNewPlaylist,
            ),
            (
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                UiAction::DismissPlaylistPopup,
            ),
        ] {
            assert_eq!(key_action(key, &browse), Some(expected));
        }
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
                &browse
            ),
            None,
            "the chooser must suppress the global todo shortcut"
        );

        let mut create = browse.clone();
        create.playlist_popup.as_mut().expect("popup").mode = PlaylistPopupMode::Create;
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
                &create
            ),
            Some(UiAction::AppendPlaylistEditorCharacter('l'))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &create),
            Some(UiAction::SelectPlaylistEditorField(
                PlaylistEditorField::Description
            ))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &create),
            Some(UiAction::SelectPlaylistEditorField(
                PlaylistEditorField::Description
            ))
        );
        create.playlist_popup.as_mut().expect("popup").editor_field =
            PlaylistEditorField::Description;
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &create),
            Some(UiAction::SelectPlaylistEditorField(
                PlaylistEditorField::Name
            ))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &create),
            Some(UiAction::CreatePlaylistAndAdd)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                &create
            ),
            Some(UiAction::DeletePlaylistEditorCharacter)
        );

        let mut edit = create;
        edit.playlist_popup.as_mut().expect("popup").mode = PlaylistPopupMode::Edit;
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &edit),
            Some(UiAction::UpdatePlaylist)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &edit),
            Some(UiAction::DismissPlaylistPopup)
        );
    }

    #[test]
    fn playlist_popup_sits_above_local_mutations_but_below_diagnostics() {
        let mut view = ViewModel {
            playlist_popup: Some(PlaylistPopupView::default()),
            local_file_popup: Some(LocalFilePopupView::Trash {
                name: "fixture.flac".to_owned(),
                path: "/music/fixture.flac".to_owned(),
                error: None,
            }),
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &view),
            Some(UiAction::DismissPlaylistPopup),
            "the visibly topmost playlist popup must receive modal input"
        );

        view.error_popup = Some(ErrorPopupView {
            report: "diagnostic fixture".to_owned(),
            ..ErrorPopupView::default()
        });
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &view),
            Some(UiAction::DismissErrorPopup),
            "diagnostics remain the highest-priority modal"
        );
    }

    /// `e` belongs to the Playlists screen alone.
    ///
    /// It used to be shared with an equalizer shortcut that only ever answered
    /// "the equalizer is disabled in direct audiophile mode" — a permanent
    /// refusal, because `docs/AUDIOPHILE.md` lists equalization among the DSP
    /// Youta deliberately does not apply. The key is now unbound everywhere
    /// else, and this test is what says so.
    #[test]
    fn the_edit_shortcut_exists_only_on_an_editable_playlists_row() {
        let playlists = ViewModel {
            screen: Screen::Playlists,
            playlist_edit_available: true,
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
                &playlists
            ),
            Some(UiAction::EditSelectedPlaylist)
        );
        let playlist_entries = ViewModel {
            screen: Screen::Playlists,
            playlist_edit_available: false,
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
                &playlist_entries
            ),
            None,
            "playlist entries without an editable row leave the key unbound"
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
                &ViewModel::default()
            ),
            None
        );
    }

    /// The queue draws its entries, marks what is playing, and offers the
    /// controls whose keys the shared map accepts.
    ///
    /// The Remove control is deliberately absent for the playing entry: the
    /// controller refuses that removal, and offering a control that can only
    /// answer "no" is what the equalizer key used to do.
    #[test]
    fn the_queue_popup_marks_the_playing_entry_and_offers_only_valid_controls() {
        let queue = crate::view::QueuePopupView {
            items: vec![
                crate::view::QueueRowView {
                    media_id: MediaId::new(SourceKind::Local, "/music/a.flac"),
                    title: "First queued track".to_owned(),
                    subtitle: "An Artist".to_owned(),
                    length: "3:03".to_owned(),
                },
                crate::view::QueueRowView {
                    media_id: MediaId::new(SourceKind::Local, "/music/b.flac"),
                    title: "Second queued track".to_owned(),
                    subtitle: String::new(),
                    length: "4:10".to_owned(),
                },
            ],
            current: Some(0),
            selected: 0,
            repeat_one: false,
        };
        let view = ViewModel {
            queue_popup: Some(queue),
            ..ViewModel::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).expect("queue terminal");
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw the queue");
        let rendered = rendered_text(&terminal);

        assert!(rendered.contains("Playback queue"));
        assert!(rendered.contains("Playing 1 of 2"));
        assert!(rendered.contains("▶ First queued track · An Artist · 3:03"));
        assert!(rendered.contains("Second queued track · 4:10"));
        assert!(rendered.contains("[C] Clear"));
        assert!(
            !rendered.contains("[x] Remove"),
            "the playing entry cannot be removed, so no control offers it"
        );
        assert!(
            hit_map
                .queue_popup_buttons
                .iter()
                .any(|(action, _)| *action == UiAction::ClearQueue),
            "the controls must be clickable, not only readable"
        );
        assert!(!hit_map.queue_popup_rows.is_empty());
    }

    /// `u` opens the queue, and must not shadow the Alt-chord that scrolls
    /// Details — the two differ only by a modifier.
    #[test]
    fn the_queue_shortcut_is_unmodified_and_leaves_the_details_chord_alone() {
        let view = ViewModel {
            details: Some(crate::view::DetailView::default()),
            details_focused: true,
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE), &view),
            Some(UiAction::OpenQueuePopup)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::ALT), &view),
            Some(UiAction::ScrollDetails(crate::view::DetailsScroll::Lines(
                -1
            )))
        );
    }

    #[test]
    fn tracker_music_has_no_dedicated_keyboard_shortcut() {
        let view = ViewModel::default();
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT),
                &view
            ),
            None
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE), &view),
            None
        );
    }

    #[test]
    fn note_and_play_next_shortcuts_do_not_replace_youtube_search_order() {
        let view = ViewModel::default();
        let newest = KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT);

        assert_eq!(
            key_action(newest, &view),
            Some(UiAction::ToggleYouTubeSearchSort)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &view),
            Some(UiAction::EditPrivateNote)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
                &view
            ),
            Some(UiAction::PlayNext)
        );
    }

    #[test]
    fn uppercase_c_toggles_creative_commons_filter_without_replacing_channel_info() {
        let view = ViewModel::default();
        let creative_commons = KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT);

        assert_eq!(
            key_action(creative_commons, &view),
            Some(UiAction::ToggleYouTubeCreativeCommons)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &view),
            Some(UiAction::ShowChannel),
            "lowercase channel-info must retain its existing action"
        );
    }

    #[test]
    fn uppercase_t_toggles_chapter_timestamps_without_replacing_text_selection() {
        let view = ViewModel {
            details: Some(DetailView::default()),
            text_selection_mode: true,
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT),
                &view
            ),
            Some(UiAction::ToggleChapterTimestamps)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleTextSelectionMode)
        );
    }

    #[test]
    fn render_contains_player_and_hotkey_controls() {
        let backend = TestBackend::new(240, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            rows: vec![RowView {
                title: "Mock video".to_owned(),
                source: "YouTube / Invidious".to_owned(),
                subtitle: "Mock channel".to_owned(),
                ..RowView::default()
            }],
            details: Some(DetailView {
                title: "Mock video".to_owned(),
                description: "Description with #example".to_owned(),
                license: "Creative Commons".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        view.playback.duration = Some(Duration::from_secs(120));
        view.playback.position = Duration::from_secs(30);
        let settings = UiSettings::default();
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &settings, &mut hit_map))
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("Mock video"));
        assert!(rendered.contains("Creative Commons"));
        assert!(rendered.contains("License:"));
        assert!(!rendered.contains("[Space] Pause"));
        assert!(!rendered.contains("[M] MOD/tracker music"));
        assert!(!rendered.contains("[Tab]"));
        assert!(!rendered.contains("[C]"));
        assert!(!rendered.contains("[N]"));
        assert!(!rendered.contains("[T]"));
        assert!(!rendered.contains("[d]"));
        assert!(!rendered.contains("[w]"));
        assert!(rendered.contains("[/] Search"));
        assert!(rendered.contains("[A] Autoplay: off"));
        assert!(rendered.contains("[k] Move up"));
        assert!(rendered.contains("[j] Move down"));
        assert!(rendered.contains("[↑] Volume up"));
        assert!(rendered.contains("[↓] Volume down"));
        assert!(rendered.contains("[p] Preferences"));
        assert!(rendered.contains("[?] Help"));
        assert!(!rendered.contains("[Enter] Start"));
        assert!(rendered.contains("0:30 / 2:00"));
        assert_minimal_footer_actions(&hit_map);
        assert!(hit_map.buttons.iter().all(|(action, _)| {
            !matches!(action, UiAction::TogglePause | UiAction::ActivateSelection)
        }));
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &view),
            Some(UiAction::ActivateSelection)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &view),
            Some(UiAction::TogglePause)
        );
    }

    fn subscription_row(title: &str, video: bool) -> RowView {
        RowView {
            media_id: video.then(|| MediaId::new(SourceKind::YouTube, "dQw4w9WgXcQ")),
            title: title.to_owned(),
            subtitle: if video {
                "2026 July 25 (yesterday) · 3:32".to_owned()
            } else {
                "https://www.youtube.com/feeds/videos.xml?channel_id=UCfixture".to_owned()
            },
            source: if video {
                "YouTube".to_owned()
            } else {
                "YouTube channel".to_owned()
            },
            subscribed: !video,
            ..RowView::default()
        }
    }

    #[test]
    fn compact_local_rows_drop_redundant_source_labels_and_folder_padding() {
        let backend = TestBackend::new(140, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            screen: Screen::Local,
            local_path: "/music".to_owned(),
            rows: vec![
                RowView {
                    title: "A long album folder".to_owned(),
                    subtitle: "/music/A long album folder".to_owned(),
                    source: "Local folder".to_owned(),
                    compact: true,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::Local, "/music/song.flac")),
                    title: "song.flac".to_owned(),
                    subtitle: "42.00 MiB".to_owned(),
                    source: "Local audio".to_owned(),
                    compact: true,
                    ..RowView::default()
                },
            ],
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw compact Local rows");
        let rendered = rendered_text(&terminal);
        let buffer = terminal.backend().buffer();

        assert!(!rendered.contains("Local folder"));
        assert!(!rendered.contains("Local audio"));
        assert_eq!(
            buffer[(hit_map.rows.x, hit_map.rows.y)].symbol(),
            "A",
            "a non-media folder title must start at the first list cell"
        );
        assert_eq!(
            buffer[(hit_map.rows.x, hit_map.rows.y.saturating_add(1))].symbol(),
            "●",
            "the next Local item must occupy the immediately following row"
        );
        assert!(rendered.contains("A long album folder · /music/A long album folder"));
        assert!(rendered.contains("● song.flac · 42.00 MiB"));
        assert_eq!(hit_map.rows_row_height, 1);
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: hit_map.rows.x,
                    row: hit_map.rows.y.saturating_add(1),
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SelectRow(1))
        );
    }

    #[test]
    fn local_play_marker_and_bold_row_follow_playback_independently_of_selection() {
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let playing = MediaId::new(SourceKind::Local, "/music/playing.webm");
        let selected = MediaId::new(SourceKind::Local, "/music/selected.flac");
        let view = ViewModel {
            screen: Screen::Local,
            local_path: "/music".to_owned(),
            rows: vec![
                RowView {
                    media_id: Some(playing.clone()),
                    title: "playing.webm".to_owned(),
                    subtitle: "8.00 MiB".to_owned(),
                    source: "Local video (audio playback)".to_owned(),
                    compact: true,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(selected),
                    title: "selected.flac".to_owned(),
                    subtitle: "12.00 MiB".to_owned(),
                    source: "Local audio".to_owned(),
                    compact: true,
                    ..RowView::default()
                },
            ],
            selected: 1,
            playing_media_id: Some(playing),
            playback: PlaybackStatus {
                idle: false,
                paused: true,
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw independent Local playing and selected rows");

        let buffer = terminal.backend().buffer();
        let playing_row = hit_map.rows.y;
        let selected_row = playing_row.saturating_add(1);
        assert_eq!(buffer[(hit_map.rows.x, playing_row)].symbol(), "⏸");
        let playing_title = &buffer[(hit_map.rows.x.saturating_add(4), playing_row)];
        assert_eq!(playing_title.symbol(), "p");
        assert!(playing_title.modifier.contains(Modifier::BOLD));
        assert_eq!(playing_title.fg, Color::Cyan);
        assert_ne!(
            buffer[(hit_map.rows.x, selected_row)].symbol(),
            "⏸",
            "selection must not move the playback marker"
        );
        assert_eq!(
            [playing_row, selected_row]
                .into_iter()
                .filter(|row| buffer[(hit_map.rows.x, *row)].symbol() == "⏸")
                .count(),
            1
        );
    }

    #[test]
    fn subscriptions_render_both_drill_down_and_split_navigation_models() {
        let backend = TestBackend::new(160, 34);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let details = DetailView {
            media_id: Some(MediaId::new(SourceKind::YouTube, "dQw4w9WgXcQ")),
            title: "Fixture channel".to_owned(),
            description: "Expanded fixture description".to_owned(),
            channel_id: "UCfixture".to_owned(),
            channel_webpage_url: Some(
                url::Url::parse("https://www.youtube.com/@fixture").expect("fixture channel URL"),
            ),
            ..DetailView::default()
        };
        let mut view = ViewModel {
            screen: Screen::Subscriptions,
            details: Some(details),
            ..ViewModel::default()
        };
        // Leading spaces represent one nested OPML folder level in source rows.
        view.subscriptions.sources = vec![subscription_row("  Fixture channel", false)];
        view.subscriptions.items = vec![subscription_row("Fixture video", true)];
        view.subscriptions.source_title = "Fixture channel".to_owned();
        view.subscriptions.source_subscriber_count = Some(13_045);
        view.playing_media_id = view.subscriptions.items[0].media_id.clone();
        view.playback = PlaybackStatus {
            idle: false,
            paused: false,
            ..PlaybackStatus::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw drill-down sources");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Sources"));
        assert!(!rendered.contains("Subscription sources"));
        assert!(rendered.contains("Fixture channel"));
        assert_eq!(
            rendered.matches("Fixture channel").count(),
            1,
            "the channel title already visible in the source row must not repeat"
        );
        assert!(rendered.contains(&format!("[O] {}", system_url_opener_name())));
        assert!(rendered.contains("[a] Add RSS feed"));
        assert!(!rendered.contains("Refresh videos"));
        assert!(hit_map.subscription_source_rows.width > 0);
        assert_eq!(hit_map.subscription_item_rows, Rect::default());
        let add_feed_target = hit_map
            .subscription_source_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::OpenRssSubscriptionPopup)
            .map(|(_, target)| *target)
            .expect("drill-down RSS-feed button target");
        assert!(
            !contains(
                hit_map.subscription_source_rows,
                add_feed_target.x,
                add_feed_target.y
            ),
            "the RSS action must not become a selectable source row"
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: add_feed_target.x,
                    row: add_feed_target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::OpenRssSubscriptionPopup)
        );

        view.subscriptions.route = SubscriptionRoute::Items;
        view.subscriptions.focus = SubscriptionPane::Items;
        view.right_panel_mode = RightPanelMode::Details;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw drill-down items");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Fixture channel · YouTube · 13,045 subscribers"));
        assert!(rendered.contains("Fixture video"));
        assert!(
            !rendered.contains("YouTube · 2026 July 25"),
            "the subscription heading already identifies the video source"
        );
        assert!(
            !rendered.contains('◆'),
            "subscription videos must omit the redundant channel marker"
        );
        assert!(
            rendered.contains("▶ Fixture video"),
            "playing subscription videos keep only the active-playback marker"
        );
        assert!(rendered.contains("Expanded fixture description"));
        assert!(rendered.contains("[R] Refresh videos"));
        assert_minimal_footer_actions(&hit_map);
        assert!(
            hit_map
                .buttons
                .iter()
                .all(|(action, _)| action != &UiAction::RefreshSubscriptionVideos),
            "the footer must not duplicate the contextual subscription refresh button"
        );
        assert!(
            !rendered.contains("[i] Details"),
            "drill-down already renders Details beside the video list"
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &view),
            None,
            "drill-down Details does not need a description toggle"
        );
        assert!(hit_map.subscription_item_rows.width > 0);

        view.subscriptions.loading = true;
        view.search_animation_frame = 2;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw animated subscription refresh");
        assert!(rendered_text(&terminal).contains("[R] Refresh videos -"));
        view.subscriptions.loading = false;

        view.subscriptions.layout = SubscriptionsLayout::Split;
        view.subscriptions.route = SubscriptionRoute::Sources;
        view.subscriptions.focus = SubscriptionPane::Items;
        view.subscriptions.description_expanded = false;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw split lists");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Sources"));
        assert!(!rendered.contains("Subscription sources"));
        assert!(rendered.contains("[a] Add RSS feed"));
        assert!(rendered.contains("Fixture channel · YouTube · 13,045 subscribers"));
        assert!(
            !rendered.contains("YouTube · 2026 July 25"),
            "split subscription rows must also omit the repeated source"
        );
        assert!(
            rendered.contains("▶ Fixture video"),
            "split subscription rows keep only the active-playback marker"
        );
        assert!(rendered.contains("[R] Refresh videos"));
        assert!(rendered.contains("[i] Details"));
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleSubscriptionDescription)
        );
        assert!(hit_map.subscription_source_rows.width > 0);
        assert!(hit_map.subscription_item_rows.width > 0);
        let (_, description_target) = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::ToggleSubscriptionDescription)
            .expect("description button target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: description_target.x,
                    row: description_target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ToggleSubscriptionDescription)
        );
        let (_, refresh_target) = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::RefreshSubscriptionVideos)
            .expect("subscription refresh button target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: refresh_target.x,
                    row: refresh_target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::RefreshSubscriptionVideos)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE), &view),
            Some(UiAction::RefreshSubscriptionVideos)
        );

        view.subscriptions.description_expanded = true;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw split description");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Expanded fixture description"));
        assert!(rendered.contains("[R] Refresh videos"));

        view.subscriptions.description_expanded = false;
        view.subscriptions.source_subscriber_count = None;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw split list without public subscriber count");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Fixture channel · YouTube"));
        assert!(!rendered.contains("Fixture channel · YouTube · "));
        assert!(rendered.contains("[i] Details"));
    }

    #[test]
    fn rss_subscription_rows_use_episode_labels_and_podcast_details() {
        let backend = TestBackend::new(140, 28);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let episode_id = MediaId::new(SourceKind::Rss, "rss-v1-fixture");
        let mut view = ViewModel {
            screen: Screen::Subscriptions,
            details: Some(DetailView {
                media_id: Some(episode_id.clone()),
                title: "Fixture RSS episode".to_owned(),
                source: "RSS podcast".to_owned(),
                description: "Fixture episode description".to_owned(),
                length: "12:34".to_owned(),
                ..DetailView::default()
            }),
            subscriptions: SubscriptionsView {
                route: SubscriptionRoute::Items,
                focus: SubscriptionPane::Items,
                source_title: "Fixture RSS show".to_owned(),
                source_kind: SubscriptionKind::Rss,
                source_subscriber_count: Some(99_999),
                items: vec![RowView {
                    media_id: Some(episode_id),
                    title: "Fixture RSS episode".to_owned(),
                    subtitle: "2026 July 30 (today) · 12:34".to_owned(),
                    source: "RSS podcast".to_owned(),
                    compact: true,
                    ..RowView::default()
                }],
                ..SubscriptionsView::default()
            },
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw RSS episode route");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Fixture RSS show · RSS/Atom"));
        assert!(rendered.contains("Fixture RSS episode"));
        assert!(rendered.contains("[R] Refresh episodes"));
        assert!(!rendered.contains("subscribers"));
        assert!(!rendered.contains("Refresh videos"));

        view.subscriptions.route = SubscriptionRoute::Sources;
        view.subscriptions.focus = SubscriptionPane::Sources;
        view.subscriptions.sources = vec![RowView {
            title: "Portable fixture title".to_owned(),
            subtitle: "https://podcasts.example/feed.xml".to_owned(),
            source: "RSS podcast".to_owned(),
            subscribed: true,
            ..RowView::default()
        }];
        view.details = Some(DetailView {
            title: "Fixture RSS show".to_owned(),
            source: "RSS podcast".to_owned(),
            description: "Authors: Fixture host\nEpisodes: 1".to_owned(),
            links: vec![DetailLinkView {
                label: "Podcast website".to_owned(),
                url: "https://podcasts.example/show".to_owned(),
                wikidata_item_id: None,
                ..DetailLinkView::default()
            }],
            ..DetailView::default()
        });
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw RSS source route");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Authors: Fixture host"));
        assert!(!rendered.contains("Subscribers:"));
        assert!(!rendered.contains("xdg-open channel"));
    }

    #[test]
    fn subscription_mouse_rows_target_their_independent_lists() {
        let view = ViewModel {
            screen: Screen::Subscriptions,
            subscriptions: SubscriptionsView {
                layout: SubscriptionsLayout::Split,
                sources: vec![subscription_row("Source", false)],
                items: vec![subscription_row("Video", true)],
                ..SubscriptionsView::default()
            },
            ..ViewModel::default()
        };
        let hit_map = HitMap {
            subscription_source_rows: Rect::new(1, 2, 20, 4),
            subscription_item_rows: Rect::new(30, 2, 20, 4),
            ..HitMap::default()
        };
        let click = |column| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(
            mouse_action(click(1), &hit_map, &view),
            Some(UiAction::SelectSubscriptionSource(0))
        );
        assert_eq!(
            mouse_action(click(30), &hit_map, &view),
            Some(UiAction::SelectSubscriptionItem(0))
        );
    }

    #[test]
    fn narrow_expanded_subscription_buttons_never_overlap_mouse_targets() {
        let backend = TestBackend::new(38, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            screen: Screen::Subscriptions,
            details: Some(DetailView {
                description: "Expanded fixture description".to_owned(),
                ..DetailView::default()
            }),
            subscriptions: SubscriptionsView {
                layout: SubscriptionsLayout::Split,
                focus: SubscriptionPane::Items,
                description_expanded: true,
                sources: vec![subscription_row("Source", false)],
                items: vec![subscription_row("Video", true)],
                ..SubscriptionsView::default()
            },
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw narrow expanded subscription");

        let back = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::ToggleSubscriptionDescription)
            .map(|(_, target)| *target)
            .expect("back target");
        let refresh = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::RefreshSubscriptionVideos)
            .map(|(_, target)| *target)
            .expect("refresh target");
        assert!(
            back.right() <= refresh.x || refresh.right() <= back.x,
            "visible buttons must own disjoint mouse regions"
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: refresh.x,
                    row: refresh.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::RefreshSubscriptionVideos)
        );
    }

    #[test]
    fn subscription_lists_keep_late_selection_visible_and_mouse_indexes_aligned() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            screen: Screen::Subscriptions,
            ..ViewModel::default()
        };
        view.subscriptions.sources = (0..20)
            .map(|index| subscription_row(&format!("Source {index:02}"), false))
            .collect();
        view.subscriptions.selected_source = 17;
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw scrolled subscriptions");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Source 17"));
        assert!(!rendered.contains("Source 00"));
        assert!(hit_map.subscription_source_first_index > 0);
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: hit_map.subscription_source_rows.x,
                    row: hit_map.subscription_source_rows.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SelectSubscriptionSource(
                hit_map.subscription_source_first_index
            ))
        );
    }

    #[test]
    fn preferences_popup_is_modal_selectable_and_clickable() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            preferences_popup: Some(PreferencesPopupView {
                subscriptions_layout: SubscriptionsLayout::DrillDown,
                skip_advertisement_chapters: true,
                youtube_prewarm: true,
                youtube_thumbnail_size: YouTubeThumbnailSize::Standard,
                show_images_in_tty: true,
                show_local_folder_sizes: true,
                bandcamp_audio_format: BandcampAudioFormat::BestAvailable,
                config_path: "/tmp/youta/config.toml".to_owned(),
                environment_override: None,
                validation_error: None,
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw preferences");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Youta preferences"));
        assert!(rendered.contains("[d] Drill-down"));
        assert!(rendered.contains("[s] Split"));
        assert!(rendered.contains("[y] Prepare selected YouTube audio: on"));
        #[cfg(feature = "images")]
        assert!(rendered.contains("[t] YouTube thumbnails: 640×480 (standard)"));
        #[cfg(not(feature = "images"))]
        assert!(rendered.contains("YouTube thumbnails: unavailable in this build"));
        assert!(rendered.contains("[f] Show Local folder sizes: on"));
        #[cfg(feature = "images")]
        assert!(rendered.contains("[i] Show images in TTY: on"));
        #[cfg(not(feature = "images"))]
        assert!(rendered.contains("[i] Show images in TTY: unavailable in this build"));
        #[cfg(feature = "bandcamp")]
        assert!(rendered.contains("[b] Bandcamp audio: Best available"));
        assert!(rendered.contains("/tmp/youta/config.toml"));
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), &view),
            Some(UiAction::SetSubscriptionsLayout(SubscriptionsLayout::Split))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &view),
            Some(UiAction::SubmitPreferences)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleYouTubePrewarm)
        );
        #[cfg(feature = "images")]
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), &view),
            Some(UiAction::CycleYouTubeThumbnailSize)
        );
        #[cfg(not(feature = "images"))]
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), &view),
            None
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleLocalFolderSizes)
        );
        #[cfg(feature = "images")]
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleTtyImages)
        );
        #[cfg(not(feature = "images"))]
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &view),
            None
        );
        #[cfg(feature = "bandcamp")]
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &view),
            Some(UiAction::CycleBandcampAudioFormat)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &view),
            None,
            "the modal must not leak top-level tab cycling"
        );
        let (_, split_target) = hit_map
            .preferences_buttons
            .iter()
            .find(|(action, _)| {
                action == &UiAction::SetSubscriptionsLayout(SubscriptionsLayout::Split)
            })
            .expect("split choice target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: split_target.x,
                    row: split_target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SetSubscriptionsLayout(SubscriptionsLayout::Split))
        );
        let (_, youtube_prewarm_target) = hit_map
            .preferences_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::ToggleYouTubePrewarm)
            .expect("YouTube-prewarm target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: youtube_prewarm_target.x,
                    row: youtube_prewarm_target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ToggleYouTubePrewarm)
        );
        #[cfg(feature = "images")]
        {
            let (_, thumbnail_size_target) = hit_map
                .preferences_buttons
                .iter()
                .find(|(action, _)| action == &UiAction::CycleYouTubeThumbnailSize)
                .expect("YouTube thumbnail-size target");
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: thumbnail_size_target.x,
                        row: thumbnail_size_target.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                Some(UiAction::CycleYouTubeThumbnailSize)
            );
        }
        let (_, folder_size_target) = hit_map
            .preferences_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::ToggleLocalFolderSizes)
            .expect("Local folder-size target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: folder_size_target.x,
                    row: folder_size_target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ToggleLocalFolderSizes)
        );
        #[cfg(feature = "images")]
        {
            let (_, tty_images_target) = hit_map
                .preferences_buttons
                .iter()
                .find(|(action, _)| action == &UiAction::ToggleTtyImages)
                .expect("TTY-images target");
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: tty_images_target.x,
                        row: tty_images_target.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                Some(UiAction::ToggleTtyImages)
            );
        }
        #[cfg(not(feature = "images"))]
        assert!(
            hit_map
                .preferences_buttons
                .iter()
                .all(|(action, _)| action != &UiAction::ToggleTtyImages),
            "an unavailable preference must not expose a mouse hitbox"
        );
        #[cfg(feature = "bandcamp")]
        {
            let (_, bandcamp_format_target) = hit_map
                .preferences_buttons
                .iter()
                .find(|(action, _)| action == &UiAction::CycleBandcampAudioFormat)
                .expect("Bandcamp format target");
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: bandcamp_format_target.x,
                        row: bandcamp_format_target.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                Some(UiAction::CycleBandcampAudioFormat)
            );
        }
        for (action, label) in [
            (UiAction::SubmitPreferences, "[Enter] Save"),
            (UiAction::DismissPreferences, "[Esc] Cancel"),
        ] {
            let target = hit_map
                .preferences_buttons
                .iter()
                .find_map(|(candidate, area)| (candidate == &action).then_some(*area))
                .expect("visible Preferences footer target");
            let visible_label = (target.x..target.right())
                .map(|column| {
                    terminal.backend().buffer()[(column, target.y)]
                        .symbol()
                        .to_owned()
                })
                .collect::<String>();
            assert_eq!(
                visible_label, label,
                "Preferences footer target must cover its rendered label"
            );
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: target.x,
                        row: target.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                Some(action)
            );
        }
    }

    #[test]
    fn diagnostic_keyboard_routing_precedes_stacked_preferences_and_text_selection() {
        let view = ViewModel {
            text_selection_mode: true,
            preferences_popup: Some(PreferencesPopupView {
                subscriptions_layout: SubscriptionsLayout::DrillDown,
                skip_advertisement_chapters: true,
                youtube_prewarm: true,
                youtube_thumbnail_size: YouTubeThumbnailSize::Standard,
                show_images_in_tty: true,
                show_local_folder_sizes: true,
                bandcamp_audio_format: BandcampAudioFormat::BestAvailable,
                config_path: "/tmp/youta/config.toml".to_owned(),
                environment_override: None,
                validation_error: Some("save failed".to_owned()),
            }),
            error_popup: Some(ErrorPopupView {
                title: "Preferences failed".to_owned(),
                report: "complete report".to_owned(),
                ..ErrorPopupView::default()
            }),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &view),
            Some(UiAction::DismissErrorPopup)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &view),
            Some(UiAction::CopyErrorReport)
        );
        let target = Rect::new(2, 3, 6, 1);
        let hit_map = HitMap {
            error_buttons: vec![(UiAction::CopyErrorReport, target)],
            preferences_buttons: vec![(
                UiAction::SetSubscriptionsLayout(SubscriptionsLayout::Split),
                target,
            )],
            detail_text_rows: vec![SelectableDetailsRow {
                x: target.x,
                y: target.y,
                cells: vec!["x".to_owned(); usize::from(target.width)],
            }],
            ..HitMap::default()
        };
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: target.x,
                    row: target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::CopyErrorReport)
        );
    }

    #[test]
    fn youtube_search_order_remains_keyboard_accessible_without_footer_duplication() {
        let backend = TestBackend::new(240, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            youtube_search_sort: YouTubeSearchSort::Newest,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw newest ordering");
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT),
                &view
            ),
            Some(UiAction::ToggleYouTubeSearchSort)
        );
        assert!(!rendered_text(&terminal).contains("[N] Sort:"));
        assert!(
            hit_map
                .buttons
                .iter()
                .all(|(action, _)| action != &UiAction::ToggleYouTubeSearchSort)
        );
    }

    #[test]
    fn creative_commons_filter_remains_keyboard_accessible_without_footer_duplication() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            youtube_creative_commons_only: true,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw enabled CC filter");
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT),
                &view
            ),
            Some(UiAction::ToggleYouTubeCreativeCommons)
        );
        assert!(!rendered_text(&terminal).contains("[C] CC:"));
        assert!(
            hit_map
                .buttons
                .iter()
                .all(|(action, _)| action != &UiAction::ToggleYouTubeCreativeCommons)
        );
    }

    #[test]
    fn video_row_shows_publication_date_while_details_omit_published_line() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            rows: vec![RowView {
                title: "Fixture video".to_owned(),
                subtitle: "Fixture channel · 2026 July 25 · 4:05".to_owned(),
                source: "YouTube".to_owned(),
                ..RowView::default()
            }],
            details: Some(DetailView {
                title: "Fixture video".to_owned(),
                source: "YouTube".to_owned(),
                published: "2026 July 25".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw publication metadata");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(
            rendered.contains("Fixture channel · 2026 July 25 · 4:05"),
            "{rendered}"
        );
        assert!(!rendered.contains("Published:"));
    }

    #[test]
    fn playable_details_show_playlist_actions_and_wrapped_selectable_membership() {
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let media_id = MediaId::new(SourceKind::YouTube, "fixture-episode");
        let view = ViewModel {
            rows: vec![RowView {
                title: "Fixture episode".to_owned(),
                ..RowView::default()
            }],
            details: Some(DetailView {
                media_id: Some(media_id.clone()),
                title: "Fixture episode".to_owned(),
                playlist_names: vec![
                    "todo".to_owned(),
                    "A playlist name long enough to wrap".to_owned(),
                ],
                ..DetailView::default()
            }),
            playlist_item: Some(PlaylistItemView {
                media_id,
                title: "Fixture episode".to_owned(),
                in_todo: true,
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw playlist membership");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("[l] Remove from todo"));
        assert!(rendered.contains("[P] Playlist…"));
        assert!(rendered.contains("Playlists: todo"));
        assert!(rendered.contains("A playlist name"));

        let selectable = hit_map
            .detail_text_rows
            .iter()
            .map(|row| row.cells.concat())
            .collect::<Vec<_>>();
        assert!(
            selectable
                .iter()
                .any(|line| line.contains("Playlists: todo"))
        );
        assert!(
            selectable
                .iter()
                .any(|line| line.contains("A playlist name")),
            "wrapped continuation must remain selectable: {selectable:?}"
        );
        for expected in [UiAction::ToggleTodoPlaylist, UiAction::OpenPlaylistPopup] {
            let (_, target) = hit_map
                .detail_buttons
                .iter()
                .find(|(action, _)| action == &expected)
                .expect("playlist Details button");
            if expected == UiAction::ToggleTodoPlaylist {
                assert_eq!(
                    target.width,
                    terminal_text_width("[l] Remove from todo"),
                    "the Details hit region must cover the full explicit action label"
                );
            }
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: target.x,
                        row: target.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                Some(expected)
            );
        }
    }

    #[test]
    fn details_hide_playlist_actions_when_the_visible_item_has_a_different_identity() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            details: Some(DetailView {
                media_id: Some(MediaId::new(SourceKind::YouTube, "linked-video")),
                title: "Linked video".to_owned(),
                ..DetailView::default()
            }),
            playlist_item: Some(PlaylistItemView {
                media_id: MediaId::new(SourceKind::YouTube, "selected-video"),
                title: "Selected video".to_owned(),
                in_todo: false,
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_details(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    0,
                    &Theme::new(false),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw linked-video Details");

        let rendered = rendered_text(&terminal);
        assert!(!rendered.contains("[l] Add to todo"));
        assert!(!rendered.contains("[l] Remove from todo"));
        assert!(!rendered.contains("[P] Playlist…"));
        assert!(hit_map.detail_buttons.iter().all(|(action, _)| !matches!(
            action,
            UiAction::ToggleTodoPlaylist | UiAction::OpenPlaylistPopup
        )));
    }

    #[test]
    fn completed_empty_search_says_nothing_found_instead_of_requesting_a_selection() {
        let mut terminal = Terminal::new(TestBackend::new(90, 18)).expect("terminal");
        let view = ViewModel {
            screen: Screen::YandexMusic,
            search_query: "missing fixture".to_owned(),
            yandex_music_route: YandexMusicRouteView::Search,
            rows: Vec::new(),
            search_activity: None,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_details(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    0,
                    &Theme::new(false),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw empty search details");

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Nothing found"));
        assert!(!rendered.contains("Select an item to load details lazily"));
    }

    #[test]
    fn details_omit_playlist_membership_line_when_the_item_has_none() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let media_id = MediaId::new(SourceKind::YouTube, "unsorted-episode");
        let view = ViewModel {
            rows: vec![RowView {
                title: "Unsorted episode".to_owned(),
                ..RowView::default()
            }],
            details: Some(DetailView {
                media_id: Some(media_id.clone()),
                title: "Unsorted episode".to_owned(),
                playlist_names: Vec::new(),
                ..DetailView::default()
            }),
            playlist_item: Some(PlaylistItemView {
                media_id,
                title: "Unsorted episode".to_owned(),
                in_todo: false,
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw item without playlist memberships");

        let rendered = rendered_text(&terminal);
        assert!(!rendered.contains("Playlists:"));
        assert!(
            hit_map
                .detail_text_rows
                .iter()
                .flat_map(|row| row.cells.iter())
                .all(|line| !line.contains("Playlists:"))
        );
    }

    #[test]
    fn playlist_metadata_omits_video_only_statistics() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            screen: Screen::Playlists,
            rows: vec![RowView {
                title: "Research".to_owned(),
                ..RowView::default()
            }],
            details: Some(DetailView {
                title: "Research".to_owned(),
                description: "Items to study later\n3 items".to_owned(),
                ..DetailView::default()
            }),
            playlist_edit_available: true,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw playlist metadata");

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("[e] Edit playlist"));
        assert!(rendered.contains("Items to study later"));
        assert!(!rendered.contains("Length:"));
        assert!(!rendered.contains("Likes:"));
        assert!(!rendered.contains("Views:"));
    }

    #[test]
    fn focused_details_render_scrolled_description_and_visible_scrollbar() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let description = (0..=60)
            .map(|line| format!("DETAIL_LINE_{line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let view = ViewModel {
            details: Some(DetailView {
                title: "Fixture video".to_owned(),
                source: "YouTube".to_owned(),
                description,
                ..DetailView::default()
            }),
            details_focused: true,
            details_scroll: 10,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw focused details");
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(!rendered.contains("DETAIL_LINE_00"));
        assert!(rendered.contains("DETAIL_LINE_10"), "{rendered}");
        assert!(
            buffer.content().iter().any(|cell| cell.symbol() == "█"),
            "overflowing details must render a scrollbar thumb"
        );
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .all(|(action, _)| action != &UiAction::ToggleTextSelectionMode),
            "scrolling must not restore the removed Select mode button"
        );
        assert!(hit_map.details_panel.width > 0);
        assert!(hit_map.details_panel.height > 0);
    }

    #[test]
    fn details_end_scroll_reaches_last_wrapped_row_and_scrollbar_bottom() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let description = (0..=60)
            .map(|line| format!("BOTTOM_SCROLL_LINE_{line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut view = ViewModel {
            details: Some(DetailView {
                title: "Long description".to_owned(),
                source: "YouTube".to_owned(),
                description,
                ..DetailView::default()
            }),
            details_focused: true,
            details_scroll: usize::MAX,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw description end");

        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("BOTTOM_SCROLL_LINE_60"), "{rendered}");
        assert!(!rendered.contains("BOTTOM_SCROLL_LINE_00"), "{rendered}");
        assert!(
            !rendered.contains("Description"),
            "the Details panel must not add a Description heading"
        );
        let scrollbar_bottom = (
            hit_map.details_panel.right().saturating_sub(1),
            hit_map.details_panel.bottom().saturating_sub(1),
        );
        assert_eq!(
            buffer[scrollbar_bottom].symbol(),
            "█",
            "the scrollbar thumb must reach the final track cell"
        );
        assert_eq!(
            hit_map.details_scroll_offset,
            hit_map.details_scroll_maximum
        );
        assert!(hit_map.details_scroll_maximum >= 3);

        let rendered_at_end = rendered;
        let upward = mouse_action(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: hit_map.details_panel.x,
                row: hit_map.details_panel.y,
                modifiers: KeyModifiers::NONE,
            },
            &hit_map,
            &view,
        );
        let expected_offset = hit_map.details_scroll_maximum - 3;
        assert_eq!(
            upward,
            Some(UiAction::SetDetailsScroll(expected_offset)),
            "the first upward wheel notch must use the visible clamped offset"
        );

        view.details_scroll = expected_offset;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw one wheel notch above description end");
        let rendered_after_wheel = rendered_text(&terminal);
        assert_eq!(hit_map.details_scroll_offset, expected_offset);
        assert_ne!(
            rendered_after_wheel, rendered_at_end,
            "one upward wheel notch must visibly change the wrapped text window"
        );
    }

    #[test]
    fn description_timecodes_are_unicode_safe_click_targets_and_selection_takes_priority() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let media_id = MediaId::new(SourceKind::YouTube, "abcdefghijk");
        let description =
            "Вступление\n00:01:35 Многоэтажное великолепие Батуми\n01:02:51 Relocation";
        let first_start = description.find("00:01:35").expect("first timestamp");
        let second_start = description.find("01:02:51").expect("second timestamp");
        let mut view = ViewModel {
            details: Some(DetailView {
                media_id: Some(media_id.clone()),
                description: description.to_owned(),
                timecodes: vec![
                    DetailTimecodeView {
                        start_byte: first_start,
                        end_byte: first_start + "00:01:35".len(),
                        seconds: 95,
                        is_chapter: true,
                    },
                    DetailTimecodeView {
                        start_byte: second_start,
                        end_byte: second_start + "01:02:51".len(),
                        seconds: 3_771,
                        is_chapter: true,
                    },
                ],
                ..DetailView::default()
            }),
            playing_media_id: Some(media_id.clone()),
            playback: PlaybackStatus {
                position: Duration::from_secs(100),
                ..PlaybackStatus::default()
            },
            playback_chapters: vec![
                Chapter {
                    title: "Многоэтажное великолепие Батуми".to_owned(),
                    start_seconds: 95,
                    end_seconds: Some(3_771),
                },
                Chapter {
                    title: "Relocation".to_owned(),
                    start_seconds: 3_771,
                    end_seconds: None,
                },
            ],
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw clickable timecodes");
        let (_, target) = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| {
                action
                    == &UiAction::ActivateTimecode {
                        media_id: media_id.clone(),
                        seconds: 95,
                    }
            })
            .expect("first timecode hit target");
        let target = *target;
        let inactive_target = hit_map
            .detail_buttons
            .iter()
            .find_map(|(action, target)| {
                (action
                    == &UiAction::ActivateTimecode {
                        media_id: media_id.clone(),
                        seconds: 3_771,
                    })
                    .then_some(*target)
            })
            .expect("second timecode hit target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: target.x,
                    row: target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ActivateTimecode {
                media_id: media_id.clone(),
                seconds: 95,
            })
        );
        assert_eq!(
            terminal.backend().buffer()[(target.x, target.y)].fg,
            Color::Magenta,
            "the active chapter timestamp must use restrained terminal-palette pink"
        );
        assert!(
            terminal.backend().buffer()[(target.x, target.y)]
                .modifier
                .contains(Modifier::UNDERLINED),
            "a clickable timestamp must be visibly distinct"
        );
        assert!(
            terminal.backend().buffer()[(target.x, target.y)]
                .modifier
                .contains(Modifier::BOLD),
            "the active chapter timestamp must be bold"
        );
        let active_title_cell = (
            target.x.saturating_add(target.width).saturating_add(1),
            target.y,
        );
        assert_eq!(
            terminal.backend().buffer()[active_title_cell].fg,
            Color::Magenta,
            "the active chapter title must share the timestamp's pink foreground"
        );
        assert!(
            terminal.backend().buffer()[active_title_cell]
                .modifier
                .contains(Modifier::BOLD),
            "the active chapter description line must be bold"
        );
        assert_eq!(
            terminal.backend().buffer()[(inactive_target.x, inactive_target.y)].fg,
            Color::Cyan,
            "inactive clickable timecodes must retain the normal accent"
        );
        assert!(
            !terminal.backend().buffer()[(inactive_target.x, inactive_target.y)]
                .modifier
                .contains(Modifier::BOLD),
            "inactive chapter timestamps must not inherit the active style"
        );

        let funny_settings = UiSettings {
            funny_mode: true,
            ..UiSettings::default()
        };
        terminal
            .draw(|frame| render(frame, &view, &funny_settings, &mut hit_map))
            .expect("draw active timecode in DOS theme");
        assert_eq!(
            terminal.backend().buffer()[(target.x, target.y)].fg,
            Color::LightMagenta,
            "the active chapter must follow the DOS theme's palette pink"
        );

        view.text_selection_mode = true;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw selection mode");
        assert!(matches!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: target.x,
                    row: target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::BeginDetailsTextSelection(_))
        ));
    }

    #[test]
    fn wrapped_description_keeps_video_action_immediately_after_its_url() {
        let url = "https://youtu.be/dQw4w9WgXcQ";
        let description = format!("prefix {url} suffix");
        let start_byte = description.find(url).expect("fixture URL");
        let links = [DetailVideoLinkView {
            start_byte,
            end_byte: start_byte + url.len(),
            video_id: "dQw4w9WgXcQ".to_owned(),
            start_seconds: None,
        }];

        let wrapped = wrap_description_source(&description, 11, &links);
        let logical = wrapped
            .iter()
            .flat_map(|line| line.tokens.iter())
            .map(|token| match token {
                WrappedDescriptionToken::Source {
                    start_byte,
                    end_byte,
                } => description[*start_byte..*end_byte].to_owned(),
                WrappedDescriptionToken::VideoAction { .. } => {
                    DESCRIPTION_VIDEO_ACTION_SYMBOL.to_owned()
                }
            })
            .collect::<String>();

        assert_eq!(
            terminal_text_width(DESCRIPTION_VIDEO_ACTION_SYMBOL),
            1,
            "the inline action must occupy exactly one terminal cell"
        );
        assert_eq!(logical, format!("prefix {url}↪ suffix"));
        assert!(wrapped.iter().all(|line| {
            line.tokens
                .iter()
                .map(|token| match token {
                    WrappedDescriptionToken::Source {
                        start_byte,
                        end_byte,
                    } => usize::from(terminal_text_width(&description[*start_byte..*end_byte])),
                    WrappedDescriptionToken::VideoAction { .. } => 1,
                })
                .sum::<usize>()
                <= 11
        }));
    }

    #[test]
    fn rendered_video_action_has_an_exact_one_cell_internal_hitbox() {
        let backend = TestBackend::new(92, 26);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let url = "https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=90";
        let description =
            format!("A deliberately long prefix makes this URL wrap: {url} then continue.");
        let start_byte = description.find(url).expect("fixture URL");
        let expected = UiAction::ActivateDescriptionVideo {
            video_id: "dQw4w9WgXcQ".to_owned(),
            start_seconds: Some(90),
        };
        let view = ViewModel {
            details: Some(DetailView {
                description,
                video_links: vec![DetailVideoLinkView {
                    start_byte,
                    end_byte: start_byte + url.len(),
                    video_id: "dQw4w9WgXcQ".to_owned(),
                    start_seconds: Some(90),
                }],
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw wrapped internal-video action");

        let [(action, area)] = hit_map.description_video_actions.as_slice() else {
            panic!("expected exactly one internal-video action");
        };
        assert_eq!(action, &expected);
        assert_eq!(area.width, 1);
        assert_eq!(
            terminal.backend().buffer()[(area.x, area.y)].symbol(),
            DESCRIPTION_VIDEO_ACTION_SYMBOL
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: area.x,
                    row: area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(expected.clone())
        );
        assert_ne!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: area.x.saturating_add(1),
                    row: area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(expected),
            "the neighboring cell must not activate the video"
        );
    }

    #[test]
    fn details_render_clickable_local_subscription_without_duplicate_channel_name() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            details: Some(DetailView {
                title: "Mock video".to_owned(),
                source: "YouTube".to_owned(),
                channel_name: "Fixture channel".to_owned(),
                channel_id: "UCfixture".to_owned(),
                channel_webpage_url: Some(
                    url::Url::parse("https://www.youtube.com/@ქართული")
                        .expect("fixture channel URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let settings = UiSettings::default();
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &settings, &mut hit_map))
            .expect("draw unsubscribed details");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(
            !rendered.contains("Channel: Fixture channel"),
            "the selected channel name is already visible in the left panel"
        );
        assert!(
            !rendered.contains("Mock video"),
            "the selected title is already visible in the left panel"
        );
        assert!(
            !rendered.contains("Source:"),
            "the selected source is already visible in the left panel"
        );
        assert!(rendered.contains("[s] Subscribe (locally)"));
        assert!(rendered.contains(&format!("[o] {} video", system_url_opener_name())));
        assert!(rendered.contains(&format!(
            "[O] {} channel · @ქართული",
            system_url_opener_name()
        )));
        let (_, subscribe_area) = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::ToggleSubscription)
            .expect("local subscribe hit target");
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: subscribe_area.x,
            row: subscribe_area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click, &hit_map, &view),
            Some(UiAction::ToggleSubscription)
        );
        let (_, open_area) = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::OpenInBrowser)
            .expect("external video opener hit target");
        let open_click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: open_area.x,
            row: open_area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(open_click, &hit_map, &view),
            Some(UiAction::OpenInBrowser)
        );
        let (_, open_channel_area) = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::OpenChannelInBrowser)
            .expect("external channel opener hit target");
        assert_eq!(
            open_channel_area.y, hit_map.details_panel.y,
            "the channel opener should reclaim the removed Select mode row"
        );
        assert_eq!(
            open_area.y,
            open_channel_area.y.saturating_add(1),
            "the video opener should follow the channel opener"
        );
        let expected_right = hit_map
            .details_panel
            .x
            .saturating_add(hit_map.details_panel.width);
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .all(|(action, _)| action != &UiAction::ToggleTextSelectionMode)
        );
        assert_eq!(
            open_channel_area.x.saturating_add(open_channel_area.width),
            expected_right,
            "the channel opener should be right-aligned"
        );
        assert_eq!(
            open_area.x.saturating_add(open_area.width),
            expected_right,
            "the video opener should be right-aligned"
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: open_channel_area.x,
                    row: open_channel_area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::OpenChannelInBrowser)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleSubscription)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT),
                &view
            ),
            Some(UiAction::OpenChannelInBrowser)
        );

        view.details
            .as_mut()
            .expect("fixture details")
            .channel_subscribed = true;
        terminal
            .draw(|frame| render(frame, &view, &settings, &mut hit_map))
            .expect("draw subscribed details");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("[s] Unsubscribe (locally)"));
    }

    #[test]
    fn left_detail_actions_fill_unused_rows_before_right_controls() {
        let media_id = MediaId::new(SourceKind::YouTube, "fixture-video");
        let view = ViewModel {
            video_comments_available: true,
            private_note_available: true,
            playlist_item: Some(PlaylistItemView {
                media_id: media_id.clone(),
                title: "Fixture video".to_owned(),
                in_todo: false,
            }),
            details: Some(DetailView {
                media_id: Some(media_id),
                title: "Fixture video".to_owned(),
                source: "YouTube".to_owned(),
                channel_id: "UCfixture".to_owned(),
                channel_name: "Fixture channel".to_owned(),
                channel_webpage_url: Some(
                    url::Url::parse("https://www.youtube.com/@fixture")
                        .expect("fixture channel URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(160, 32)).expect("terminal");
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| {
                render(frame, &view, &UiSettings::default(), &mut hit_map);
            })
            .expect("draw paired detail actions");

        let area_for = |action: &UiAction| {
            hit_map
                .detail_buttons
                .iter()
                .find_map(|(candidate, area)| (candidate == action).then_some(*area))
                .unwrap_or_else(|| panic!("missing detail action {action:?}"))
        };
        for (left, right) in [
            (UiAction::ToggleTodoPlaylist, UiAction::OpenVideoComments),
            (UiAction::OpenPlaylistPopup, UiAction::OpenChannelInBrowser),
            (UiAction::EditPrivateNote, UiAction::OpenInBrowser),
        ] {
            let left_area = area_for(&left);
            let right_area = area_for(&right);
            assert_eq!(
                left_area.y, right_area.y,
                "the left action should reuse the earliest compatible right-control row"
            );
            assert!(
                left_area.right().saturating_add(2) <= right_area.x,
                "paired controls must retain a two-cell gap"
            );
        }
    }

    #[test]
    fn detail_action_rail_requires_its_complete_spaced_height() {
        let buttons = (0..8)
            .map(|line_index| DetailButtonPlacement {
                line_index,
                column: 0,
                label: "[x] Action".to_owned(),
                style: Style::default(),
                action: UiAction::SetDetailsFocus(true),
            })
            .collect::<Vec<_>>();
        let details = DetailView {
            source: "Local audio".to_owned(),
            ..DetailView::default()
        };
        let sizing = ThumbnailSizing::fixed(30);

        assert!(
            detail_action_rail(&buttons, 120, 14, 0, 0, sizing, &details, true).is_none(),
            "a wide pane one row short must keep the compact controls above artwork"
        );
        let exact_fit = detail_action_rail(&buttons, 120, 15, 0, 0, sizing, &details, true)
            .expect("the complete 15-row rail should fit");
        assert_eq!(exact_fit.height, 15);
    }

    #[test]
    fn wide_subscription_channel_uses_the_shared_spaced_action_rail() {
        let mut view = ViewModel {
            screen: Screen::Subscriptions,
            external_opener_available: true,
            details: Some(DetailView {
                title: "Fixture channel".to_owned(),
                source: "YouTube".to_owned(),
                description: "Channel description below artwork.".to_owned(),
                thumbnail_url: Some(
                    url::Url::parse("https://images.example/channel-action-rail.jpg")
                        .expect("fixture channel artwork URL"),
                ),
                channel_id: "UCfixture".to_owned(),
                channel_name: "Fixture channel".to_owned(),
                channel_webpage_url: Some(
                    url::Url::parse("https://www.youtube.com/@fixture")
                        .expect("fixture channel URL"),
                ),
                channel_subscriber_count: Some(1_234),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        view.subscriptions.source_kind = SubscriptionKind::YouTube;
        view.subscriptions.source_title = "Fixture channel".to_owned();
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).expect("terminal");
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            rendered_artwork: true,
            prepared_artwork_size: Some(Size::new(90, 8)),
            ..MockThumbnailRenderer::default()
        };
        let theme = Theme::new(false);

        terminal
            .draw(|frame| {
                render_subscription_source_details(
                    frame,
                    Rect::new(0, 0, 140, 30),
                    &view,
                    true,
                    12,
                    &theme,
                    &mut hit_map,
                    Some(&mut thumbnails),
                );
            })
            .expect("draw subscription channel action rail");

        let area_for = |action: &UiAction| {
            hit_map
                .detail_buttons
                .iter()
                .find_map(|(candidate, area)| (candidate == action).then_some(*area))
                .unwrap_or_else(|| panic!("missing channel action {action:?}"))
        };
        let subscribe_area = area_for(&UiAction::ToggleSubscription);
        let open_area = area_for(&UiAction::OpenChannelInBrowser);
        let thumbnail_area = hit_map
            .thumbnail_area
            .expect("ready channel artwork hitbox");
        assert_eq!(subscribe_area.y, thumbnail_area.y);
        assert_eq!(open_area.y, subscribe_area.y.saturating_add(2));
        assert!(
            thumbnail_area.right().saturating_add(2) <= subscribe_area.x
                && thumbnail_area.right().saturating_add(2) <= open_area.x
        );
        assert_eq!(
            thumbnail_area.y,
            hit_map.details_panel.y.saturating_add(1),
            "channel statistics must remain above the shared artwork block"
        );
        let buffer = terminal.backend().buffer();
        let description_y = (0..buffer.area.height)
            .find(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("Channel description below artwork.")
            })
            .expect("rendered channel description");
        assert_eq!(
            description_y,
            thumbnail_area
                .bottom()
                .max(open_area.bottom())
                .saturating_add(1),
            "one blank row must separate channel artwork and controls from its description"
        );
        assert!(rendered_text(&terminal).contains("Subscribers: 1,234"));
    }

    #[test]
    fn wide_artwork_places_all_detail_actions_in_a_spaced_right_rail() {
        let media_id = MediaId::new(SourceKind::YouTube, "fixture-video");
        let view = ViewModel {
            video_comments_available: true,
            private_note_available: true,
            playlist_item: Some(PlaylistItemView {
                media_id: media_id.clone(),
                title: "Fixture video".to_owned(),
                in_todo: false,
            }),
            details: Some(DetailView {
                media_id: Some(media_id),
                title: "Fixture video".to_owned(),
                source: "YouTube".to_owned(),
                length: "12:34".to_owned(),
                views: "1,234".to_owned(),
                likes: "56".to_owned(),
                comments: "78".to_owned(),
                description: format!(
                    "Description starts below the shared artwork block. {}",
                    "x".repeat(150)
                ),
                thumbnail_url: Some(
                    url::Url::parse("https://images.example/wide-action-rail.jpg")
                        .expect("fixture thumbnail URL"),
                ),
                thumbnail_dimensions: Some((640, 480)),
                channel_id: "UCfixture".to_owned(),
                channel_name: "Fixture channel".to_owned(),
                channel_webpage_url: Some(
                    url::Url::parse("https://www.youtube.com/@fixture")
                        .expect("fixture channel URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(180, 44)).expect("terminal");
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            rendered_artwork: true,
            prepared_artwork_size: Some(Size::new(120, 20)),
            ..MockThumbnailRenderer::default()
        };
        let theme = Theme::new(false);

        terminal
            .draw(|frame| {
                render_details_with_terminal_window(
                    frame,
                    Rect::new(0, 0, 180, 44),
                    &view,
                    true,
                    12,
                    None,
                    &theme,
                    &mut hit_map,
                    Some(&mut thumbnails),
                );
            })
            .expect("draw wide detail-action rail");

        let thumbnail_area = hit_map.thumbnail_area.expect("ready artwork hitbox");
        let expected_actions = [
            UiAction::ToggleTodoPlaylist,
            UiAction::OpenVideoComments,
            UiAction::OpenPlaylistPopup,
            UiAction::OpenChannelInBrowser,
            UiAction::EditPrivateNote,
            UiAction::OpenInBrowser,
            UiAction::ToggleSubscription,
        ];
        let mut action_areas = expected_actions
            .iter()
            .map(|expected| {
                hit_map
                    .detail_buttons
                    .iter()
                    .find_map(|(action, area)| (action == expected).then_some(*area))
                    .unwrap_or_else(|| panic!("missing detail action {expected:?}"))
            })
            .collect::<Vec<_>>();
        action_areas.sort_by_key(|area| area.y);

        assert_eq!(
            hit_map.detail_buttons.len(),
            expected_actions.len(),
            "every visible detail action must participate in the rail"
        );
        assert_eq!(
            thumbnail_area.y,
            hit_map.details_panel.y.saturating_add(1),
            "only the statistics row may precede artwork when the selected title is already visible"
        );
        assert_eq!(
            action_areas.first().map(|area| area.y),
            Some(thumbnail_area.y),
            "the action rail and artwork must start on the same row"
        );
        assert!(
            action_areas
                .windows(2)
                .all(|pair| pair[1].y == pair[0].y.saturating_add(2)),
            "one empty row must separate every adjacent action"
        );
        assert!(
            action_areas
                .iter()
                .all(|area| thumbnail_area.right().saturating_add(2) <= area.x),
            "every action must retain a two-cell gutter to the right of the artwork"
        );
        let expected_labels = [
            "[l] Add to todo".to_owned(),
            "[F6] Twenty comments".to_owned(),
            "[P] Playlist…".to_owned(),
            format!("[O] {} channel · @fixture", system_url_opener_name()),
            "[n] Add private note".to_owned(),
            format!("[o] {} video", system_url_opener_name()),
            "[s] Subscribe (locally)".to_owned(),
        ];
        for ((expected, expected_label), area) in expected_actions
            .iter()
            .zip(expected_labels.iter())
            .zip(action_areas.iter())
        {
            let rendered_label = (area.left()..area.right())
                .map(|x| terminal.backend().buffer()[(x, area.y)].symbol())
                .collect::<String>();
            assert_eq!(
                rendered_label,
                expected_label.as_str(),
                "the visible rail label must describe its exact mouse action"
            );
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: area.x,
                        row: area.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                Some(expected.clone()),
                "the exact visible label must retain its action"
            );
        }
        let spacer = action_areas[0];
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: spacer.x,
                    row: spacer.y.saturating_add(1),
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SetDetailsFocus(true)),
            "the empty row between labels must not activate either action"
        );
        for pair in action_areas.windows(2) {
            let spacer_y = pair[0].y.saturating_add(1);
            assert!(
                (pair[0].x..hit_map.details_panel.right()).all(|x| terminal.backend().buffer()
                    [(x, spacer_y)]
                    .symbol()
                    .chars()
                    .all(char::is_whitespace)),
                "the visual spacer row between actions must remain empty"
            );
        }
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: thumbnail_area.x,
                    row: thumbnail_area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ToggleThumbnailExpansion),
            "moving controls must preserve the exact artwork expansion target"
        );
        let details_row = hit_map
            .detail_text_rows
            .iter()
            .find(|row| {
                row.cells
                    .concat()
                    .contains("Description starts below the shared artwork block.")
            })
            .expect("description row");
        let rail_bottom = action_areas.last().expect("last action").bottom();
        assert_eq!(
            details_row.y,
            thumbnail_area.bottom().max(rail_bottom).saturating_add(1),
            "one blank row must separate artwork and controls from the description"
        );
        assert_eq!(
            details_row.x, hit_map.details_panel.x,
            "description text must reclaim the pane's left edge"
        );
        assert!(
            u16::try_from(details_row.cells.len()).unwrap_or(u16::MAX) > thumbnail_area.width,
            "description wrapping must reclaim more width than the artwork column"
        );
    }

    #[test]
    fn narrow_detail_actions_preserve_order_after_pairing_stops() {
        let media_id = MediaId::new(SourceKind::YouTube, "fixture-video");
        let view = ViewModel {
            video_comments_available: true,
            private_note_available: true,
            playlist_item: Some(PlaylistItemView {
                media_id: media_id.clone(),
                title: "Fixture video".to_owned(),
                in_todo: false,
            }),
            details: Some(DetailView {
                media_id: Some(media_id),
                source: "YouTube".to_owned(),
                channel_id: "UCfixture".to_owned(),
                channel_webpage_url: Some(
                    url::Url::parse("https://www.youtube.com/@fixture")
                        .expect("fixture channel URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(62, 32)).expect("terminal");
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw narrow paired detail actions");

        let actions = [
            UiAction::ToggleTodoPlaylist,
            UiAction::OpenPlaylistPopup,
            UiAction::EditPrivateNote,
            UiAction::ToggleSubscription,
        ];
        let placements = actions.each_ref().map(|expected| {
            let (_, area) = hit_map
                .detail_buttons
                .iter()
                .find(|(action, _)| action == expected)
                .expect("left-side detail action");
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: area.x,
                        row: area.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                Some(expected.clone())
            );
            assert_eq!(terminal.backend().buffer()[(area.x, area.y)].symbol(), "[");
            *area
        });
        assert!(
            placements.windows(2).all(|pair| pair[0].y < pair[1].y),
            "left-side actions must retain their logical order on narrow panes"
        );
        let rendered = rendered_text(&terminal);
        for label in [
            "[l] Add to todo",
            "[P] Playlist…",
            "[n] Add private note",
            "[s] Subscribe (locally)",
        ] {
            assert!(rendered.contains(label), "missing rendered label {label:?}");
        }
    }

    #[test]
    fn narrow_artwork_keeps_all_actions_above_a_full_width_thumbnail() {
        let media_id = MediaId::new(SourceKind::YouTube, "fixture-video");
        let view = ViewModel {
            video_comments_available: true,
            private_note_available: true,
            playlist_item: Some(PlaylistItemView {
                media_id: media_id.clone(),
                title: "Fixture video".to_owned(),
                in_todo: false,
            }),
            details: Some(DetailView {
                media_id: Some(media_id),
                title: "Fixture video".to_owned(),
                source: "YouTube".to_owned(),
                description: "Narrow fallback description.".to_owned(),
                thumbnail_url: Some(
                    url::Url::parse("https://images.example/narrow-fallback.jpg")
                        .expect("fixture thumbnail URL"),
                ),
                thumbnail_dimensions: Some((640, 480)),
                channel_id: "UCfixture".to_owned(),
                channel_webpage_url: Some(
                    url::Url::parse("https://www.youtube.com/@fixture")
                        .expect("fixture channel URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 44)).expect("terminal");
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            rendered_artwork: true,
            prepared_artwork_size: Some(Size::new(80, 20)),
            ..MockThumbnailRenderer::default()
        };
        let theme = Theme::new(false);

        terminal
            .draw(|frame| {
                render_details_with_terminal_window(
                    frame,
                    Rect::new(0, 0, 80, 44),
                    &view,
                    true,
                    12,
                    None,
                    &theme,
                    &mut hit_map,
                    Some(&mut thumbnails),
                );
            })
            .expect("draw narrow artwork fallback");

        let [(_, requested_thumbnail)] = thumbnails.synchronized.as_slice() else {
            panic!("expected one synchronized thumbnail");
        };
        assert_eq!(requested_thumbnail.x, hit_map.details_panel.x);
        assert_eq!(requested_thumbnail.width, hit_map.details_panel.width);
        for expected in [
            UiAction::OpenVideoComments,
            UiAction::OpenChannelInBrowser,
            UiAction::OpenInBrowser,
            UiAction::ToggleTodoPlaylist,
            UiAction::OpenPlaylistPopup,
            UiAction::EditPrivateNote,
            UiAction::ToggleSubscription,
        ] {
            let (_, area) = hit_map
                .detail_buttons
                .iter()
                .find(|(action, _)| action == &expected)
                .unwrap_or_else(|| panic!("missing narrow fallback action {expected:?}"));
            assert!(
                area.bottom() <= requested_thumbnail.y,
                "{expected:?} must remain above full-width artwork"
            );
        }
    }

    #[test]
    fn channel_handle_display_accepts_one_trailing_slash_but_rejects_extra_path() {
        let handle =
            url::Url::parse("https://www.youtube.com/@myChanName/").expect("fixture handle");
        assert_eq!(
            youtube_channel_handle(Some(&handle)).as_deref(),
            Some("@myChanName")
        );

        for unsafe_url in [
            "https://www.youtube.com/@myChanName//",
            "https://www.youtube.com/@myChanName/videos",
        ] {
            let url = url::Url::parse(unsafe_url).expect("unsafe fixture URL");
            assert_eq!(
                youtube_channel_handle(Some(&url)),
                None,
                "{unsafe_url:?} must not be displayed as an exact channel handle"
            );
        }
    }

    #[test]
    fn narrow_details_keep_right_controls_clipped_separate_and_clickable() {
        let backend = TestBackend::new(42, 18);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            details: Some(DetailView {
                title: "Mock video".to_owned(),
                source: "YouTube".to_owned(),
                channel_name: "A channel name longer than the panel".to_owned(),
                channel_id: "UCfixture".to_owned(),
                channel_webpage_url: Some(
                    url::Url::parse("https://www.youtube.com/@myChanName")
                        .expect("fixture channel URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render(frame, &view, &UiSettings::default(), &mut hit_map);
            })
            .expect("draw narrow details");

        let placements =
            [UiAction::OpenChannelInBrowser, UiAction::OpenInBrowser].map(|expected| {
                let (_, area) = hit_map
                    .detail_buttons
                    .iter()
                    .find(|(action, _)| action == &expected)
                    .expect("right-side control");
                assert!(area.width > 0);
                assert!(area.x >= hit_map.details_panel.x);
                assert!(
                    area.x.saturating_add(area.width)
                        <= hit_map
                            .details_panel
                            .x
                            .saturating_add(hit_map.details_panel.width)
                );
                assert_eq!(
                    mouse_action(
                        MouseEvent {
                            kind: MouseEventKind::Down(MouseButton::Left),
                            column: area.x,
                            row: area.y,
                            modifiers: KeyModifiers::NONE,
                        },
                        &hit_map,
                        &view,
                    ),
                    Some(expected)
                );
                *area
            });
        assert_eq!(placements[1].y, placements[0].y.saturating_add(1));
    }

    #[test]
    fn physical_linux_console_hides_and_blocks_external_openers() {
        let attachment = TerminalAttachment {
            linux: true,
            stdin_is_terminal: true,
            stdout_is_terminal: true,
            term: Some("linux".to_owned()),
            ssh: false,
            tmux: false,
            output_device: Some(PathBuf::from("/dev/tty3")),
        };
        let mut view = ViewModel {
            external_opener_available: attachment.external_opener_available(),
            details: Some(DetailView {
                source: "YouTube".to_owned(),
                channel_id: "UCfixture".to_owned(),
                channel_webpage_url: Some(
                    url::Url::parse("https://www.youtube.com/@fixture")
                        .expect("fixture channel URL"),
                ),
                links: vec![DetailLinkView {
                    label: "Fixture website".to_owned(),
                    url: "https://example.com/fixture".to_owned(),
                    ..DetailLinkView::default()
                }],
                ..DetailView::default()
            }),
            selected_detail_link: Some(0),
            ..ViewModel::default()
        };
        assert!(!view.external_opener_available);

        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw physical-console details");
        let rendered = rendered_text(&terminal);

        assert!(!rendered.contains(system_url_opener_name()));
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .all(|(action, _)| !action.requires_external_opener())
        );
        assert!(hit_map.detail_links.is_empty());
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &view),
            None
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT),
                &view
            ),
            None
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), &view),
            None
        );

        let stale_external_target = Rect::new(3, 4, 8, 1);
        let stale_hit_map = HitMap {
            detail_buttons: vec![(UiAction::OpenInBrowser, stale_external_target)],
            description_video_actions: vec![(
                UiAction::ActivateDescriptionVideo {
                    video_id: "dQw4w9WgXcQ".to_owned(),
                    start_seconds: None,
                },
                Rect::new(20, 4, 1, 1),
            )],
            ..HitMap::default()
        };
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: stale_external_target.x,
                    row: stale_external_target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &stale_hit_map,
                &view,
            ),
            None,
            "a stale hit map must not bypass physical-console policy"
        );
        assert!(matches!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 20,
                    row: 4,
                    modifiers: KeyModifiers::NONE,
                },
                &stale_hit_map,
                &view,
            ),
            Some(UiAction::ActivateDescriptionVideo { .. })
        ));

        view.youtube_setup_popup = Some(YouTubeSetupPopupView::default());
        hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw physical-console setup popup");
        let rendered = rendered_text(&terminal);
        assert!(!rendered.contains("[F1]"));
        assert!(!rendered.contains("[F2]"));
        assert!(!rendered.contains("[F3]"));
        assert_eq!(hit_map.youtube_setup_buttons.len(), 2);
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), &view),
            None
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &view),
            Some(UiAction::SubmitYouTubeSetup)
        );

        view.youtube_setup_popup = None;
        view.error_popup = Some(ErrorPopupView {
            gh_available: true,
            ..ErrorPopupView::default()
        });
        hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw physical-console error popup");
        assert!(!rendered_text(&terminal).contains("Copy + open issue"));
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &view),
            None
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &view),
            Some(UiAction::RequestGitHubIssueSubmission)
        );

        view.error_popup = Some(yt_dlp_forbidden_error(true));
        hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw physical-console yt-dlp popup");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains(YT_DLP_PROJECT_URL));
        assert!(rendered.contains(GENTOO_YT_DLP_PACKAGE_URL));
        assert!(!rendered.contains("[u] Project"));
        assert!(!rendered.contains("[p] Gentoo package"));
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE), &view),
            None
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), &view),
            None
        );
        assert!(
            hit_map
                .error_buttons
                .iter()
                .all(|(action, _)| !action.requires_external_opener())
        );
    }

    #[test]
    fn graphical_pty_keeps_external_opener_controls_and_hotkeys() {
        let attachment = TerminalAttachment {
            linux: true,
            stdin_is_terminal: true,
            stdout_is_terminal: true,
            term: Some("xterm-256color".to_owned()),
            ssh: false,
            tmux: false,
            output_device: Some(PathBuf::from("/dev/pts/7")),
        };
        let view = ViewModel {
            external_opener_available: attachment.external_opener_available(),
            details: Some(DetailView {
                source: "YouTube".to_owned(),
                channel_id: "UCfixture".to_owned(),
                channel_webpage_url: Some(
                    url::Url::parse("https://www.youtube.com/@fixture")
                        .expect("fixture channel URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        assert!(view.external_opener_available);

        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw PTY details");
        let rendered = rendered_text(&terminal);

        assert!(rendered.contains(&format!("[o] {} video", system_url_opener_name())));
        assert!(rendered.contains(&format!("[O] {} channel", system_url_opener_name())));
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .any(|(action, _)| action == &UiAction::OpenInBrowser)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &view),
            Some(UiAction::OpenInBrowser)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT),
                &view
            ),
            Some(UiAction::OpenChannelInBrowser)
        );
    }

    #[test]
    fn history_details_omit_video_only_controls_and_statistics() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            screen: Screen::History,
            rows: vec![RowView {
                title: "Local fixture".to_owned(),
                subtitle: "stopped at 0:42".to_owned(),
                source: "local".to_owned(),
                ..RowView::default()
            }],
            details: Some(DetailView {
                title: "Local fixture".to_owned(),
                source: "local".to_owned(),
                description: "stopped at 0:42".to_owned(),
                length: "must not render".to_owned(),
                likes: "must not render".to_owned(),
                views: "must not render".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw History details");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("stopped at 0:42"));
        assert!(!rendered.contains(&format!("{} video", system_url_opener_name())));
        assert!(!rendered.contains("Length:"));
        assert!(!rendered.contains("Likes:"));
        assert!(!rendered.contains("Views:"));
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .all(|(action, _)| action != &UiAction::OpenInBrowser)
        );
    }

    #[cfg(feature = "radio")]
    #[test]
    fn radio_rows_and_details_render_as_live_station_controls() {
        let backend = TestBackend::new(140, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let media_id = MediaId::new(SourceKind::Radio, "sector-radio-progressive-flac");
        let view = ViewModel {
            screen: Screen::Radio,
            rows: vec![RowView {
                media_id: Some(media_id.clone()),
                title: "Sector Radio — Progressive".to_owned(),
                subtitle: "FLAC · variable bitrate · 44.1 kHz".to_owned(),
                source: "Radio".to_owned(),
                watched_percent: 42,
                hide_watched_marker: true,
                compact: true,
                radio_favorite: true,
                ..RowView::default()
            }],
            playing_media_id: Some(media_id),
            playback: PlaybackStatus {
                idle: false,
                paused: false,
                ..PlaybackStatus::default()
            },
            radio_recording: Some(RadioRecordingView {
                station_id: "sector-radio-progressive-flac".to_owned(),
                station_name: "Sector Radio — Progressive".to_owned(),
            }),
            details: Some(DetailView {
                title: "Sector Radio — Progressive".to_owned(),
                source: "Radio".to_owned(),
                channel_webpage_url: Some(
                    url::Url::parse("https://sectorradio.com/").expect("station URL"),
                ),
                description: "Lossless progressive electronic music.\n\nQuality: FLAC · variable bitrate · 44.1 kHz\nStream: http://89.223.45.5:8000/progressive-flac".to_owned(),
                length: "must not render".to_owned(),
                likes: "must not render".to_owned(),
                views: "must not render".to_owned(),
                radio_favorite: true,
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw Radio details");
        let rendered = rendered_text(&terminal);

        assert!(rendered.contains("▶ ★ ● Sector Radio — Progressive"));
        assert!(rendered.contains("[f] Unfavorite"));
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .any(|(action, _)| action == &UiAction::ToggleRadioRecording)
        );
        assert!(!rendered.contains("▶ ● Sector Radio"));
        assert!(!rendered.contains("42%"));
        assert!(rendered.contains("FLAC · variable bitrate · 44.1 kHz"));
        assert!(!rendered.contains("stereo"));
        assert!(rendered.contains(&format!(
            "[O] {} · https://sectorradio.com/",
            system_url_opener_name()
        )));
        assert!(!rendered.contains("External links"));
        assert!(!rendered.contains("Station website"));
        assert!(!rendered.contains("[B] Sort:"));
        assert!(!rendered.contains("Length:"));
        assert!(!rendered.contains("Likes:"));
        assert!(!rendered.contains("Views:"));
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .any(|(action, _)| action == &UiAction::OpenInBrowser)
        );
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .any(|(action, _)| action == &UiAction::ToggleRadioFavorite)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT),
                &view
            ),
            Some(UiAction::OpenInBrowser)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('B'), KeyModifiers::SHIFT),
                &view
            ),
            Some(UiAction::CycleRadioSort)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleRadioFavorite)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleRadioRecording)
        );
        assert!(
            hit_map
                .buttons
                .iter()
                .all(|(action, _)| action != &UiAction::CycleRadioSort)
        );
        assert_eq!(RadioSort::Name.next(), RadioSort::BitrateDescending);
        assert_eq!(
            RadioSort::BitrateDescending.next(),
            RadioSort::BitrateAscending
        );
        assert_eq!(RadioSort::BitrateAscending.next(), RadioSort::Name);
    }

    #[cfg(feature = "radio")]
    #[test]
    fn radio_play_marker_and_bold_title_follow_playback_identity_not_selection() {
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let selected = MediaId::new(SourceKind::Radio, "selected-station");
        let playing = MediaId::new(SourceKind::Radio, "playing-station");
        let mut view = ViewModel {
            screen: Screen::Radio,
            selected: 0,
            playing_media_id: Some(playing.clone()),
            rows: vec![
                RowView {
                    media_id: Some(selected),
                    title: "Selected station".to_owned(),
                    source: "Radio".to_owned(),
                    hide_watched_marker: true,
                    compact: true,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(playing),
                    title: "Playing station".to_owned(),
                    source: "Radio".to_owned(),
                    hide_watched_marker: true,
                    compact: true,
                    ..RowView::default()
                },
            ],
            playback: PlaybackStatus {
                idle: false,
                paused: false,
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_body(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    &Theme::new(false),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw playing Radio station");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "S");
        assert_eq!(buffer[(0, 1)].symbol(), "▶");
        assert_eq!(buffer[(2, 1)].symbol(), "P");
        assert!(
            buffer[(2, 1)].modifier.contains(Modifier::BOLD),
            "the playing station title must be bold even when another row is selected"
        );

        view.playing_media_id = None;
        terminal
            .draw(|frame| {
                render_body(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    &Theme::new(false),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw stopped Radio stations");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 1)].symbol(), "P");
        assert!(
            !rendered_text(&terminal).contains('▶'),
            "stopped Radio rows must not retain a stale playing marker"
        );
    }

    #[cfg(feature = "apple-podcasts")]
    #[test]
    fn apple_podcast_details_use_podcast_controls_without_video_statistics() {
        let backend = TestBackend::new(100, 22);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            screen: Screen::ApplePodcasts,
            rows: vec![RowView {
                title: "Fixture episode".to_owned(),
                subtitle: "2026 July 27 · 42:05".to_owned(),
                source: "Apple Podcasts".to_owned(),
                ..RowView::default()
            }],
            details: Some(DetailView {
                title: "Fixture episode".to_owned(),
                source: "Apple Podcasts".to_owned(),
                description: "Episode notes".to_owned(),
                channel_webpage_url: Some(
                    url::Url::parse("https://podcasts.example/show").expect("podcast website"),
                ),
                length: "42:05".to_owned(),
                likes: "must not render".to_owned(),
                views: "must not render".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw Apple episode details");
        let rendered = rendered_text(&terminal);

        assert!(rendered.contains(&format!("[o] {} podcast", system_url_opener_name())));
        assert!(rendered.contains("Length: 42:05"));
        assert!(!rendered.contains(&format!("{} video", system_url_opener_name())));
        assert!(!rendered.contains(&format!("{} channel", system_url_opener_name())));
        assert!(!rendered.contains("Likes:"));
        assert!(!rendered.contains("Views:"));
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .any(|(action, _)| action == &UiAction::OpenInBrowser)
        );
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .all(|(action, _)| action != &UiAction::OpenChannelInBrowser)
        );

        view.rows[0].title = "Fixture show".to_owned();
        view.details = Some(DetailView {
            title: "Fixture show".to_owned(),
            source: "Apple Podcasts".to_owned(),
            description: "Show notes".to_owned(),
            channel_webpage_url: Some(
                url::Url::parse("https://podcasts.example/show").expect("podcast website"),
            ),
            ..DetailView::default()
        });
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw Apple show details");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains(&format!("[o] {} podcast", system_url_opener_name())));
        assert!(!rendered.contains("Length:"));
        assert!(!rendered.contains("Likes:"));
        assert!(!rendered.contains("Views:"));
    }

    #[test]
    fn librivox_book_details_render_audiobook_metadata_links_and_cover() {
        let cover_url = url::Url::parse("https://archive.org/download/fixture/cover.jpg")
            .expect("LibriVox cover URL");
        let author_url = "https://librivox.org/author/4517";
        let keyword_url = "https://librivox.org/search?primary_key=Palestine";
        let media_id = MediaId::new(SourceKind::LibriVox, "123/456");
        let mut view = ViewModel {
            screen: Screen::LibriVox,
            external_opener_available: true,
            rows: vec![RowView {
                media_id: Some(media_id.clone()),
                title: "With the Turks in Palestine".to_owned(),
                subtitle: "Alexander Aaronsohn · 6:03".to_owned(),
                source: "LibriVox".to_owned(),
                ..RowView::default()
            }],
            details: Some(DetailView {
                media_id: Some(media_id.clone()),
                title: "With the Turks in Palestine".to_owned(),
                source: "LibriVox".to_owned(),
                length: "6:03".to_owned(),
                description: "Genres: Travel, Memoir\nA public-domain audiobook description."
                    .to_owned(),
                license: "Public domain in the United States".to_owned(),
                likes: "must not render".to_owned(),
                views: "must not render".to_owned(),
                links: vec![
                    DetailLinkView {
                        prefix: "Author: ".to_owned(),
                        label: "Alexander Aaronsohn".to_owned(),
                        url: author_url.to_owned(),
                        presentation: DetailLinkPresentation::LabelOnlySpaced,
                        internal_target: Some(DetailLinkInternalTarget::LibriVoxAuthor(
                            "4517".to_owned(),
                        )),
                        ..DetailLinkView::default()
                    },
                    DetailLinkView {
                        prefix: "Keywords: ".to_owned(),
                        label: "Palestine".to_owned(),
                        url: keyword_url.to_owned(),
                        presentation: DetailLinkPresentation::LabelOnly,
                        ..DetailLinkView::default()
                    },
                ],
                thumbnail_url: Some(cover_url.clone()),
                thumbnail_dimensions: Some((1200, 1200)),
                ..DetailView::default()
            }),
            playlist_item: Some(PlaylistItemView {
                media_id,
                title: "With the Turks in Palestine".to_owned(),
                in_todo: false,
            }),
            private_note_available: true,
            ..ViewModel::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(180, 54)).expect("LibriVox terminal");
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            rendered_artwork: true,
            prepared_artwork_size: Some(Size::new(18, 8)),
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &view,
                    &UiSettings::default(),
                    &mut hit_map,
                    Some(&mut thumbnails),
                );
            })
            .expect("draw LibriVox book details");
        let rendered = rendered_text(&terminal);

        assert!(rendered.contains(&format!("[o] {} audiobook", system_url_opener_name())));
        assert!(rendered.contains("Length: 6:03"));
        assert!(rendered.contains("Genres: Travel, Memoir"));
        assert!(rendered.contains("License: Public domain in the United States"));
        assert!(rendered.contains("Author: Alexander Aaronsohn"));
        assert!(rendered.contains("Keywords: Palestine"));
        assert!(!rendered.contains(author_url));
        assert!(!rendered.contains(keyword_url));
        assert!(!rendered.contains("Likes:"));
        assert!(!rendered.contains("Views:"));
        assert!(rendered.contains("[l] Add to todo"));
        assert!(rendered.contains("[P] Playlist…"));
        assert!(rendered.contains("[n] Add private note"));
        assert!(rendered.contains("THUMBNAIL IMAGE"));
        assert!(hit_map.thumbnail_area.is_some());
        assert_eq!(thumbnails.synchronized.len(), 1);
        assert_eq!(thumbnails.synchronized[0].0.as_ref(), Some(&cover_url));

        let keyword_area = hit_map
            .detail_links
            .iter()
            .find_map(|(index, area)| (*index == 1).then_some(*area))
            .expect("compact keyword link target");
        assert_eq!(keyword_area.width, terminal_text_width("Palestine"));
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: keyword_area.x,
                    row: keyword_area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ActivateDetailLink(1))
        );

        view.external_opener_available = false;
        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &view,
                    &UiSettings::default(),
                    &mut hit_map,
                    Some(&mut thumbnails),
                );
            })
            .expect("draw LibriVox details without a browser");
        assert!(hit_map.detail_links.is_empty());
        let author_action = UiAction::OpenLibriVoxAuthorById("4517".to_owned());
        let author_marker = hit_map
            .detail_buttons
            .iter()
            .find_map(|(action, area)| (action == &author_action).then_some(*area))
            .expect("LibriVox author internal marker");
        assert_eq!(author_marker.width, terminal_text_width("↪"));
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: author_marker.x,
                    row: author_marker.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(author_action)
        );
    }

    #[test]
    fn librivox_search_list_and_child_navigation_keep_shared_controls() {
        let media_id = MediaId::new(SourceKind::LibriVox, "123/456");
        let mut view = ViewModel {
            screen: Screen::LibriVox,
            details: Some(DetailView {
                media_id: Some(media_id.clone()),
                ..DetailView::default()
            }),
            playlist_item: Some(PlaylistItemView {
                media_id,
                title: "Fixture section".to_owned(),
                in_todo: false,
            }),
            private_note_available: true,
            ..ViewModel::default()
        };

        for (key, expected) in [
            (KeyCode::Char('/'), UiAction::BeginSearch),
            (KeyCode::Char('j'), UiAction::MoveSelection(1)),
            (KeyCode::Char('k'), UiAction::MoveSelection(-1)),
            (KeyCode::Enter, UiAction::ActivateSelection),
            (KeyCode::Char('l'), UiAction::ToggleTodoPlaylist),
            (KeyCode::Char('P'), UiAction::OpenPlaylistPopup),
            (KeyCode::Char('n'), UiAction::EditPrivateNote),
            (KeyCode::Esc, UiAction::GoBack),
            (KeyCode::Backspace, UiAction::GoBack),
        ] {
            assert_eq!(
                key_action(KeyEvent::new(key, KeyModifiers::NONE), &view),
                Some(expected),
                "LibriVox omitted {key:?} from its shared control surface"
            );
        }
        assert_eq!(
            key_action_with_page_rows(
                KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
                &view,
                Some(6),
                None,
            ),
            Some(UiAction::MoveSelection(-6))
        );
        assert_eq!(
            key_action_with_page_rows(
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                &view,
                Some(6),
                None,
            ),
            Some(UiAction::MoveSelection(6))
        );

        view.search_editing = true;
        view.search_query = "public domain".to_owned();
        view.search_cursor_byte = "public".len();
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &view),
            Some(UiAction::MoveSearchCursor(-1))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &view),
            Some(UiAction::MoveSearchCursor(1))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &view),
            Some(UiAction::SubmitSearch)
        );
    }

    #[test]
    fn details_keep_text_selection_on_the_keyboard_without_a_panel_button() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            details: Some(DetailView {
                title: "Selectable fixture".to_owned(),
                description: "Drag across this text".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw Details without a text-selection button");
        assert!(!rendered_text(&terminal).contains("Select mode"));
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .all(|(action, _)| action != &UiAction::ToggleTextSelectionMode)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleTextSelectionMode)
        );

        view.text_selection_mode = true;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw active text-selection mode");
        assert!(!rendered_text(&terminal).contains("Exit select mode"));
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .all(|(action, _)| action != &UiAction::ToggleTextSelectionMode)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &view),
            Some(UiAction::ToggleTextSelectionMode),
            "Esc must still leave keyboard-only text-selection mode"
        );
        let (selectable_x, selectable_y) = hit_map
            .detail_text_rows
            .first()
            .map(|row| (row.x, row.y))
            .expect("selectable title row");
        let anchor = DetailsTextPosition { row: 0, column: 0 };
        view.details_text_selection = Some(DetailsTextSelection {
            anchor,
            focus: DetailsTextPosition { row: 0, column: 5 },
            dragging: true,
        });
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw highlighted selection");
        assert_eq!(
            terminal.backend().buffer()[(selectable_x, selectable_y)].bg,
            Color::Cyan
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Drag(MouseButton::Left),
                    column: selectable_x.saturating_add(4),
                    row: selectable_y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::UpdateDetailsTextSelection(DetailsTextPosition {
                row: 0,
                column: 4
            }))
        );
    }

    #[test]
    fn selected_channel_name_uses_black_foreground_on_selected_background() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            search_kind: SearchKind::Channels,
            rows: vec![RowView {
                title: "Zebra channel".to_owned(),
                subtitle: "42 subscribers".to_owned(),
                source: "YouTube channel".to_owned(),
                ..RowView::default()
            }],
            selected: 0,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw selected channel");

        let title_cell = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .find(|cell| cell.symbol() == "Z")
            .expect("selected channel title cell");
        assert_eq!(title_cell.fg, Color::Black);
        assert_eq!(title_cell.bg, Color::Cyan);
    }

    #[test]
    fn details_subscription_button_obeys_hidden_hotkey_setting() {
        let backend = TestBackend::new(120, 28);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            details: Some(DetailView {
                title: "Mock video".to_owned(),
                channel_name: "Fixture channel".to_owned(),
                channel_id: "UCfixture".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let settings = UiSettings {
            show_hotkeys: false,
            ..UiSettings::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &settings, &mut hit_map))
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("Subscribe (locally)"));
        assert!(!rendered.contains("[s] Subscribe (locally)"));
        assert!(!rendered.contains("Select mode"));
        assert!(rendered.contains(&format!("{} video", system_url_opener_name())));
        assert!(!rendered.contains(&format!("[o] {} video", system_url_opener_name())));
        assert!(!rendered.contains(&format!("[O] {}", system_url_opener_name())));
        assert_eq!(hit_map.detail_buttons.len(), 2);
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .all(|(action, _)| action != &UiAction::ToggleTextSelectionMode)
        );
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .any(|(action, _)| action == &UiAction::OpenInBrowser)
        );
    }

    #[test]
    fn details_render_license_row_only_for_recognized_creative_commons_labels() {
        for (source, hidden_license) in [
            ("YouTube", ""),
            ("YouTube", "unknown"),
            ("YouTube", "not applicable"),
            ("YouTube", "publisher terms"),
            ("YouTube", "Standard YouTube License"),
            ("YouTube", "youtube"),
            ("LibriVox", "publisher terms"),
            ("LibriVox", "public domain"),
        ] {
            let backend = TestBackend::new(120, 28);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let view = ViewModel {
                details: Some(DetailView {
                    title: "Standard-license fixture".to_owned(),
                    source: source.to_owned(),
                    license: hidden_license.to_owned(),
                    wikidata: "not loaded (lazy)".to_owned(),
                    ..DetailView::default()
                }),
                ..ViewModel::default()
            };
            let mut hit_map = HitMap::default();
            terminal
                .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
                .expect("draw");
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            assert!(
                !rendered.contains("License:"),
                "unexpected license row for {hidden_license:?}"
            );
        }

        for (creative_commons_license, expected_display) in [
            (
                "Creative Commons Attribution licence",
                "Creative Commons Attribution",
            ),
            (
                "creative commons attribution LICENSE",
                "Creative Commons Attribution",
            ),
            (
                "https://creativecommons.org/licenses/by/4.0/",
                "https://creativecommons.org/licenses/by/4.0/",
            ),
        ] {
            let backend = TestBackend::new(120, 28);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let view = ViewModel {
                details: Some(DetailView {
                    title: "Creative Commons fixture".to_owned(),
                    source: "YouTube".to_owned(),
                    license: creative_commons_license.to_owned(),
                    wikidata: "not loaded (lazy)".to_owned(),
                    ..DetailView::default()
                }),
                ..ViewModel::default()
            };
            let mut hit_map = HitMap::default();
            terminal
                .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
                .expect("draw");
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            assert!(rendered.contains("License:"));
            assert!(rendered.contains(expected_display));
            if creative_commons_license != expected_display {
                assert!(
                    !rendered.contains(creative_commons_license),
                    "localized spelling must not leak into Details"
                );
            }
        }
    }

    #[test]
    fn details_render_wikidata_only_once_as_an_external_link() {
        let backend = TestBackend::new(120, 28);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            details: Some(DetailView {
                title: "Wikidata fixture".to_owned(),
                source: "YouTube".to_owned(),
                wikidata: "no linked Wikidata item found".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw empty result");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(!rendered.contains("Wikidata:"));
        assert!(!rendered.contains("no linked Wikidata item found"));

        let details = view.details.as_mut().expect("details");
        details.wikidata = "Douglas Adams (Q42): https://www.wikidata.org/wiki/Q42".to_owned();
        details.links.push(DetailLinkView {
            label: "Douglas Adams (Q42)".to_owned(),
            url: "https://www.wikidata.org/wiki/Q42".to_owned(),
            wikidata_item_id: Some("Q42".to_owned()),
            ..DetailLinkView::default()
        });
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw linked result");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(!rendered.contains("Wikidata:"));
        assert!(rendered.contains("Douglas Adams (Q42)"));
        assert_eq!(rendered.matches("Douglas Adams (Q42)").count(), 1);
    }

    #[test]
    fn details_never_render_the_thumbnail_url_as_text() {
        let backend = TestBackend::new(120, 28);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            details: Some(DetailView {
                title: "Thumbnail fixture".to_owned(),
                source: "YouTube".to_owned(),
                thumbnail_url: Some(
                    url::Url::parse("https://i.ytimg.com/vi/fixture/maxresdefault.jpg")
                        .expect("fixture thumbnail URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(!rendered.contains("Thumbnail:"));
        assert!(!rendered.contains("i.ytimg.com"));
        assert!(!rendered.contains("maxresdefault.jpg"));
    }

    #[test]
    fn supported_thumbnail_renderer_receives_only_the_selected_url_and_reserved_area() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let thumbnail_url = url::Url::parse("https://i.ytimg.com/vi/fixture/mqdefault.jpg")
            .expect("fixture thumbnail URL");
        let view = ViewModel {
            details: Some(DetailView {
                title: "Thumbnail fixture".to_owned(),
                source: "YouTube".to_owned(),
                description: vec!["Description remains below the image."; 15].join("\n"),
                thumbnail_url: Some(thumbnail_url.clone()),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let settings = UiSettings::default();
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("THUMBNAIL IMAGE"));
        assert!(rendered.contains("Description remains below"));
        assert!(!rendered.contains(thumbnail_url.as_str()));
        assert_eq!(thumbnails.synchronized.len(), 1);
        assert_eq!(thumbnails.synchronized[0].0.as_ref(), Some(&thumbnail_url));
        assert_eq!(
            thumbnails.synchronized[0].1.height,
            DEFAULT_THUMBNAIL_HEIGHT
        );
        assert_eq!(thumbnails.clear_count, 0);
    }

    #[test]
    fn selected_local_mov_uses_its_midpoint_frame_instead_of_remote_artwork() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let midpoint = LocalVideoThumbnailView {
            path: PathBuf::from("/tmp/youta-video-thumbnail-fixture.MOV"),
            midpoint_millis: 60_500,
        };
        let view = ViewModel {
            details: Some(DetailView {
                title: "Local MOV fixture".to_owned(),
                source: "Local video (audio playback)".to_owned(),
                description: "Full path: /tmp/youta-video-thumbnail-fixture.MOV".to_owned(),
                thumbnail_url: Some(
                    url::Url::parse("https://images.example/stale-artwork.jpg")
                        .expect("stale artwork URL"),
                ),
                local_video_thumbnail: Some(midpoint.clone()),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let settings = UiSettings::default();
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw MOV midpoint frame");

        assert!(
            thumbnails.synchronized.is_empty(),
            "a local video frame must take precedence over stale embedded artwork"
        );
        assert_eq!(thumbnails.synchronized_local_videos.len(), 1);
        assert_eq!(thumbnails.synchronized_local_videos[0].0, midpoint);
        assert_eq!(
            thumbnails.synchronized_local_videos[0].1.height,
            DEFAULT_THUMBNAIL_HEIGHT
        );
        assert_eq!(thumbnails.clear_count, 0);
    }

    #[test]
    fn switching_from_local_video_to_audio_clears_the_stale_midpoint_frame() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            details: Some(DetailView {
                title: "Local MOV fixture".to_owned(),
                source: "Local video (audio playback)".to_owned(),
                local_video_thumbnail: Some(LocalVideoThumbnailView {
                    path: PathBuf::from("/tmp/youta-video-thumbnail-fixture.mov"),
                    midpoint_millis: 20_000,
                }),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let settings = UiSettings::default();
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw local-video midpoint");
        assert_eq!(thumbnails.synchronized_local_videos.len(), 1);
        assert_eq!(thumbnails.clear_count, 0);

        view.details = Some(DetailView {
            title: "Local audio fixture".to_owned(),
            source: "Local audio".to_owned(),
            description: "Full path: /tmp/youta-audio-fixture.flac".to_owned(),
            ..DetailView::default()
        });
        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw local audio without artwork");

        assert_eq!(
            thumbnails.clear_count, 1,
            "the prior midpoint frame must be cleared when the next item has no image"
        );
        assert_eq!(thumbnails.synchronized_local_videos.len(), 1);
        assert!(thumbnails.synchronized.is_empty());
    }

    #[test]
    fn disabled_thumbnail_renderer_does_not_extract_a_local_video_frame() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            details: Some(DetailView {
                title: "Text-only MOV fixture".to_owned(),
                source: "Local video (audio playback)".to_owned(),
                local_video_thumbnail: Some(LocalVideoThumbnailView {
                    path: PathBuf::from("/tmp/youta-video-thumbnail-fixture.mov"),
                    midpoint_millis: 10_000,
                }),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer::default();

        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &view,
                    &UiSettings::default(),
                    &mut hit_map,
                    Some(&mut thumbnails),
                );
            })
            .expect("draw text-only MOV Details");

        assert!(thumbnails.synchronized_local_videos.is_empty());
    }

    #[test]
    fn private_note_popup_obscures_thumbnail_without_clearing_cached_state() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let thumbnail_url = url::Url::parse("https://i.ytimg.com/vi/fixture/mqdefault.jpg")
            .expect("fixture thumbnail URL");
        let mut view = ViewModel {
            details: Some(DetailView {
                title: "Cached thumbnail fixture".to_owned(),
                source: "YouTube".to_owned(),
                description: "Description remains available after editing.".to_owned(),
                thumbnail_url: Some(thumbnail_url.clone()),
                ..DetailView::default()
            }),
            private_note_popup: Some(PrivateNotePopupView {
                target_label: "Cached thumbnail fixture".to_owned(),
                storage_path: "/tmp/youta/state/notes.toml".to_owned(),
                ..PrivateNotePopupView::default()
            }),
            ..ViewModel::default()
        };
        let settings = UiSettings::default();
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            rendered_artwork: true,
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw private-note popup");
        assert_eq!(thumbnails.obscure_count, 1);
        assert_eq!(thumbnails.clear_count, 0);
        assert!(thumbnails.synchronized.is_empty());

        view.private_note_popup = None;
        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw immediately after closing private-note popup");

        assert_eq!(thumbnails.obscure_count, 1);
        assert_eq!(thumbnails.clear_count, 0);
        assert_eq!(thumbnails.synchronized.len(), 1);
        assert_eq!(thumbnails.synchronized[0].0.as_ref(), Some(&thumbnail_url));
        assert!(rendered_text(&terminal).contains("THUMBNAIL IMAGE"));
    }

    #[test]
    fn expanded_wikidata_keeps_source_artwork_and_links_every_p18_value() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let source_thumbnail =
            url::Url::parse("https://images.example/source.jpg").expect("source thumbnail URL");
        let first_p18_preview = url::Url::parse(
            "https://commons.wikimedia.org/wiki/Special:Redirect/file/Douglas%20Adams.jpg",
        )
        .expect("first P18 preview URL");
        let second_p18_preview = url::Url::parse(
            "https://commons.wikimedia.org/wiki/Special:Redirect/file/Douglas%20Adams%202008.jpg",
        )
        .expect("second P18 preview URL");
        let first_p18_page =
            "https://commons.wikimedia.org/wiki/File:Douglas%20Adams.jpg".to_owned();
        let second_p18_page =
            "https://commons.wikimedia.org/wiki/File:Douglas%20Adams%202008.jpg".to_owned();
        let wikidata_text = "image (P18): Douglas Adams.jpg; Douglas Adams 2008.jpg".to_owned();
        let first_start = wikidata_text
            .find("Douglas Adams.jpg")
            .expect("first P18 display");
        let second_start = wikidata_text
            .find("Douglas Adams 2008.jpg")
            .expect("second P18 display");
        let mut view = ViewModel {
            details: Some(DetailView {
                thumbnail_url: Some(source_thumbnail.clone()),
                expanded_wikidata_item: Some("Q42".to_owned()),
                wikidata_entities: vec![DetailWikidataEntityView {
                    item_id: "Q42".to_owned(),
                    text: wikidata_text,
                    value_links: vec![
                        DetailWikidataValueLinkView {
                            start_byte: first_start,
                            end_byte: first_start + "Douglas Adams.jpg".len(),
                            url: first_p18_page.clone(),
                        },
                        DetailWikidataValueLinkView {
                            start_byte: second_start,
                            end_byte: second_start + "Douglas Adams 2008.jpg".len(),
                            url: second_p18_page.clone(),
                        },
                    ],
                    media_controls: Vec::new(),
                    image_url: Some(first_p18_preview.clone()),
                }],
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &view,
                    &UiSettings::default(),
                    &mut hit_map,
                    Some(&mut thumbnails),
                );
            })
            .expect("draw expanded Wikidata with source artwork");

        assert_eq!(thumbnails.synchronized.len(), 1);
        assert_eq!(
            thumbnails.synchronized[0].0.as_ref(),
            Some(&source_thumbnail),
            "P18 must not replace primary provider artwork"
        );
        for page_url in [&first_p18_page, &second_p18_page] {
            assert!(
                hit_map.detail_buttons.iter().any(|(action, _)| {
                    action == &UiAction::OpenWikidataValue(page_url.clone())
                }),
                "each P18 value must remain clickable"
            );
        }

        view.details
            .as_mut()
            .expect("fixture details")
            .thumbnail_url = None;
        let mut fallback_thumbnails = MockThumbnailRenderer {
            enabled: true,
            ..MockThumbnailRenderer::default()
        };
        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &view,
                    &UiSettings::default(),
                    &mut hit_map,
                    Some(&mut fallback_thumbnails),
                );
            })
            .expect("draw bounded P18 fallback");
        assert_eq!(fallback_thumbnails.synchronized.len(), 1);
        assert_eq!(
            fallback_thumbnails.synchronized[0].0.as_ref(),
            Some(&first_p18_preview),
            "without primary artwork only the first ordered P18 preview is rendered"
        );
        assert!(
            fallback_thumbnails
                .synchronized
                .iter()
                .all(|(url, _)| url.as_ref() != Some(&second_p18_preview))
        );
    }

    #[test]
    fn wikidata_media_controls_are_distinct_clickable_and_follow_playback_state() {
        let backend = TestBackend::new(120, 36);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let text = "image (P18): Portrait.jpg\n\
                    audio (P51): ▶ Spoken fixture.opus\n\
                    video (P10): ▶ Video fixture.webm"
            .to_owned();
        let marker_offsets = text
            .match_indices(WIKIDATA_MEDIA_PLAY_SYMBOL)
            .map(|(offset, _)| offset)
            .collect::<Vec<_>>();
        assert_eq!(marker_offsets.len(), 2);
        let audio_page =
            url::Url::parse("https://commons.wikimedia.org/wiki/File:Spoken%20fixture.opus")
                .expect("audio page URL");
        let video_page =
            url::Url::parse("https://commons.wikimedia.org/wiki/File:Video%20fixture.webm")
                .expect("video page URL");
        let audio_id = MediaId::new(SourceKind::WikimediaCommons, audio_page.to_string());
        let media_control =
            |marker_start_byte, media_id, kind, title: &str, webpage_url| DetailWikidataMediaView {
                marker_start_byte,
                marker_end_byte: marker_start_byte + WIKIDATA_MEDIA_PLAY_SYMBOL.len(),
                media_id,
                kind,
                title: title.to_owned(),
                webpage_url,
                playback_url: url::Url::parse(&format!(
                    "https://commons.wikimedia.org/wiki/Special:Redirect/file/{}",
                    title.replace(' ', "%20")
                ))
                .expect("playback URL"),
            };
        let audio = media_control(
            marker_offsets[0],
            audio_id.clone(),
            MediaKind::Audio,
            "Spoken fixture.opus",
            audio_page.clone(),
        );
        let video = media_control(
            marker_offsets[1],
            MediaId::new(SourceKind::WikimediaCommons, video_page.to_string()),
            MediaKind::Video,
            "Video fixture.webm",
            video_page.clone(),
        );
        let links = [
            (
                "Portrait.jpg",
                "https://commons.wikimedia.org/wiki/File:Portrait.jpg",
            ),
            ("Spoken fixture.opus", audio_page.as_str()),
            ("Video fixture.webm", video_page.as_str()),
        ]
        .into_iter()
        .map(|(display, url)| {
            let start_byte = text.find(display).expect("linked value");
            DetailWikidataValueLinkView {
                start_byte,
                end_byte: start_byte + display.len(),
                url: url.to_owned(),
            }
        })
        .collect();
        let mut view = ViewModel {
            details: Some(DetailView {
                thumbnail_url: Some(
                    url::Url::parse("https://images.example/provider.jpg")
                        .expect("provider artwork URL"),
                ),
                expanded_wikidata_item: Some("Q42".to_owned()),
                wikidata_entities: vec![DetailWikidataEntityView {
                    item_id: "Q42".to_owned(),
                    text,
                    value_links: links,
                    media_controls: vec![audio, video],
                    image_url: Some(
                        url::Url::parse(
                            "https://commons.wikimedia.org/wiki/Special:Redirect/file/Portrait.jpg?width=512",
                        )
                        .expect("P18 preview URL"),
                    ),
                }],
                ..DetailView::default()
            }),
            details_focused: true,
            selected_wikidata_media: Some(0),
            playing_media_id: Some(audio_id),
            playback: PlaybackStatus {
                idle: false,
                paused: false,
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw active Wikidata media");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("⏸ Spoken fixture.opus"));
        assert!(rendered.contains("▶ Video fixture.webm"));

        let control_areas = [0, 1].map(|index| {
            hit_map
                .detail_buttons
                .iter()
                .find_map(|(action, area)| {
                    (action == &UiAction::ActivateWikidataMedia(index)).then_some(*area)
                })
                .expect("media control hit target")
        });
        assert_ne!(control_areas[0], control_areas[1]);
        for (index, area) in control_areas.into_iter().enumerate() {
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: area.x,
                        row: area.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                Some(UiAction::ActivateWikidataMedia(index))
            );
        }
        for page in [audio_page.as_str(), video_page.as_str()] {
            assert!(
                hit_map
                    .detail_buttons
                    .iter()
                    .any(|(action, _)| { action == &UiAction::OpenWikidataValue(page.to_owned()) }),
                "the filename must retain its Commons page action"
            );
        }
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &view),
            Some(UiAction::ActivateWikidataMedia(0))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT), &view),
            Some(UiAction::MoveWikidataMedia(1))
        );
        view.selected_wikidata_media = Some(1);
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &view),
            Some(UiAction::ActivateWikidataMedia(1))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT), &view),
            Some(UiAction::MoveWikidataMedia(-1))
        );

        view.playback.paused = true;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw paused Wikidata media");
        let rendered = rendered_text(&terminal);
        assert!(!rendered.contains("⏸ Spoken fixture.opus"));
        assert!(rendered.contains("▶ Spoken fixture.opus"));
        assert!(rendered.contains("▶ Video fixture.webm"));
    }

    #[test]
    fn thumbnail_prefetch_uses_search_rows_and_cached_subscription_source_artwork() {
        let first = url::Url::parse("https://i.ytimg.com/vi/first/mqdefault.jpg")
            .expect("first thumbnail URL");
        let second = url::Url::parse("https://i.ytimg.com/vi/second/mqdefault.jpg")
            .expect("second thumbnail URL");
        let channel = url::Url::parse("https://yt3.ggpht.com/cached-channel=s800")
            .expect("channel artwork URL");
        let mut view = ViewModel {
            screen: Screen::Search,
            rows: vec![
                RowView {
                    thumbnail_url: Some(first.clone()),
                    ..RowView::default()
                },
                RowView::default(),
                RowView {
                    thumbnail_url: Some(second.clone()),
                    ..RowView::default()
                },
            ],
            ..ViewModel::default()
        };
        let mut renderer = MockThumbnailRenderer::default();

        assert!(synchronize_thumbnail_prefetch(
            &view,
            &UiSettings::default(),
            &mut renderer,
        ));
        assert_eq!(renderer.prefetch_batches, [vec![first, second]]);

        view.screen = Screen::Subscriptions;
        view.subscriptions.sources = vec![
            RowView {
                thumbnail_url: Some(channel.clone()),
                ..RowView::default()
            },
            RowView::default(),
        ];
        assert!(synchronize_thumbnail_prefetch(
            &view,
            &UiSettings::default(),
            &mut renderer,
        ));
        assert_eq!(
            renderer.prefetch_batches.last(),
            Some(&vec![channel.clone()]),
            "persisted channel artwork must warm before another source is selected"
        );

        let disabled = UiSettings {
            prefetch_search_thumbnails: false,
            ..UiSettings::default()
        };
        assert!(synchronize_thumbnail_prefetch(
            &view,
            &disabled,
            &mut renderer,
        ));
        assert_eq!(
            renderer.prefetch_batches.last(),
            Some(&vec![channel]),
            "the Search-only preference must not reintroduce channel-switch latency"
        );

        view.screen = Screen::Search;
        assert!(synchronize_thumbnail_prefetch(
            &view,
            &disabled,
            &mut renderer,
        ));
        assert!(renderer.prefetch_batches.last().is_some_and(Vec::is_empty));
    }

    #[test]
    fn thumbnail_prefetch_prioritizes_the_selected_expansion_image() {
        let expanded = url::Url::parse("https://i.ytimg.com/vi/selected/maxresdefault.jpg")
            .expect("expanded thumbnail URL");
        let selected = url::Url::parse("https://i.ytimg.com/vi/selected/sddefault.jpg")
            .expect("selected thumbnail URL");
        let next = url::Url::parse("https://i.ytimg.com/vi/next/sddefault.jpg")
            .expect("next thumbnail URL");
        let view = ViewModel {
            screen: Screen::Search,
            rows: vec![
                RowView {
                    thumbnail_url: Some(selected.clone()),
                    ..RowView::default()
                },
                RowView {
                    thumbnail_url: Some(next.clone()),
                    ..RowView::default()
                },
            ],
            details: Some(DetailView {
                thumbnail_url: Some(selected),
                expanded_thumbnail_url: Some(expanded.clone()),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut renderer = MockThumbnailRenderer::default();

        assert!(synchronize_thumbnail_prefetch(
            &view,
            &UiSettings::default(),
            &mut renderer,
        ));
        assert_eq!(
            renderer.prefetch_batches,
            [vec![
                expanded.clone(),
                view.rows[0]
                    .thumbnail_url
                    .clone()
                    .expect("selected thumbnail"),
                next.clone()
            ]],
            "the click target must warm before list thumbnails without replacing their configured size"
        );

        let no_list_warming = UiSettings {
            prefetch_search_thumbnails: false,
            ..UiSettings::default()
        };
        assert!(synchronize_thumbnail_prefetch(
            &view,
            &no_list_warming,
            &mut renderer,
        ));
        assert_eq!(
            renderer.prefetch_batches.last(),
            Some(&vec![expanded.clone()]),
            "selected expansion warming is independent from the list-prefetch preference"
        );

        let mut same_source = view;
        same_source
            .details
            .as_mut()
            .expect("fixture details")
            .expanded_thumbnail_url = Some(expanded.clone());
        same_source.rows[0].thumbnail_url = Some(expanded.clone());
        assert!(synchronize_thumbnail_prefetch(
            &same_source,
            &UiSettings::default(),
            &mut renderer,
        ));
        assert_eq!(
            renderer.prefetch_batches.last(),
            Some(&vec![expanded, next]),
            "the same configured and expanded URL must be requested only once"
        );
    }

    #[cfg(feature = "yandex-music")]
    #[test]
    fn yandex_original_artwork_is_prefetched_behind_the_bounded_panel_image() {
        let panel =
            url::Url::parse("https://avatars.yandex.net/get-music-content/fixture/1000x1000")
                .expect("bounded Yandex panel artwork URL");
        let original = url::Url::parse("https://avatars.yandex.net/get-music-content/fixture/orig")
            .expect("original Yandex artwork URL");
        let view = ViewModel {
            screen: Screen::YandexMusic,
            rows: vec![RowView {
                thumbnail_url: Some(panel.clone()),
                ..RowView::default()
            }],
            details: Some(DetailView {
                source: "Yandex Music".to_owned(),
                thumbnail_url: Some(panel),
                expanded_thumbnail_url: Some(original.clone()),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut renderer = MockThumbnailRenderer::default();

        assert!(synchronize_thumbnail_prefetch(
            &view,
            &UiSettings::default(),
            &mut renderer,
        ));
        assert_eq!(
            renderer.prefetch_batches,
            [vec![original]],
            "Yandex's original image must warm in the background before fullscreen expansion"
        );
    }

    #[test]
    fn expanded_youtube_artwork_uses_the_largest_advertised_image() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let selected =
            url::Url::parse("https://i.ytimg.com/vi/fixture/sddefault.jpg").expect("selected URL");
        let expanded = url::Url::parse("https://i.ytimg.com/vi/fixture/maxresdefault.jpg")
            .expect("expanded URL");
        let view = ViewModel {
            details: Some(DetailView {
                source: "YouTube".to_owned(),
                thumbnail_url: Some(selected),
                expanded_thumbnail_url: Some(expanded.clone()),
                thumbnail_expanded: true,
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        let mut renderer = MockThumbnailRenderer {
            enabled: true,
            rendered_artwork: true,
            prepared_artwork_size: Some(Size::new(80, 30)),
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &view,
                    &UiSettings::default(),
                    &mut hit_map,
                    Some(&mut renderer),
                );
            })
            .expect("draw expanded YouTube artwork");

        assert_eq!(
            renderer.synchronized[0].0.as_ref(),
            Some(&expanded),
            "the full-terminal overlay must use the largest advertised source"
        );
    }

    #[test]
    fn terminal_pixel_dimensions_keep_independent_nonzero_reports() {
        assert_eq!(
            nonzero_terminal_window_pixels(0, 1_200),
            (None, Some(1_200))
        );
        assert_eq!(
            nonzero_terminal_window_pixels(1_920, 0),
            (Some(1_920), None),
            "YouTube's usable width must survive a missing pixel height"
        );
        assert_eq!(
            nonzero_terminal_window_pixels(2_560, 1_440),
            (Some(2_560), Some(1_440))
        );
    }

    #[cfg(feature = "yandex-music")]
    #[test]
    fn full_hd_yandex_artwork_uses_more_than_the_400_pixel_fallback_area() {
        let backend = TestBackend::new(192, 60);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            screen: Screen::YandexMusic,
            details: Some(DetailView {
                title: "Adaptive Yandex artwork".to_owned(),
                source: "Yandex Music".to_owned(),
                thumbnail_url: Some(
                    url::Url::parse("https://avatars.yandex.net/get-music-content/fixture/800x800")
                        .expect("fixture thumbnail URL"),
                ),
                thumbnail_dimensions: Some((800, 800)),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let terminal_window = TerminalWindowMetrics::new(192, 60, 1_920, 1_200)
            .expect("complete full-HD terminal metrics");
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            ..MockThumbnailRenderer::default()
        };
        let theme = Theme::new(false);

        terminal
            .draw(|frame| {
                render_details_with_terminal_window(
                    frame,
                    Rect::new(96, 0, 96, 58),
                    &view,
                    true,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    Some(terminal_window),
                    &theme,
                    &mut hit_map,
                    Some(&mut thumbnails),
                );
            })
            .expect("draw adaptive Yandex artwork");

        let [(_, requested_area)] = thumbnails.synchronized.as_slice() else {
            panic!("expected one synchronized Yandex artwork request");
        };
        assert!(
            requested_area.height > DEFAULT_THUMBNAIL_HEIGHT,
            "an 800×800 source in a full-HD terminal must render larger than the 400×400 fallback"
        );
    }

    #[test]
    fn configured_thumbnail_height_controls_the_reserved_area() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            details: Some(DetailView {
                title: "Configured thumbnail fixture".to_owned(),
                source: "YouTube".to_owned(),
                description: vec!["Description remains below the image."; 15].join("\n"),
                thumbnail_url: Some(
                    url::Url::parse("https://images.example/configured.jpg")
                        .expect("fixture thumbnail URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let settings = UiSettings {
            thumbnail_height: 7,
            ..UiSettings::default()
        };
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw");

        assert_eq!(thumbnails.synchronized.len(), 1);
        assert_eq!(thumbnails.synchronized[0].1.height, 7);
        assert!(rendered_text(&terminal).contains("Description remains below"));
    }

    #[test]
    fn ready_fitted_artwork_leaves_one_blank_row_before_following_details() {
        let backend = TestBackend::new(120, 44);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            screen: Screen::Local,
            details: Some(DetailView {
                title: "Wide local image".to_owned(),
                source: "Local image".to_owned(),
                description: "Following Details text.".to_owned(),
                thumbnail_url: Some(
                    url::Url::parse("file:///tmp/youta-wide-image.jpg").expect("fixture image URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let settings = UiSettings {
            thumbnail_height: 12,
            ..UiSettings::default()
        };
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            rendered_artwork: true,
            // A 16:9 image fitted into the Local image screen's doubled
            // 60×24-cell box at 10×20 pixels per cell occupies 17 rows.
            prepared_artwork_size: Some(Size::new(60, 17)),
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw fitted local artwork");

        let [(_, requested_area)] = thumbnails.synchronized.as_slice() else {
            panic!("expected one synchronized thumbnail");
        };
        let [rendered_area] = thumbnails.rendered_areas.as_slice() else {
            panic!("expected one rendered thumbnail");
        };
        let details_row = hit_map
            .detail_text_rows
            .iter()
            .find(|row| row.cells.concat().contains("Following Details text."))
            .expect("following Details row");

        assert_eq!(requested_area.height, 24);
        assert_eq!(rendered_area.height, 17);
        assert_eq!(
            rendered_area.y, requested_area.y,
            "the TUI must not insert empty rows before ready artwork"
        );
        assert_eq!(
            details_row.y,
            rendered_area.bottom().saturating_add(1),
            "one deliberate blank row must separate artwork from Details text"
        );
    }

    #[test]
    fn portrait_artwork_uses_the_worker_prepared_width_without_realignment() {
        let available = Rect::new(61, 8, 60, 24);
        let thumbnails = MockThumbnailRenderer {
            prepared_artwork_size: Some(Size::new(27, 24)),
            ..MockThumbnailRenderer::default()
        };

        assert_eq!(
            thumbnails.prepared_artwork_area(available),
            Some(Rect::new(available.x, available.y, 27, 24)),
            "Fit padding previously placed portrait pixels at the area's left edge"
        );
    }

    #[test]
    fn youtube_thumbnails_expand_at_1080_terminal_window_pixels() {
        let description = "x".repeat(63 * SHORT_YOUTUBE_DESCRIPTION_LINE_LIMIT);
        for (height_pixels, expected_height) in [(1079, 7), (1080, 32)] {
            let backend = TestBackend::new(120, 60);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let view = ViewModel {
                details: Some(DetailView {
                    title: "Adaptive YouTube thumbnail".to_owned(),
                    source: "YouTube".to_owned(),
                    description: description.clone(),
                    thumbnail_url: Some(
                        url::Url::parse("https://images.example/adaptive.jpg")
                            .expect("fixture thumbnail URL"),
                    ),
                    ..DetailView::default()
                }),
                ..ViewModel::default()
            };
            let terminal_window = TerminalWindowMetrics::new(120, 60, 1920, height_pixels)
                .expect("complete fixture window metrics");
            let mut hit_map = HitMap::default();
            let mut thumbnails = MockThumbnailRenderer {
                enabled: true,
                ..MockThumbnailRenderer::default()
            };
            let theme = Theme::new(false);
            let details_area = Rect::new(56, 2, 64, 50);

            terminal
                .draw(|frame| {
                    render_details_with_terminal_window(
                        frame,
                        details_area,
                        &view,
                        true,
                        7,
                        Some(terminal_window),
                        &theme,
                        &mut hit_map,
                        Some(&mut thumbnails),
                    );
                })
                .expect("draw adaptive thumbnail");

            let [(_, thumbnail_area)] = thumbnails.synchronized.as_slice() else {
                panic!("expected one synchronized thumbnail");
            };
            assert_eq!(thumbnail_area.width, details_area.width);
            assert_eq!(
                thumbnail_area.height, expected_height,
                "terminal-window threshold {height_pixels}"
            );
        }

        assert_eq!(
            youtube_thumbnail_height(
                7,
                64,
                false,
                true,
                None,
                TerminalWindowMetrics::new(120, 60, 1920, 1080),
            ),
            7,
            "non-YouTube artwork must retain the configured height"
        );
    }

    #[test]
    fn short_youtube_descriptions_expand_thumbnails_below_1080_pixels() {
        let details_area = Rect::new(56, 2, 64, 60);
        let rendered_description_width = usize::from(details_area.width.saturating_sub(1));
        for (source, wrapped_lines, expected_height) in
            [("YouTube", 14, 56), ("YouTube", 15, 7), ("PeerTube", 14, 7)]
        {
            let backend = TestBackend::new(120, 70);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let details = DetailView {
                title: "Short-description thumbnail".to_owned(),
                source: source.to_owned(),
                description: "x".repeat(rendered_description_width * wrapped_lines),
                thumbnail_url: Some(
                    url::Url::parse("https://images.example/short-description.jpg")
                        .expect("fixture thumbnail URL"),
                ),
                ..DetailView::default()
            };
            assert_eq!(
                youtube_description_is_short(&details, details_area.width),
                source == "YouTube" && wrapped_lines < SHORT_YOUTUBE_DESCRIPTION_LINE_LIMIT,
                "{source} description with {wrapped_lines} wrapped lines"
            );
            let view = ViewModel {
                details: Some(details),
                ..ViewModel::default()
            };
            let terminal_window = TerminalWindowMetrics::new(120, 70, 1920, 720)
                .expect("complete fixture window metrics");
            let mut hit_map = HitMap::default();
            let mut thumbnails = MockThumbnailRenderer {
                enabled: true,
                ..MockThumbnailRenderer::default()
            };
            let theme = Theme::new(false);

            terminal
                .draw(|frame| {
                    render_details_with_terminal_window(
                        frame,
                        details_area,
                        &view,
                        true,
                        7,
                        Some(terminal_window),
                        &theme,
                        &mut hit_map,
                        Some(&mut thumbnails),
                    );
                })
                .expect("draw short-description thumbnail");

            let [(_, thumbnail_area)] = thumbnails.synchronized.as_slice() else {
                panic!("expected one synchronized thumbnail");
            };
            assert_eq!(thumbnail_area.width, details_area.width);
            assert_eq!(
                thumbnail_area.height, expected_height,
                "{source} description with {wrapped_lines} wrapped lines"
            );
        }
        assert_eq!(
            youtube_thumbnail_height(7, details_area.width, true, true, None, None),
            18,
            "short descriptions should use the image backend's fallback cell geometry"
        );
        assert_eq!(
            youtube_thumbnail_height(7, details_area.width, true, true, Some((640, 480)), None,),
            24,
            "standard 4:3 artwork must reserve its full width without side gaps"
        );
    }

    #[test]
    fn ready_artwork_click_toggles_expansion_even_during_details_text_selection() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            text_selection_mode: true,
            details: Some(DetailView {
                title: "Clickable artwork fixture".to_owned(),
                source: "YouTube".to_owned(),
                description: "Selectable text below the image.".to_owned(),
                thumbnail_url: Some(
                    url::Url::parse("https://images.example/clickable.jpg")
                        .expect("fixture thumbnail URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            rendered_artwork: true,
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &view,
                    &UiSettings::default(),
                    &mut hit_map,
                    Some(&mut thumbnails),
                );
            })
            .expect("draw ready artwork");

        let thumbnail_area = hit_map.thumbnail_area.expect("ready artwork hitbox");
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: thumbnail_area.x,
            row: thumbnail_area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click, &hit_map, &view),
            Some(UiAction::ToggleThumbnailExpansion),
            "the image action must take precedence over Details focus without disabling text selection"
        );
    }

    #[test]
    fn expanded_artwork_uses_the_full_terminal_and_preserves_aspect_fit() {
        for (width, height) in [(120, 40), (70, 50)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let mut view = ViewModel {
                rows: vec![RowView {
                    title: "Left-panel fixture".to_owned(),
                    ..RowView::default()
                }],
                details: Some(DetailView {
                    title: "Expanded artwork fixture".to_owned(),
                    source: "Local image".to_owned(),
                    description: "Hidden while artwork is expanded.".to_owned(),
                    thumbnail_url: Some(
                        url::Url::parse("file:///tmp/youta-expanded-artwork.jpg")
                            .expect("fixture thumbnail URL"),
                    ),
                    ..DetailView::default()
                }),
                ..ViewModel::default()
            };
            let mut hit_map = HitMap::default();
            view.details
                .as_mut()
                .expect("fixture details")
                .thumbnail_expanded = true;
            let mut expanded = MockThumbnailRenderer {
                enabled: true,
                rendered_artwork: true,
                prepared_artwork_size: Some(Size::new(width / 2, height / 2)),
                ..MockThumbnailRenderer::default()
            };
            terminal
                .draw(|frame| {
                    render_frame(
                        frame,
                        &view,
                        &UiSettings::default(),
                        &mut hit_map,
                        Some(&mut expanded),
                    );
                })
                .expect("draw expanded artwork");
            let expanded_requested_area = expanded.synchronized[0].1;
            let expanded_rendered_area = expanded.rendered_areas[0];

            assert_eq!(expanded_requested_area, Rect::new(0, 0, width, height));
            assert_eq!(expanded_rendered_area.width, width / 2);
            assert_eq!(expanded_rendered_area.height, height / 2);
            assert_eq!(
                expanded_rendered_area,
                centered_sized_rect(width / 2, height / 2, expanded_requested_area)
            );
            assert_eq!(hit_map.thumbnail_area, Some(expanded_rendered_area));
            assert_eq!(
                hit_map.thumbnail_overlay_area,
                Some(Rect::new(0, 0, width, height))
            );
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: 0,
                        row: 0,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                Some(UiAction::ToggleThumbnailExpansion),
                "a second click in the overlay letterbox must restore Details"
            );
            assert!(
                !rendered_text(&terminal).contains("Hidden while artwork is expanded."),
                "the full-terminal overlay must cover following Details text"
            );
            assert!(
                !rendered_text(&terminal).contains("Left-panel fixture"),
                "the full-terminal overlay must cover the media list"
            );
        }
    }

    #[test]
    fn expanded_thumbnail_escape_precedes_details_and_parent_navigation() {
        let view = ViewModel {
            screen: Screen::Local,
            details_focused: true,
            text_selection_mode: true,
            playlist_back_available: true,
            details: Some(DetailView {
                thumbnail_expanded: true,
                thumbnail_url: Some(
                    url::Url::parse("https://images.example/fullscreen.jpg")
                        .expect("fixture thumbnail URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &view),
            Some(UiAction::ToggleThumbnailExpansion)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &view),
            None,
            "the expanded thumbnail behaves as a modal overlay"
        );
    }

    #[test]
    fn expanded_thumbnail_precedes_hidden_search_editing() {
        let view = ViewModel {
            search_editing: true,
            search_query: "hidden query".to_owned(),
            details: Some(DetailView {
                thumbnail_expanded: true,
                thumbnail_url: Some(
                    url::Url::parse("https://images.example/fullscreen.jpg")
                        .expect("fixture thumbnail URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &view),
            Some(UiAction::ToggleThumbnailExpansion)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), &view),
            None,
            "typing must not edit a search field hidden behind the artwork"
        );
    }

    #[test]
    fn stale_thumbnail_expansion_can_close_without_blocking_navigation() {
        let view = ViewModel {
            screen: Screen::Local,
            details: Some(DetailView {
                thumbnail_expanded: true,
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &view),
            Some(UiAction::ToggleThumbnailExpansion)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &view),
            Some(UiAction::MoveSelection(1)),
            "an absent artwork source must not make normal Details modal"
        );
    }

    #[test]
    fn error_popup_obscures_expanded_thumbnail_and_keeps_input_precedence() {
        let mut terminal = Terminal::new(TestBackend::new(100, 32)).expect("terminal");
        let view = ViewModel {
            details: Some(DetailView {
                thumbnail_expanded: true,
                thumbnail_url: Some(
                    url::Url::parse("https://images.example/fullscreen.jpg")
                        .expect("fixture thumbnail URL"),
                ),
                ..DetailView::default()
            }),
            error_popup: Some(ErrorPopupView {
                title: "Fixture error".to_owned(),
                report: "Complete fixture report".to_owned(),
                ..ErrorPopupView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            rendered_artwork: true,
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &view,
                    &UiSettings::default(),
                    &mut hit_map,
                    Some(&mut thumbnails),
                );
            })
            .expect("draw popup over expanded thumbnail");

        assert_eq!(thumbnails.obscure_count, 1);
        assert!(thumbnails.synchronized.is_empty());
        assert!(hit_map.thumbnail_overlay_area.is_none());
        assert!(rendered_text(&terminal).contains("Fixture error"));
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &view),
            Some(UiAction::DismissErrorPopup)
        );
    }

    #[test]
    fn expanded_thumbnail_owns_the_full_hitbox_while_loading() {
        let mut terminal = Terminal::new(TestBackend::new(100, 32)).expect("terminal");
        let view = ViewModel {
            details: Some(DetailView {
                thumbnail_expanded: true,
                thumbnail_url: Some(
                    url::Url::parse("https://images.example/loading.jpg")
                        .expect("fixture thumbnail URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            rendered_artwork: false,
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &view,
                    &UiSettings::default(),
                    &mut hit_map,
                    Some(&mut thumbnails),
                );
            })
            .expect("draw loading expanded thumbnail");

        assert_eq!(thumbnails.synchronized[0].1, Rect::new(0, 0, 100, 32));
        assert_eq!(
            hit_map.thumbnail_overlay_area,
            Some(Rect::new(0, 0, 100, 32))
        );
        assert!(hit_map.thumbnail_area.is_none());
    }

    #[test]
    fn expanded_local_video_synchronizes_its_midpoint_against_the_full_terminal() {
        let mut terminal = Terminal::new(TestBackend::new(100, 32)).expect("terminal");
        let local_video = LocalVideoThumbnailView {
            path: PathBuf::from("/tmp/youta-expanded-video.mov"),
            midpoint_millis: 42_000,
        };
        let view = ViewModel {
            details: Some(DetailView {
                thumbnail_expanded: true,
                local_video_thumbnail: Some(local_video.clone()),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            rendered_artwork: true,
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &view,
                    &UiSettings::default(),
                    &mut hit_map,
                    Some(&mut thumbnails),
                );
            })
            .expect("draw expanded local video");

        assert_eq!(
            thumbnails.synchronized_local_videos,
            [(local_video, Rect::new(0, 0, 100, 32))]
        );
        assert!(thumbnails.synchronized.is_empty());
    }

    #[test]
    fn absent_loading_and_failed_artwork_never_create_an_expansion_hitbox() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            details: Some(DetailView {
                title: "No artwork fixture".to_owned(),
                description: "Details remain focusable.".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &view,
                    &UiSettings::default(),
                    &mut hit_map,
                    Some(&mut thumbnails),
                );
            })
            .expect("draw absent artwork");
        assert!(hit_map.thumbnail_area.is_none());
        assert!(thumbnails.synchronized.is_empty());

        view.details
            .as_mut()
            .expect("fixture details")
            .thumbnail_url = Some(
            url::Url::parse("https://images.example/loading.jpg")
                .expect("loading fixture thumbnail URL"),
        );
        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &view,
                    &UiSettings::default(),
                    &mut hit_map,
                    Some(&mut thumbnails),
                );
            })
            .expect("draw loading or failed placeholder");
        assert!(hit_map.thumbnail_area.is_none());
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: thumbnails.synchronized[0].1.x,
            row: thumbnails.synchronized[0].1.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click, &hit_map, &view),
            Some(UiAction::SetDetailsFocus(true)),
            "a placeholder must retain ordinary Details focus behavior"
        );
    }

    #[test]
    fn configured_thumbnail_height_is_bounded_by_the_available_details_area() {
        let backend = TestBackend::new(120, 26);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            details: Some(DetailView {
                title: "Bounded thumbnail fixture".to_owned(),
                source: "YouTube".to_owned(),
                description: "Reserved description row.".to_owned(),
                thumbnail_url: Some(
                    url::Url::parse("https://images.example/bounded.jpg")
                        .expect("fixture thumbnail URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let settings = UiSettings {
            thumbnail_height: 100,
            ..UiSettings::default()
        };
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw");

        assert_eq!(thumbnails.synchronized.len(), 1);
        let thumbnail_area = thumbnails.synchronized[0].1;
        assert!(thumbnail_area.height < settings.thumbnail_height);
        assert!(
            thumbnail_area.bottom() <= hit_map.details_panel.bottom().saturating_sub(1),
            "thumbnail must leave one description row"
        );
        assert!(rendered_text(&terminal).contains("Reserved description row"));
    }

    #[cfg(feature = "images")]
    #[test]
    fn linux_console_halfblocks_are_quantized_to_named_ansi_colors() {
        let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 3, 1));
        buffer[(0, 0)].fg = Color::Rgb(250, 80, 90);
        buffer[(0, 0)].bg = Color::Rgb(4, 7, 9);
        buffer[(0, 0)].modifier = Modifier::BOLD | Modifier::ITALIC;
        buffer[(1, 0)].fg = Color::Indexed(42);
        buffer[(2, 0)].fg = Color::Rgb(255, 255, 255);
        buffer[(2, 0)].modifier = Modifier::ITALIC;

        quantize_linux_console_thumbnail(&mut buffer, Rect::new(0, 0, 2, 1));

        assert_eq!(buffer[(0, 0)].fg, Color::LightRed);
        assert_eq!(buffer[(0, 0)].bg, Color::Black);
        assert!(buffer[(0, 0)].modifier.is_empty());
        assert_eq!(buffer[(1, 0)].fg, Color::Indexed(42));
        assert_eq!(
            buffer[(2, 0)].fg,
            Color::Rgb(255, 255, 255),
            "cells outside the rendered thumbnail must remain untouched"
        );
        assert!(buffer[(2, 0)].modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn linux_console_frame_tracks_bright_foreground_intensity_as_bold() {
        let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 5, 1));
        buffer[(0, 0)].fg = Color::DarkGray;
        buffer[(1, 0)].fg = Color::White;
        buffer[(2, 0)].fg = Color::Indexed(42);
        buffer[(3, 0)].bg = Color::LightGreen;
        buffer[(3, 0)].modifier = Modifier::DIM | Modifier::ITALIC | Modifier::UNDERLINED;
        buffer[(4, 0)].fg = Color::Red;
        buffer[(4, 0)].modifier = Modifier::BOLD;

        normalize_linux_console_buffer_for_color_output(&mut buffer, true);

        assert_eq!(buffer[(0, 0)].fg, Color::DarkGray);
        assert!(buffer[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(1, 0)].fg, Color::White);
        assert!(buffer[(1, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(2, 0)].fg, Color::Green);
        assert!(!buffer[(2, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(3, 0)].bg, Color::Green);
        assert!(
            !buffer[(3, 0)]
                .modifier
                .intersects(Modifier::DIM | Modifier::ITALIC)
        );
        assert!(buffer[(3, 0)].modifier.contains(Modifier::UNDERLINED));
        assert_eq!(buffer[(4, 0)].fg, Color::LightRed);
        assert!(buffer[(4, 0)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn linux_console_frame_removes_colors_when_crossterm_suppresses_them() {
        let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 2, 1));
        buffer[(0, 0)].fg = Color::DarkGray;
        buffer[(0, 0)].bg = Color::LightBlue;
        buffer[(1, 0)].fg = Color::Rgb(1, 2, 3);

        normalize_linux_console_buffer_for_color_output(&mut buffer, false);

        assert_eq!(buffer[(0, 0)].fg, Color::Reset);
        assert_eq!(buffer[(0, 0)].bg, Color::Reset);
        assert!(buffer[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(1, 0)].fg, Color::Reset);
    }

    #[test]
    fn linux_console_no_color_backend_keeps_intensity_transitions_tracked() {
        use ratatui::backend::Backend;

        let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 2, 1));
        buffer[(0, 0)].set_symbol("L").set_fg(Color::DarkGray);
        buffer[(1, 0)].set_symbol("о");
        normalize_linux_console_buffer_for_color_output(&mut buffer, false);

        let output = CapturedWriter::default();
        let mut backend = CrosstermBackend::new(output.clone());
        backend
            .draw([(0, 0, &buffer[(0, 0)]), (1, 0, &buffer[(1, 0)])].into_iter())
            .expect("write color-suppressed console cells");
        let bytes = output
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let label = bytes
            .iter()
            .position(|byte| *byte == b'L')
            .expect("bright label must be emitted");
        let normal_intensity = bytes
            .windows(b"\x1b[22m".len())
            .position(|window| window == b"\x1b[22m")
            .expect("plain text must reset label intensity");
        let cyrillic = bytes
            .windows("о".len())
            .position(|window| window == "о".as_bytes())
            .expect("plain Cyrillic cell must be emitted");

        assert!(
            !bytes
                .windows(b"\x1b[;m".len())
                .any(|window| window == b"\x1b[;m"),
            "suppressed colors must not serialize as an untracked SGR reset"
        );
        assert!(label < normal_intensity && normal_intensity < cyrillic);
    }

    #[test]
    fn linux_console_backend_resets_intensity_before_plain_cyrillic() {
        use ratatui::backend::Backend;

        const CHILD_MARKER: &str = "YOUTA_LINUX_CONSOLE_COLOR_TEST_CHILD";
        if std::env::var_os(CHILD_MARKER).is_none() {
            let status = std::process::Command::new(
                std::env::current_exe().expect("locate current test executable"),
            )
            .args([
                "--exact",
                "tui::tests::linux_console_backend_resets_intensity_before_plain_cyrillic",
            ])
            .env(CHILD_MARKER, "1")
            .env_remove("NO_COLOR")
            .status()
            .expect("run isolated Crossterm color regression");
            assert!(status.success(), "isolated color regression must pass");
            return;
        }

        let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 2, 1));
        buffer[(0, 0)].set_symbol("L").set_fg(Color::DarkGray);
        buffer[(1, 0)].set_symbol("о");
        normalize_linux_console_buffer_for_color_output(&mut buffer, true);

        let output = CapturedWriter::default();
        let mut backend = CrosstermBackend::new(output.clone());
        backend
            .draw([(0, 0, &buffer[(0, 0)]), (1, 0, &buffer[(1, 0)])].into_iter())
            .expect("write normalized console cells");
        let bytes = output
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bright_gray = bytes
            .windows(b"38;5;8".len())
            .position(|window| window == b"38;5;8")
            .expect("bright gray must be emitted as an explicit indexed color");
        let label = bytes
            .iter()
            .position(|byte| *byte == b'L')
            .expect("bright label must be emitted");
        let normal_intensity = bytes
            .windows(b"\x1b[22m".len())
            .position(|window| window == b"\x1b[22m")
            .expect("plain text must reset the bright label's intensity");
        let cyrillic = bytes
            .windows("о".len())
            .position(|window| window == "о".as_bytes())
            .expect("plain Cyrillic cell must be emitted");

        assert!(
            bright_gray < label,
            "explicit bright gray must precede the bright label"
        );
        assert!(
            normal_intensity < cyrillic,
            "normal intensity must precede the plain Cyrillic glyph"
        );
    }

    #[cfg(feature = "images")]
    #[test]
    fn repeated_tui_frames_replace_loading_with_the_real_thumbnail_protocol() {
        use std::collections::BTreeSet;
        use std::time::{Duration, Instant};

        use crate::thumbnails::{ThumbnailFailure, ThumbnailState, tests as thumbnail_tests};

        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let thumbnail_url =
            url::Url::parse("https://images.example/fixture.png").expect("fixture thumbnail URL");
        let mut view = ViewModel {
            details: Some(DetailView {
                title: "Asynchronous thumbnail fixture".to_owned(),
                source: "YouTube".to_owned(),
                description: "Description remains visible after the image loads.".to_owned(),
                thumbnail_url: Some(thumbnail_url.clone()),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let settings = UiSettings::default();
        let mut hit_map = HitMap::default();
        let (manager, replies, observed) = thumbnail_tests::manager_with_mock_transport();
        let mut thumbnails = TerminalThumbnailRenderer::new(manager);

        // Match the production loop ordering: poll any completed work, then
        // synchronize and render the selected target during the frame.
        thumbnails.poll();
        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw loading frame");
        assert_eq!(thumbnails.manager.state(), &ThumbnailState::Loading);
        assert!(rendered_text(&terminal).contains("Loading thumbnail…"));
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("worker must receive the TUI-selected thumbnail"),
            thumbnail_url
        );

        replies
            .send(Ok(thumbnail_tests::fixture_thumbnail_png()))
            .expect("release successful mock thumbnail");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            thumbnails.poll();
            terminal
                .draw(|frame| {
                    render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
                })
                .expect("draw subsequent thumbnail frame");
            if thumbnails.manager.state() != &ThumbnailState::Loading {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "repeated TUI frames left the completed thumbnail in Loading"
            );
            std::thread::yield_now();
        }

        assert_eq!(thumbnails.manager.state(), &ThumbnailState::Ready);
        let rendered = rendered_text(&terminal);
        assert!(
            !rendered.contains("Loading thumbnail…"),
            "the ready image must replace the loading label"
        );
        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw protocol frame after the clearing transition");
        let rendered = rendered_text(&terminal);
        assert!(
            rendered.contains('\u{10EEEE}') || rendered.contains("\u{1b}_G"),
            "the real ratatui-image protocol must occupy the reserved area"
        );
        let buffer = terminal.backend().buffer();
        let placeholder_rows = buffer
            .content()
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.symbol().contains('\u{10EEEE}'))
            .map(|(index, _)| buffer.pos_of(index).1)
            .collect::<BTreeSet<_>>();
        assert!(
            placeholder_rows.len() > 1,
            "Kitty protocol rows were collapsed by terminal-buffer diffing: {placeholder_rows:?}"
        );
        assert!(rendered.contains("Description remains visible"));

        let failed_url = url::Url::parse("https://images.example/failed.png")
            .expect("failed fixture thumbnail URL");
        view.details
            .as_mut()
            .expect("fixture details")
            .thumbnail_url = Some(failed_url.clone());
        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw replacement loading frame");
        assert_eq!(thumbnails.manager.state(), &ThumbnailState::Loading);
        assert!(rendered_text(&terminal).contains("Loading thumbnail…"));
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("worker must receive the replacement thumbnail"),
            failed_url
        );
        replies
            .send(Err(ThumbnailFailure::DownloadFailed))
            .expect("release failed mock thumbnail");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            thumbnails.poll();
            terminal
                .draw(|frame| {
                    render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
                })
                .expect("draw failed thumbnail frame");
            if thumbnails.manager.state() != &ThumbnailState::Loading {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "replacement thumbnail remained Loading after a failed response"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            thumbnails.manager.state(),
            &ThumbnailState::Failed(ThumbnailFailure::DownloadFailed)
        );
        let rendered = rendered_text(&terminal);
        assert!(!rendered.contains("Loading thumbnail…"));
        assert!(rendered.contains("Thumbnail unavailable: thumbnail download failed"));
    }

    #[cfg(feature = "images")]
    #[test]
    fn first_ready_expanded_thumbnail_frame_is_never_a_blank_modal() {
        use std::time::{Duration, Instant};

        use crate::thumbnails::{ThumbnailState, tests as thumbnail_tests};

        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let thumbnail_url = url::Url::parse("https://images.example/fixture-640.png")
            .expect("fixture thumbnail URL");
        let expanded_thumbnail_url = url::Url::parse("https://images.example/fixture-maxres.png")
            .expect("expanded fixture thumbnail URL");
        let mut view = ViewModel {
            details: Some(DetailView {
                title: "Expanded thumbnail fixture".to_owned(),
                source: "YouTube".to_owned(),
                description: "Useful details remain visible.".to_owned(),
                thumbnail_url: Some(thumbnail_url.clone()),
                expanded_thumbnail_url: Some(expanded_thumbnail_url.clone()),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let settings = UiSettings::default();
        let mut hit_map = HitMap::default();
        let (manager, replies, observed) = thumbnail_tests::manager_with_mock_transport();
        let mut thumbnails = TerminalThumbnailRenderer::new(manager);

        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("start the selected thumbnail request");
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("worker must receive the selected thumbnail"),
            thumbnail_url
        );
        replies
            .send(Ok(thumbnail_tests::fixture_thumbnail_png()))
            .expect("release selected mock thumbnail");
        let deadline = Instant::now() + Duration::from_secs(2);
        while thumbnails.manager.state() == &ThumbnailState::Loading {
            thumbnails.poll();
            assert!(
                Instant::now() < deadline,
                "selected thumbnail remained Loading after its mock response"
            );
            std::thread::yield_now();
        }
        for _ in 0..2 {
            terminal
                .draw(|frame| {
                    render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
                })
                .expect("stabilize selected thumbnail protocol");
        }

        view.details
            .as_mut()
            .expect("fixture details")
            .thumbnail_expanded = true;
        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw expanded thumbnail loading frame");
        assert_eq!(thumbnails.manager.state(), &ThumbnailState::Loading);
        assert!(
            rendered_text(&terminal).contains("Useful details remain visible."),
            "loading enlarged artwork must retain useful screen content"
        );
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("worker must receive the enlarged thumbnail"),
            expanded_thumbnail_url,
            "clicking the selected thumbnail must request its largest advertised image"
        );

        replies
            .send(Ok(thumbnail_tests::fixture_thumbnail_png()))
            .expect("release successful mock thumbnail");
        let deadline = Instant::now() + Duration::from_secs(2);
        while thumbnails.manager.state() == &ThumbnailState::Loading {
            thumbnails.poll();
            assert!(
                Instant::now() < deadline,
                "expanded thumbnail remained Loading after its mock response"
            );
            std::thread::yield_now();
        }
        assert_eq!(thumbnails.manager.state(), &ThumbnailState::Ready);

        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw first ready expanded thumbnail frame");
        let rendered = rendered_text(&terminal);
        assert!(
            rendered.contains("Useful details remain visible."),
            "the terminal-image clearing transition must retain useful content instead of a blank modal"
        );
        assert!(
            thumbnails.needs_immediate_redraw(),
            "the clearing transition must schedule its protocol frame without another user click"
        );

        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw enlarged terminal-image protocol frame");
        let rendered = rendered_text(&terminal);
        assert!(
            rendered.contains('\u{10EEEE}') || rendered.contains("\u{1b}_G"),
            "the immediate follow-up frame must contain the enlarged terminal image"
        );
    }

    #[cfg(feature = "images")]
    #[test]
    fn thumbnail_finishing_behind_note_popup_resumes_without_a_second_request() {
        use std::time::{Duration, Instant};

        use crossbeam_channel::TryRecvError;

        use crate::thumbnails::{ThumbnailState, tests as thumbnail_tests};

        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let thumbnail_url =
            url::Url::parse("https://images.example/note-cache.png").expect("thumbnail URL");
        let mut view = ViewModel {
            details: Some(DetailView {
                title: "Private-note thumbnail fixture".to_owned(),
                source: "YouTube".to_owned(),
                description: "The image must resume after the note closes.".to_owned(),
                thumbnail_url: Some(thumbnail_url.clone()),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let settings = UiSettings::default();
        let mut hit_map = HitMap::default();
        let (manager, replies, observed) = thumbnail_tests::manager_with_mock_transport();
        let mut thumbnails = TerminalThumbnailRenderer::new(manager);

        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("start thumbnail request");
        assert_eq!(thumbnails.manager.state(), &ThumbnailState::Loading);
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("initial worker request"),
            thumbnail_url
        );

        view.private_note_popup = Some(PrivateNotePopupView {
            target_label: "Private-note thumbnail fixture".to_owned(),
            storage_path: "/tmp/youta/state/notes.toml".to_owned(),
            ..PrivateNotePopupView::default()
        });
        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("obscure in-flight thumbnail");
        assert_eq!(thumbnails.manager.state(), &ThumbnailState::Loading);

        replies
            .send(Ok(thumbnail_tests::fixture_thumbnail_png()))
            .expect("finish thumbnail behind popup");
        let deadline = Instant::now() + Duration::from_secs(2);
        while thumbnails.manager.state() == &ThumbnailState::Loading {
            thumbnails.poll();
            assert!(
                Instant::now() < deadline,
                "thumbnail remained Loading behind the note popup"
            );
            std::thread::yield_now();
        }
        assert_eq!(thumbnails.manager.state(), &ThumbnailState::Ready);

        view.private_note_popup = None;
        for _ in 0..2 {
            terminal
                .draw(|frame| {
                    render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
                })
                .expect("resume thumbnail after note popup");
        }
        assert!(!rendered_text(&terminal).contains("Loading thumbnail…"));
        assert_eq!(
            observed.try_recv(),
            Err(TryRecvError::Empty),
            "closing a note must not start another thumbnail worker"
        );
    }

    #[cfg(feature = "images")]
    #[test]
    fn revisited_subscription_artwork_is_ready_without_another_worker_request() {
        use std::time::{Duration, Instant};

        use crossbeam_channel::TryRecvError;

        use crate::thumbnails::{ThumbnailState, tests as thumbnail_tests};

        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let first =
            url::Url::parse("https://yt3.ggpht.com/tui-first=s800").expect("first artwork URL");
        let second =
            url::Url::parse("https://yt3.ggpht.com/tui-second=s800").expect("second artwork URL");
        let mut view = ViewModel {
            screen: Screen::Subscriptions,
            details: Some(DetailView {
                title: "First subscribed channel".to_owned(),
                source: "YouTube".to_owned(),
                description: "Cached channel description".to_owned(),
                thumbnail_url: Some(first.clone()),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        view.subscriptions.route = SubscriptionRoute::Items;
        view.subscriptions.source_title = "Subscribed channel".to_owned();
        let settings = UiSettings::default();
        let mut hit_map = HitMap::default();
        let (manager, replies, observed) = thumbnail_tests::manager_with_mock_transport();
        let mut thumbnails = TerminalThumbnailRenderer::new(manager);

        for source in [&first, &second] {
            view.details
                .as_mut()
                .expect("subscription channel details")
                .thumbnail_url = Some(source.clone());
            terminal
                .draw(|frame| {
                    render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
                })
                .expect("draw cold subscription artwork");
            assert_eq!(thumbnails.manager.state(), &ThumbnailState::Loading);
            assert!(rendered_text(&terminal).contains("Loading thumbnail…"));
            assert_eq!(
                observed
                    .recv_timeout(Duration::from_secs(1))
                    .expect("cold artwork worker request"),
                *source
            );
            replies
                .send(Ok(thumbnail_tests::fixture_thumbnail_png()))
                .expect("release cold artwork response");
            let deadline = Instant::now() + Duration::from_secs(2);
            while thumbnails.manager.state() == &ThumbnailState::Loading {
                thumbnails.poll();
                assert!(
                    Instant::now() < deadline,
                    "subscription artwork remained Loading after its mock response"
                );
                std::thread::yield_now();
            }
            assert_eq!(thumbnails.manager.state(), &ThumbnailState::Ready);
        }

        view.details
            .as_mut()
            .expect("subscription channel details")
            .thumbnail_url = Some(first);
        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("redraw revisited subscription artwork");

        assert_eq!(
            thumbnails.manager.state(),
            &ThumbnailState::Ready,
            "the A→B→A transition must restore A synchronously"
        );
        assert!(
            !rendered_text(&terminal).contains("Loading thumbnail…"),
            "revisited prepared artwork must not expose a loading placeholder"
        );
        assert_eq!(
            observed.try_recv(),
            Err(TryRecvError::Empty),
            "the A→B→A transition must not emit another worker/network request"
        );
    }

    #[test]
    fn thumbnail_is_not_requested_when_the_details_panel_is_too_short() {
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            details: Some(DetailView {
                title: "Small panel fixture".to_owned(),
                source: "YouTube".to_owned(),
                description: "Text takes priority.".to_owned(),
                thumbnail_url: Some(
                    url::Url::parse("https://images.example/small.jpg")
                        .expect("fixture thumbnail URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &view,
                    &UiSettings::default(),
                    &mut hit_map,
                    Some(&mut thumbnails),
                );
            })
            .expect("draw");

        assert!(thumbnails.synchronized.is_empty());
        assert!(thumbnails.clear_count > 0);
    }

    #[test]
    fn bottom_controls_hide_hotkey_values_without_hiding_action_labels() {
        let backend = TestBackend::new(240, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let settings = UiSettings {
            show_hotkeys: false,
            ..UiSettings::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_buttons(
                    frame,
                    frame.area(),
                    &settings,
                    &Theme::new(false),
                    Screen::Search,
                    YouTubeSearchSort::Relevance,
                    RadioSort::Name,
                    false,
                    true,
                    false,
                    None,
                    false,
                    false,
                    Some(LocalSizeSort::Off),
                    "",
                    false,
                    &mut hit_map,
                );
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("Move up"));
        assert!(rendered.contains("Move down"));
        assert!(rendered.contains("Volume up"));
        assert!(rendered.contains("Volume down"));
        assert!(!rendered.contains("Pause"));
        assert!(!rendered.contains("Start"));
        assert!(rendered.contains("Search"));
        assert!(rendered.contains("Preferences"));
        assert!(rendered.contains("Autoplay: off"));
        assert!(rendered.contains("Help"));
        assert!(!rendered.contains("Sort: relevance"));
        for hidden in [
            "[/]", "[A]", "[N]", "[C]", "[p]", "[k]", "[j]", "[↑]", "[↓]", "[Enter]", "[T]",
            "[Tab]", "[M]",
        ] {
            assert!(!rendered.contains(hidden));
        }
        assert_minimal_footer_actions(&hit_map);
        assert!(hit_map.buttons.iter().all(|(action, _)| {
            !matches!(action, UiAction::TogglePause | UiAction::ActivateSelection)
        }));
    }

    #[test]
    fn bottom_controls_do_not_duplicate_subscription_refresh() {
        let backend = TestBackend::new(240, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_buttons(
                    frame,
                    frame.area(),
                    &UiSettings::default(),
                    &Theme::new(false),
                    Screen::Subscriptions,
                    YouTubeSearchSort::Relevance,
                    RadioSort::Name,
                    false,
                    true,
                    false,
                    None,
                    false,
                    false,
                    None,
                    "",
                    false,
                    &mut hit_map,
                );
            })
            .expect("draw subscription footer");

        assert!(!rendered_text(&terminal).contains("[R] Refresh"));
        assert!(
            hit_map
                .buttons
                .iter()
                .all(|(action, _)| action != &UiAction::RefreshSubscriptionVideos),
            "the footer must not duplicate the refresh action shown beside the subscription list"
        );
    }

    #[test]
    fn transient_openrc_notice_replaces_then_restores_the_one_line_footer() {
        let backend = TestBackend::new(40, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        let notice = "Run rc-service gpm start. GPM mouse unavailable; F8 pointer remains active.";

        terminal
            .draw(|frame| {
                render_buttons(
                    frame,
                    frame.area(),
                    &UiSettings::default(),
                    &Theme::new(false),
                    Screen::Search,
                    YouTubeSearchSort::Relevance,
                    RadioSort::Name,
                    false,
                    true,
                    false,
                    None,
                    false,
                    false,
                    Some(LocalSizeSort::Off),
                    notice,
                    false,
                    &mut hit_map,
                );
            })
            .expect("draw GPM notice");

        assert!(
            rendered_text(&terminal).contains("Run rc-service gpm start"),
            "the actionable command must survive narrow footer clipping"
        );
        assert!(
            hit_map.buttons.is_empty(),
            "replaced controls must not leave invisible click targets"
        );

        terminal
            .draw(|frame| {
                render_buttons(
                    frame,
                    frame.area(),
                    &UiSettings::default(),
                    &Theme::new(false),
                    Screen::Search,
                    YouTubeSearchSort::Relevance,
                    RadioSort::Name,
                    false,
                    true,
                    false,
                    None,
                    false,
                    false,
                    Some(LocalSizeSort::Off),
                    "",
                    false,
                    &mut hit_map,
                );
            })
            .expect("restore footer controls");

        let restored = rendered_text(&terminal);
        assert!(!restored.contains("rc-service"));
        assert!(restored.contains("[/] Search"));
        assert!(!hit_map.buttons.is_empty());
    }

    #[test]
    fn transient_footer_notice_never_inherits_playback_start_animation() {
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        let view = ViewModel {
            playback_starting: true,
            playback_start_animation_frame: 0,
            transient_footer_notice: Some("Run rc-service gpm start.".to_owned()),
            ..ViewModel::default()
        };

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw direct footer notice");

        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let footer_y = area.bottom().saturating_sub(1);
        let footer = (area.x..area.right())
            .map(|x| buffer[(x, footer_y)].symbol())
            .collect::<String>();
        assert!(footer.starts_with("Run rc-service gpm start."));
        assert!(!footer.starts_with("| "));
        assert!(hit_map.buttons.is_empty());
    }

    #[test]
    fn one_line_bottom_controls_keep_navigation_click_targets_aligned() {
        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_buttons(
                    frame,
                    frame.area(),
                    &UiSettings::default(),
                    &Theme::new(false),
                    Screen::Search,
                    YouTubeSearchSort::Relevance,
                    RadioSort::Name,
                    false,
                    true,
                    false,
                    None,
                    false,
                    false,
                    Some(LocalSizeSort::Off),
                    "",
                    false,
                    &mut hit_map,
                );
            })
            .expect("draw");

        assert_minimal_footer_actions(&hit_map);
        let rendered = rendered_text(&terminal);
        for label in [
            "[/] Search",
            "[A] off",
            "[k] Up",
            "[j] Down",
            "[↑] +",
            "[↓] -",
            "[p] Prefs",
            "[?] Help",
        ] {
            assert!(rendered.contains(label), "missing compact label {label}");
        }
        assert!(!rendered.contains("[Tab]"));
        assert!(
            hit_map
                .buttons
                .iter()
                .all(|(_, target)| target.y == terminal.backend().buffer().area.y)
        );
        assert!(hit_map.buttons.iter().all(|(_, target)| {
            target.x >= terminal.backend().buffer().area.x
                && target.right() <= terminal.backend().buffer().area.right()
        }));
        assert!(hit_map.buttons.iter().all(|(action, _)| {
            !matches!(action, UiAction::TogglePause | UiAction::ActivateSelection)
        }));
    }

    #[test]
    fn playlist_back_remains_keyboard_accessible_without_footer_duplication() {
        let backend = TestBackend::new(320, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        let view = ViewModel {
            screen: Screen::Playlists,
            playlist_back_available: true,
            ..ViewModel::default()
        };

        terminal
            .draw(|frame| {
                render_buttons(
                    frame,
                    frame.area(),
                    &UiSettings::default(),
                    &Theme::new(false),
                    Screen::Playlists,
                    YouTubeSearchSort::Relevance,
                    RadioSort::Name,
                    false,
                    true,
                    false,
                    None,
                    false,
                    true,
                    None,
                    "",
                    false,
                    &mut hit_map,
                );
            })
            .expect("draw playlist-entry controls");

        assert!(!rendered_text(&terminal).contains("Back to playlists"));
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &view),
            Some(UiAction::GoBack)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &view),
            Some(UiAction::GoBack)
        );
        assert!(
            hit_map
                .buttons
                .iter()
                .all(|(action, _)| action != &UiAction::GoBack)
        );
        assert_minimal_footer_actions(&hit_map);
    }

    #[test]
    fn playlist_escape_preserves_transient_context_precedence() {
        let view = ViewModel {
            screen: Screen::Playlists,
            playlist_back_available: true,
            ..ViewModel::default()
        };

        let mut details_focused = view.clone();
        details_focused.details_focused = true;
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                &details_focused
            ),
            Some(UiAction::SetDetailsFocus(false))
        );

        let mut search_editing = view.clone();
        search_editing.search_editing = true;
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                &search_editing
            ),
            Some(UiAction::CancelSearch)
        );

        let mut help_open = view.clone();
        help_open.help_open = true;
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &help_open),
            Some(UiAction::ToggleHelp)
        );

        let mut popup_open = view;
        popup_open.playlist_popup = Some(PlaylistPopupView::default());
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &popup_open),
            Some(UiAction::DismissPlaylistPopup)
        );
    }

    #[test]
    fn playlist_actions_remain_keyboard_accessible_without_footer_duplication() {
        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        let playlist_item = PlaylistItemView {
            media_id: MediaId::new(SourceKind::YouTube, "playable"),
            title: "Playable".to_owned(),
            in_todo: false,
        };
        let view = ViewModel {
            playlist_item: Some(playlist_item.clone()),
            ..ViewModel::default()
        };

        terminal
            .draw(|frame| {
                render_buttons(
                    frame,
                    frame.area(),
                    &UiSettings::default(),
                    &Theme::new(false),
                    Screen::Search,
                    YouTubeSearchSort::Relevance,
                    RadioSort::Name,
                    false,
                    true,
                    false,
                    Some(&playlist_item),
                    false,
                    false,
                    None,
                    "",
                    false,
                    &mut hit_map,
                );
            })
            .expect("draw playable controls");

        let rendered = rendered_text(&terminal);
        assert!(!rendered.contains("[l] Add to todo"));
        assert!(!rendered.contains("[P] Playlist…"));
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleTodoPlaylist)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT),
                &view
            ),
            Some(UiAction::OpenPlaylistPopup)
        );
        assert!(hit_map.buttons.iter().all(|(action, _)| !matches!(
            action,
            UiAction::ToggleTodoPlaylist | UiAction::OpenPlaylistPopup
        )));
        assert_minimal_footer_actions(&hit_map);
    }

    #[test]
    fn hidden_hotkeys_keep_only_minimal_footer_action_labels() {
        let backend = TestBackend::new(240, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        let playlist_item = PlaylistItemView {
            media_id: MediaId::new(SourceKind::YouTube, "playable"),
            title: "Playable".to_owned(),
            in_todo: true,
        };
        terminal
            .draw(|frame| {
                render_buttons(
                    frame,
                    frame.area(),
                    &UiSettings {
                        show_hotkeys: false,
                        ..UiSettings::default()
                    },
                    &Theme::new(false),
                    Screen::Search,
                    YouTubeSearchSort::Relevance,
                    RadioSort::Name,
                    false,
                    true,
                    false,
                    Some(&playlist_item),
                    false,
                    false,
                    None,
                    "",
                    false,
                    &mut hit_map,
                );
            })
            .expect("draw controls without hotkey values");

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Search"));
        assert!(rendered.contains("Autoplay: off"));
        assert!(rendered.contains("Preferences"));
        assert!(rendered.contains("Help"));
        assert!(!rendered.contains("Remove from todo"));
        assert!(!rendered.contains("Playlist…"));
        assert!(!rendered.contains("[l]"));
        assert!(!rendered.contains("[P]"));
        assert!(hit_map.buttons.iter().all(|(action, _)| !matches!(
            action,
            UiAction::ToggleTodoPlaylist | UiAction::OpenPlaylistPopup
        )));
        assert_minimal_footer_actions(&hit_map);
    }

    #[test]
    fn eighty_column_search_footer_keeps_autoplay_visible_and_clickable() {
        let backend = TestBackend::new(80, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        let view = ViewModel::default();

        terminal
            .draw(|frame| {
                render_buttons(
                    frame,
                    frame.area(),
                    &UiSettings::default(),
                    &Theme::new(false),
                    Screen::Search,
                    YouTubeSearchSort::Relevance,
                    RadioSort::Name,
                    false,
                    true,
                    false,
                    None,
                    false,
                    false,
                    None,
                    "",
                    false,
                    &mut hit_map,
                );
            })
            .expect("draw compact Search controls");

        let rendered = rendered_text(&terminal);
        for label in [
            "[/] Search",
            "[A] off",
            "[k] Up",
            "[j] Down",
            "[↑] +",
            "[↓] -",
            "[p] Prefs",
            "[?] Help",
        ] {
            assert!(
                rendered.contains(label),
                "missing compact Search label {label}"
            );
        }
        for expected in [
            UiAction::BeginSearch,
            UiAction::ToggleAutoplay,
            UiAction::MoveSelection(-1),
            UiAction::MoveSelection(1),
            UiAction::ChangeVolume(5),
            UiAction::ChangeVolume(-5),
            UiAction::OpenPreferences,
            UiAction::ToggleHelp,
        ] {
            assert!(
                hit_map
                    .buttons
                    .iter()
                    .any(|(action, target)| action == &expected && target.width > 0),
                "missing visible compact Search action {expected:?}"
            );
        }
        assert_minimal_footer_actions(&hit_map);
        assert!(!rendered.contains("[Tab]"));
        assert!(!rendered.contains("[C]"));
        assert!(!rendered.contains("[S]"));
        assert!(!rendered.contains("[Space] Pause"));
        assert!(!rendered.contains("[Enter] Start"));
        assert!(hit_map.buttons.iter().all(|(action, _)| {
            !matches!(action, UiAction::TogglePause | UiAction::ActivateSelection)
        }));
        let (_, target) = hit_map
            .buttons
            .iter()
            .find(|(action, _)| action == &UiAction::ToggleAutoplay)
            .expect("visible Autoplay click target");
        assert!(target.width > 0);
        assert!(target.right() <= terminal.backend().buffer().area.right());
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: target.x,
                    row: target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ToggleAutoplay)
        );
    }

    #[test]
    fn uppercase_a_toggles_autoplay_without_replacing_lowercase_queue_action() {
        let view = ViewModel::default();

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleAutoplay)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT),
                &view
            ),
            Some(UiAction::ToggleAutoplay)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &view),
            Some(UiAction::AddToQueue)
        );
    }

    #[test]
    fn local_size_sort_is_keyboard_only_and_disabled_with_folder_sizes() {
        let backend = TestBackend::new(240, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        let view = ViewModel {
            screen: Screen::Local,
            local_size_sort: LocalSizeSort::Descending,
            local_folder_sizes_enabled: true,
            ..ViewModel::default()
        };

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw Local controls");
        assert!(!rendered_text(&terminal).contains("[Z] Size sort:"));
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleLocalSizeSort)
        );
        assert!(
            hit_map
                .buttons
                .iter()
                .all(|(action, _)| action != &UiAction::ToggleLocalSizeSort)
        );
        assert_eq!(LocalSizeSort::Off.next(), LocalSizeSort::Ascending);
        assert_eq!(LocalSizeSort::Ascending.next(), LocalSizeSort::Descending);
        assert_eq!(LocalSizeSort::Descending.next(), LocalSizeSort::Off);

        let disabled = ViewModel {
            local_folder_sizes_enabled: false,
            ..view
        };
        let mut disabled_hit_map = HitMap::default();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    &disabled,
                    &UiSettings::default(),
                    &mut disabled_hit_map,
                );
            })
            .expect("draw disabled Local controls");
        assert!(!rendered_text(&terminal).contains("Size sort:"));
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::NONE),
                &disabled,
            ),
            None
        );
        assert!(
            disabled_hit_map
                .buttons
                .iter()
                .all(|(action, _)| action != &UiAction::ToggleLocalSizeSort)
        );
    }

    #[cfg(feature = "youtube-music")]
    #[test]
    fn youtube_music_bottom_controls_omit_youtube_video_filters() {
        let backend = TestBackend::new(240, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| {
                render_buttons(
                    frame,
                    frame.area(),
                    &UiSettings::default(),
                    &Theme::new(false),
                    Screen::YouTubeMusic,
                    YouTubeSearchSort::Newest,
                    RadioSort::Name,
                    true,
                    true,
                    false,
                    None,
                    false,
                    false,
                    Some(LocalSizeSort::Off),
                    "",
                    false,
                    &mut hit_map,
                );
            })
            .expect("draw Music controls");
        let rendered = rendered_text(&terminal);

        assert!(!rendered.contains("CC only"));
        assert!(!rendered.contains("Sort:"));
        assert!(!rendered.contains("Videos/channels"));
        assert!(rendered.contains("[/] Search"));
        assert!(!rendered.contains("[d] Download"));
        assert_minimal_footer_actions(&hit_map);
    }

    #[test]
    fn seek_bar_has_no_separator_border_above_its_status() {
        let backend = TestBackend::new(80, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel::default();
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("0:00 / --:--"));
        assert!(!rendered.contains("────────"));
        assert_eq!(hit_map.seek_bar, Rect::new(0, 0, 80, 1));
        let status_row = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
            .collect::<String>();
        assert!(
            status_row.contains("0:00 / --:--"),
            "playback status must render below, rather than over, the seek track"
        );
    }

    #[test]
    fn live_seek_status_hides_backend_timeline_and_has_no_seek_target() {
        let backend = TestBackend::new(100, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            playback: PlaybackStatus {
                idle: false,
                live: true,
                position: Duration::from_secs(8_641_222),
                duration: Some(Duration::from_secs(8_641_235)),
                paused: false,
                volume: 80,
                speed: 1.0,
                title: Some("Fixture live station".to_owned()),
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };
        let mut hit_map = HitMap {
            seek_bar: Rect::new(0, 0, 100, 1),
            seek_markers: vec![(UiAction::SeekPercent(50.0), Rect::new(50, 0, 1, 1))],
            ..HitMap::default()
        };

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw live status");

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("LIVE"));
        assert!(rendered.contains("Fixture live station"));
        assert!(!rendered.contains("2400:20:22"));
        assert!(!rendered.contains("2400:20:35"));
        assert_eq!(hit_map.seek_bar, Rect::default());
        assert!(hit_map.seek_markers.is_empty());
        assert!(
            hit_map.now_playing.is_some(),
            "the live title must remain a navigation target"
        );
    }

    #[test]
    fn live_cached_range_restores_a_bounded_clickable_seek_bar() {
        let backend = TestBackend::new(160, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            playback: PlaybackStatus {
                idle: false,
                live: true,
                live_seekable_range: Some(crate::playback::BufferedRange {
                    start: Duration::from_secs(8_641_000),
                    end: Duration::from_secs(8_641_235),
                }),
                position: Duration::from_secs(222),
                duration: Some(Duration::from_secs(235)),
                paused: false,
                volume: 80,
                speed: 1.0,
                title: Some("Fixture live station".to_owned()),
                buffered_ranges: vec![crate::playback::BufferedRange {
                    start: Duration::ZERO,
                    end: Duration::from_secs(235),
                }],
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw cached live seek bar");

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("LIVE −0:13 / 3:55 buffer"));
        assert!(rendered.contains("Fixture live station"));
        assert!(!rendered.contains("2400:20"));
        assert_eq!(hit_map.seek_bar, Rect::new(0, 0, 160, 1));
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 80,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SeekPercent(80.0 / 159.0 * 100.0))
        );
    }

    #[test]
    fn chapter_label_height_adapts_to_density_mode_and_terminal_space() {
        let chapters = (0..29)
            .map(|index| Chapter {
                title: format!("Chapter {index}"),
                start_seconds: index * 10,
                end_seconds: None,
            })
            .collect();
        let mut view = ViewModel {
            playback: PlaybackStatus {
                duration: Some(Duration::from_secs(1_000)),
                ..PlaybackStatus::default()
            },
            playback_chapters: chapters,
            show_chapter_timestamps: true,
            ..ViewModel::default()
        };

        assert_eq!(chapter_label_row_count(&view, 240, 32, false), 4);
        assert_eq!(chapter_label_row_count(&view, 80, 32, false), 4);
        assert_eq!(chapter_label_row_count(&view, 240, 15, false), 1);
        assert_eq!(chapter_label_row_count(&view, 240, 14, false), 0);
        assert_eq!(chapter_label_row_count(&view, 240, 16, true), 0);
        view.show_chapter_timestamps = false;
        assert_eq!(chapter_label_row_count(&view, 240, 32, false), 4);
        view.playback.duration = None;
        assert_eq!(chapter_label_row_count(&view, 240, 32, false), 1);
        view.playback_chapters.clear();
        assert_eq!(chapter_label_row_count(&view, 240, 32, false), 0);
    }

    #[test]
    fn chapter_label_sizing_uses_fractional_duration_like_rendering() {
        let view = ViewModel {
            playback: PlaybackStatus {
                duration: Some(Duration::from_millis(31_900)),
                ..PlaybackStatus::default()
            },
            playback_chapters: vec![
                Chapter {
                    title: "First boundary chapter title".to_owned(),
                    start_seconds: 14,
                    end_seconds: None,
                },
                Chapter {
                    title: "Second boundary chapter title".to_owned(),
                    start_seconds: 22,
                    end_seconds: None,
                },
            ],
            show_chapter_timestamps: true,
            ..ViewModel::default()
        };

        assert_eq!(
            chapter_label_row_count(&view, 80, 32, false),
            2,
            "the sizing pass must preserve the renderer's fractional column geometry"
        );
    }

    #[test]
    fn inactive_unicode_chapter_label_borrows_free_lane_space_before_activation() {
        let target_title = "Возникновение “Скелы” и криминализация украинской армии";
        let mut view = ViewModel {
            playback: PlaybackStatus {
                position: Duration::ZERO,
                duration: Some(Duration::from_secs(1_000)),
                ..PlaybackStatus::default()
            },
            playback_chapters: vec![
                Chapter {
                    title: "Вступление".to_owned(),
                    start_seconds: 0,
                    end_seconds: Some(400),
                },
                Chapter {
                    title: target_title.to_owned(),
                    start_seconds: 400,
                    end_seconds: Some(900),
                },
                Chapter {
                    title: "Заключение".to_owned(),
                    start_seconds: 900,
                    end_seconds: Some(1_000),
                },
            ],
            show_chapter_timestamps: true,
            ..ViewModel::default()
        };

        let inactive_layout = chapter_label_layout(&view, Duration::from_secs(1_000), 100, 1);
        let inactive = inactive_layout
            .placements
            .iter()
            .find(|placement| placement.index == 1)
            .expect("inactive Unicode chapter placement");
        let following = inactive_layout
            .placements
            .iter()
            .filter(|placement| placement.row == inactive.row && placement.start > inactive.start)
            .min_by_key(|placement| placement.start)
            .expect("following occupied interval");

        assert!(
            inactive.width > 20,
            "an inactive label must not retain the conservative placement width"
        );
        assert_eq!(
            inactive.start + inactive.width,
            following.start,
            "the long inactive label should use every free cell before its neighbour"
        );
        assert!(
            inactive.text.contains("Возникновение “Скелы”"),
            "Unicode text beyond the former 20-cell cap must remain visible: {}",
            inactive.text
        );

        view.playback.position = Duration::from_secs(400);
        let active_layout = chapter_label_layout(&view, Duration::from_secs(1_000), 100, 1);
        let active = active_layout
            .placements
            .iter()
            .find(|placement| placement.index == 1)
            .expect("active Unicode chapter placement");
        let active_following = active_layout
            .placements
            .iter()
            .filter(|placement| placement.row == active.row && placement.start > active.start)
            .min_by_key(|placement| placement.start)
            .expect("occupied interval after active chapter");

        assert_eq!(inactive.start, active.start);
        assert!(
            active.start.saturating_add(active.width) <= active_following.start,
            "the priority active label must not overlap its following interval"
        );
        assert!(active.text.starts_with("▶ 06:40 Возникновение"));
    }

    #[test]
    fn seek_bar_chapter_markers_and_labels_seek_to_exact_timecodes() {
        let backend = TestBackend::new(80, 4);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let media_id = MediaId::new(SourceKind::YouTube, "abcdefghijk");
        let view = ViewModel {
            playback: PlaybackStatus {
                idle: false,
                position: Duration::from_secs(35),
                duration: Some(Duration::from_secs(100)),
                ..PlaybackStatus::default()
            },
            playing_media_id: Some(media_id.clone()),
            playback_chapters: vec![
                Chapter {
                    title: "Intro".to_owned(),
                    start_seconds: 0,
                    end_seconds: Some(30),
                },
                Chapter {
                    title: "Middle".to_owned(),
                    start_seconds: 30,
                    end_seconds: Some(60),
                },
                Chapter {
                    title: "Finish".to_owned(),
                    start_seconds: 60,
                    end_seconds: Some(100),
                },
            ],
            show_chapter_timestamps: true,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw chapter timeline");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("▶ 00:30 Middle"), "{rendered}");
        assert!(
            rendered.contains('┃'),
            "current chapter needs a strong split"
        );
        assert!(
            rendered.matches('│').count() >= 2,
            "all noncurrent chapter splits must remain visible"
        );

        let (_, marker) = hit_map
            .seek_markers
            .iter()
            .find(|(action, area)| {
                area.y == hit_map.seek_bar.y
                    && action
                        == &UiAction::ActivateTimecode {
                            media_id: media_id.clone(),
                            seconds: 60,
                        }
            })
            .expect("exact chapter marker");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: marker.x,
                    row: marker.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ActivateTimecode {
                media_id,
                seconds: 60,
            }),
            "an exact split must win over the generic percentage seek target"
        );
    }

    #[test]
    fn list_prefixed_description_timecodes_render_on_the_seek_bar() {
        let description = "\
➤ 00:00 Introduction
➤ 05:45 Дело не в фамилиях, а в системе
prose 07:25 remains clickable but is not a chapter";
        let chapters = crate::links::parse_description_chapters(description, Some(600))
            .into_iter()
            .map(|chapter| Chapter {
                title: chapter.title,
                start_seconds: chapter.start_seconds,
                end_seconds: chapter.end_seconds,
            })
            .collect();
        let backend = TestBackend::new(100, 4);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            playback: PlaybackStatus {
                idle: false,
                position: Duration::from_secs(350),
                duration: Some(Duration::from_mins(10)),
                ..PlaybackStatus::default()
            },
            playing_media_id: Some(MediaId::new(SourceKind::YouTube, "abcdefghijk")),
            playback_chapters: chapters,
            show_chapter_timestamps: true,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw chapters parsed from a real description");

        let rendered = rendered_text(&terminal);
        assert!(
            rendered.contains("▶ 05:45 Дело не в фамилиях"),
            "the active description chapter must be visible above the seek bar: {rendered}"
        );
        assert_eq!(
            hit_map
                .seek_markers
                .iter()
                .filter(|(action, area)| {
                    area.y == hit_map.seek_bar.y
                        && matches!(action, UiAction::ActivateTimecode { .. })
                })
                .count(),
            2,
            "only the two list-prefixed description timecodes must become exact seek markers"
        );
    }

    #[test]
    fn seek_bar_omits_exact_advertisement_chapters_when_enabled() {
        let media_id = MediaId::new(SourceKind::YouTube, "abcdefghijk");
        let backend = TestBackend::new(120, 5);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            playback: PlaybackStatus {
                position: Duration::from_secs(10),
                duration: Some(Duration::from_secs(90)),
                ..PlaybackStatus::default()
            },
            playing_media_id: Some(media_id),
            playback_chapters: vec![
                Chapter {
                    title: "Introduction".to_owned(),
                    start_seconds: 0,
                    end_seconds: Some(30),
                },
                Chapter {
                    title: "Реклама".to_owned(),
                    start_seconds: 30,
                    end_seconds: Some(45),
                },
                Chapter {
                    title: "Main section".to_owned(),
                    start_seconds: 45,
                    end_seconds: None,
                },
            ],
            skip_advertisement_chapters: true,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw advertisement-filtered seek bar");

        assert!(!rendered_text(&terminal).contains("Реклама"));
        assert_eq!(
            hit_map
                .seek_markers
                .iter()
                .filter_map(|(action, _)| match action {
                    UiAction::ActivateTimecode { seconds, .. } => Some(*seconds),
                    _ => None,
                })
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([0, 45])
        );
    }

    #[test]
    fn chapter_labels_use_two_rows_before_hiding_collisions() {
        let media_id = MediaId::new(SourceKind::YouTube, "abcdefghijk");
        let view = ViewModel {
            playback: PlaybackStatus {
                position: Duration::from_secs(10),
                duration: Some(Duration::from_secs(100)),
                ..PlaybackStatus::default()
            },
            playing_media_id: Some(media_id),
            playback_chapters: vec![
                Chapter {
                    title: "First nearby chapter".to_owned(),
                    start_seconds: 10,
                    end_seconds: Some(11),
                },
                Chapter {
                    title: "Second nearby chapter".to_owned(),
                    start_seconds: 11,
                    end_seconds: Some(100),
                },
            ],
            ..ViewModel::default()
        };
        let backend = TestBackend::new(80, 4);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw two-line chapter labels");

        let mut label_rows = hit_map
            .seek_markers
            .iter()
            .filter_map(|(_, area)| (area.y < hit_map.seek_bar.y).then_some(area.y))
            .collect::<Vec<_>>();
        label_rows.sort_unstable();
        label_rows.dedup();
        assert_eq!(
            label_rows,
            [0, 1],
            "overlapping chapter labels should use both rows before one is hidden"
        );
        assert_eq!(hit_map.seek_bar, Rect::new(0, 2, 80, 1));
        let status_row = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 3)].symbol())
            .collect::<String>();
        assert!(status_row.contains("0:10 / 1:40"));
    }

    #[test]
    fn dense_chapter_labels_use_four_clickable_rows_when_space_allows() {
        let media_id = MediaId::new(SourceKind::YouTube, "abcdefghijk");
        let view = ViewModel {
            playback: PlaybackStatus {
                position: Duration::from_secs(17),
                duration: Some(Duration::from_secs(100)),
                ..PlaybackStatus::default()
            },
            playing_media_id: Some(media_id),
            playback_chapters: (10..18)
                .map(|seconds| Chapter {
                    title: format!("Clustered chapter {seconds}"),
                    start_seconds: seconds,
                    end_seconds: None,
                })
                .collect(),
            ..ViewModel::default()
        };
        let backend = TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw adaptive chapter labels");

        let mut label_rows = hit_map
            .seek_markers
            .iter()
            .filter_map(|(_, area)| (area.y < hit_map.seek_bar.y).then_some(area.y))
            .collect::<Vec<_>>();
        label_rows.sort_unstable();
        label_rows.dedup();
        assert_eq!(label_rows, [0, 1, 2, 3]);
        assert_eq!(hit_map.seek_bar, Rect::new(0, 4, 80, 1));
        let status_row = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 5)].symbol())
            .collect::<String>();
        assert!(status_row.contains("0:17 / 1:40"));
    }

    #[test]
    fn chapter_timestamp_toggle_changes_labels_but_not_exact_seek_targets() {
        let media_id = MediaId::new(SourceKind::YouTube, "abcdefghijk");
        let mut view = ViewModel {
            playback: PlaybackStatus {
                duration: Some(Duration::from_secs(100)),
                ..PlaybackStatus::default()
            },
            playing_media_id: Some(media_id.clone()),
            playback_chapters: vec![Chapter {
                title: "Introduction".to_owned(),
                start_seconds: 10,
                end_seconds: Some(100),
            }],
            show_chapter_timestamps: true,
            ..ViewModel::default()
        };
        let backend = TestBackend::new(80, 4);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw timestamped chapter");
        assert!(rendered_text(&terminal).contains("00:10 Introduction"));

        view.show_chapter_timestamps = false;
        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw names-only chapter");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Introduction"));
        assert!(!rendered.contains("00:10 Introduction"));
        assert!(hit_map.seek_markers.iter().any(|(action, _)| {
            action
                == &UiAction::ActivateTimecode {
                    media_id: media_id.clone(),
                    seconds: 10,
                }
        }));
    }

    #[test]
    fn chapter_labels_hide_one_final_period_but_preserve_ellipses() {
        let view = ViewModel {
            playback: PlaybackStatus {
                position: Duration::from_secs(10),
                duration: Some(Duration::from_secs(100)),
                ..PlaybackStatus::default()
            },
            playing_media_id: Some(MediaId::new(SourceKind::YouTube, "abcdefghijk")),
            playback_chapters: vec![
                Chapter {
                    title: "Sentence title.".to_owned(),
                    start_seconds: 10,
                    end_seconds: Some(70),
                },
                Chapter {
                    title: "Wait...".to_owned(),
                    start_seconds: 70,
                    end_seconds: Some(100),
                },
            ],
            ..ViewModel::default()
        };
        let backend = TestBackend::new(100, 4);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw normalized chapter labels");

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Sentence title"));
        assert!(!rendered.contains("Sentence title."));
        assert!(rendered.contains("Wait..."));
    }

    #[test]
    fn short_chapter_seekbar_keeps_markers_and_places_exact_status_below_track() {
        let media_id = MediaId::new(SourceKind::YouTube, "abcdefghijk");
        let view = ViewModel {
            playback: PlaybackStatus {
                idle: false,
                position: Duration::from_secs(18 * 60 + 28),
                duration: Some(Duration::from_secs(60 * 60 + 33 * 60 + 6)),
                paused: true,
                volume: 80,
                speed: 1.0,
                ..PlaybackStatus::default()
            },
            playing_media_id: Some(media_id.clone()),
            playback_chapters: vec![
                Chapter {
                    title: "Introduction".to_owned(),
                    start_seconds: 0,
                    end_seconds: Some(16 * 60),
                },
                Chapter {
                    title: "Current chapter".to_owned(),
                    start_seconds: 16 * 60,
                    end_seconds: None,
                },
            ],
            ..ViewModel::default()
        };
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw short chapter seek bar");

        assert_eq!(hit_map.seek_bar, Rect::new(0, 0, 120, 1));
        assert!(
            hit_map.seek_markers.iter().any(|(action, area)| {
                area.y == hit_map.seek_bar.y
                    && action
                        == &UiAction::ActivateTimecode {
                            media_id: media_id.clone(),
                            seconds: 16 * 60,
                        }
            }),
            "short terminals must retain exact chapter split click targets"
        );
        let track_row = (0..120)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        let status_row = (0..120)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
            .collect::<String>();
        let expected = "18:28 / 1:33:06  1×  vol 80% ⏸";
        assert!(!track_row.contains(expected));
        assert!(status_row.contains(expected));
    }

    #[test]
    fn seek_status_uses_pause_symbol_without_redundant_playing_marker() {
        let backend = TestBackend::new(80, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        let mut view = ViewModel {
            playback: PlaybackStatus {
                idle: false,
                paused: true,
                position: Duration::from_secs(10),
                duration: Some(Duration::from_secs(100)),
                volume: 80,
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw paused seek status");
        let paused = rendered_text(&terminal);
        assert!(paused.contains("vol 80% ⏸"));
        assert!(!paused.contains("paused"));
        assert!(!paused.contains("playing"));

        view.playback.paused = false;
        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw playing seek status");
        let playing = rendered_text(&terminal);
        assert!(playing.contains("vol 80%"));
        assert!(
            !playing.contains("vol 80% ▶"),
            "active playback does not need a redundant playing marker in the seek status"
        );
        assert!(!playing.contains("paused"));
        assert!(!playing.contains("playing"));

        view.playback.idle = true;
        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw idle seek status");
        let idle = rendered_text(&terminal);
        assert!(idle.contains("vol 80%"));
        assert!(!idle.contains("vol 80% ▶"));
        assert!(!idle.contains("vol 80% ⏸"));
    }

    #[test]
    fn colliding_chapter_splits_remain_visible_without_inventing_unknown_positions() {
        let chapters = vec![
            Chapter {
                title: "First".to_owned(),
                start_seconds: 10,
                end_seconds: Some(11),
            },
            Chapter {
                title: "Second".to_owned(),
                start_seconds: 11,
                end_seconds: None,
            },
        ];
        let backend = TestBackend::new(8, 3);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        let mut view = ViewModel {
            playback: PlaybackStatus {
                position: Duration::from_secs(11),
                duration: Some(Duration::from_secs(100)),
                ..PlaybackStatus::default()
            },
            playing_media_id: Some(MediaId::new(SourceKind::YouTube, "abcdefghijk")),
            playback_chapters: chapters.clone(),
            show_chapter_timestamps: true,
            ..ViewModel::default()
        };

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw colliding markers");
        assert!(rendered_text(&terminal).contains('┇'));

        view.playback.duration = None;
        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw unknown-duration chapter");
        let rendered = rendered_text(&terminal);
        assert!(
            rendered.contains("▶ 0:11"),
            "a narrow unknown-duration timeline must retain the active timestamp: {rendered}"
        );
        assert!(
            rendered.contains('…'),
            "the active chapter title must be truncated explicitly: {rendered}"
        );
        assert!(hit_map.seek_markers.is_empty());
    }

    #[test]
    fn seek_bar_layers_discontinuous_cached_ranges_behind_progress_for_both_styles() {
        for seek_bar_style in [SeekBarStyle::Line, SeekBarStyle::NyanCat] {
            let backend = TestBackend::new(80, 2);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let view = ViewModel {
                playback: PlaybackStatus {
                    idle: false,
                    position: Duration::from_secs(10),
                    duration: Some(Duration::from_secs(100)),
                    buffered_ranges: vec![
                        crate::playback::BufferedRange {
                            start: Duration::ZERO,
                            end: Duration::from_secs(15),
                        },
                        crate::playback::BufferedRange {
                            start: Duration::from_secs(30),
                            end: Duration::from_secs(40),
                        },
                        crate::playback::BufferedRange {
                            start: Duration::from_secs(60),
                            end: Duration::from_secs(80),
                        },
                        crate::playback::BufferedRange {
                            start: Duration::from_secs(90),
                            end: Duration::from_secs(120),
                        },
                    ],
                    ..PlaybackStatus::default()
                },
                ..ViewModel::default()
            };
            let settings = UiSettings {
                seek_bar_style,
                ..UiSettings::default()
            };
            let mut hit_map = HitMap::default();

            terminal
                .draw(|frame| {
                    render_seek_bar(
                        frame,
                        frame.area(),
                        &view,
                        &settings,
                        &Theme::new(false),
                        &mut hit_map,
                    );
                })
                .expect("draw");
            let buffer = terminal.backend().buffer();

            assert_eq!(buffer[(4, 0)].fg, Color::Cyan);
            assert_eq!(
                buffer[(4, 0)].symbol(),
                ratatui::symbols::block::FULL,
                "played progress must cover an overlapping cached range"
            );
            for x in [24, 31, 48, 63, 72, 79] {
                assert_eq!(buffer[(x, 0)].fg, Color::DarkGray);
                assert_eq!(buffer[(x, 0)].symbol(), ratatui::symbols::block::FULL);
            }
            for x in [20, 40, 68] {
                assert_eq!(
                    buffer[(x, 0)].symbol(),
                    " ",
                    "gaps between cached intervals must remain visible"
                );
            }

            let status_row = (0..80).map(|x| buffer[(x, 1)].symbol()).collect::<String>();
            assert!(
                status_row.contains("0:10 / 1:40"),
                "the status must remain readable below cached seek ranges"
            );
            assert_ne!(
                buffer[(24, 1)].symbol(),
                ratatui::symbols::block::FULL,
                "cached seek styling must not bleed into the status row"
            );
            let rendered = buffer
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            if seek_bar_style == SeekBarStyle::NyanCat {
                assert!(rendered.contains("=^.^="));
            }
        }
    }

    #[test]
    fn playing_status_has_an_exact_click_target_only_while_playback_is_active() {
        let backend = TestBackend::new(220, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        let active = ViewModel {
            playback: PlaybackStatus {
                idle: false,
                duration: Some(Duration::from_secs(120)),
                title: Some("Fixture title".to_owned()),
                volume: 80,
                speed: 1.0,
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &active,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw");

        let target = hit_map.now_playing.expect("now-playing click target");
        assert_eq!(target.height, 1);
        assert_eq!(target.width, "Fixture title".chars().count() as u16);
        let rendered_status = (target.x..target.right())
            .map(|x| terminal.backend().buffer()[(x, target.y)].symbol())
            .collect::<String>();
        assert_eq!(rendered_status, "Fixture title");

        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: target.x,
            row: target.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click, &hit_map, &active),
            Some(UiAction::ShowNowPlaying)
        );

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &ViewModel::default(),
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw inactive state");
        assert!(hit_map.now_playing.is_none());
    }

    #[test]
    fn radio_metadata_extends_the_clickable_stable_station_title() {
        let backend = TestBackend::new(220, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        let active = ViewModel {
            playback: PlaybackStatus {
                idle: false,
                title: Some("France Musique".to_owned()),
                volume: 80,
                speed: 1.0,
                ..PlaybackStatus::default()
            },
            radio_now_playing: Some("On air: Le Concert — Current segment".to_owned()),
            ..ViewModel::default()
        };

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &active,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw Radio status");

        let expected = "France Musique · On air: Le Concert — Current segment";
        let target = hit_map.now_playing.expect("Radio now-playing target");
        assert_eq!(target.width, terminal_text_width(expected));
        let rendered_status = (target.x..target.right())
            .map(|x| terminal.backend().buffer()[(x, target.y)].symbol())
            .collect::<String>();
        assert_eq!(rendered_status, expected);
        assert!(!rendered_status.contains("http"));
    }

    #[test]
    fn render_shows_known_and_unknown_download_progress_without_hiding_seekbar() {
        let backend = TestBackend::new(160, 34);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            download: Some(DownloadView {
                title: "Fixture video".to_owned(),
                downloaded_bytes: 2048,
                total_bytes: Some(4096),
                bytes_per_second: Some(512),
                eta_seconds: Some(4),
                active: true,
                completed_path: None,
            }),
            ..ViewModel::default()
        };
        view.playback.duration = Some(Duration::from_secs(120));
        view.playback.position = Duration::from_secs(30);
        let settings = UiSettings::default();
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &settings, &mut hit_map))
            .expect("draw known progress");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("Downloading Fixture video"));
        assert!(rendered.contains("50.0%"));
        assert!(rendered.contains("2.0 KiB / 4.0 KiB"));
        assert!(rendered.contains("0:30 / 2:00"));

        view.download = Some(DownloadView {
            title: "Unknown-size stream".to_owned(),
            downloaded_bytes: 1024,
            active: true,
            ..DownloadView::default()
        });
        terminal
            .draw(|frame| render(frame, &view, &settings, &mut hit_map))
            .expect("draw unknown progress");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("Unknown-size stream"));
        assert!(rendered.contains("1.0 KiB · size unknown"));
    }

    #[test]
    fn download_ratio_and_binary_units_are_bounded_without_float_casts() {
        assert!((download_ratio(2, 4) - 0.5).abs() < f64::EPSILON);
        assert!((download_ratio(5, 4) - 1.0).abs() < f64::EPSILON);
        assert!(download_ratio(1, 0).abs() < f64::EPSILON);
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
    }

    #[test]
    fn render_keeps_the_confined_completed_download_path_visible() {
        let backend = TestBackend::new(140, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            download: Some(DownloadView {
                title: "Fixture".to_owned(),
                downloaded_bytes: 4096,
                total_bytes: Some(4096),
                active: false,
                completed_path: Some(
                    "/home/listener/.config/youta/downloads/fixture.opus".to_owned(),
                ),
                ..DownloadView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw completed path");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("Downloaded:"));
        assert!(rendered.contains("downloads/fixture.opus"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|cell| cell.bg == Color::Green),
            "only a validated completed download receives the dark-green background"
        );

        let active = DownloadView {
            title: "Still downloading".to_owned(),
            downloaded_bytes: 1,
            total_bytes: Some(2),
            active: true,
            ..DownloadView::default()
        };
        let mut active_terminal =
            Terminal::new(TestBackend::new(80, 2)).expect("active download terminal");
        active_terminal
            .draw(|frame| {
                render_download_bar(frame, frame.area(), &active, &Theme::new(false));
            })
            .expect("draw active download");
        assert!(
            active_terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|cell| cell.bg != Color::Green),
            "in-progress and failed downloads must not use the success background"
        );
    }

    #[test]
    fn diagnostic_popup_renders_scrolled_report_position_and_github_buttons() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut report_lines = vec!["FIRST_MARKER".to_owned()];
        report_lines.extend((1..=60).map(|line| format!("filler diagnostic line {line:02}")));
        report_lines.push("SCROLLED_MARKER".to_owned());
        let view = ViewModel {
            error_popup: Some(ErrorPopupView {
                title: "Playback failed".to_owned(),
                report: report_lines.join("\n"),
                scroll_offset: 10,
                gh_available: true,
                action_status: None,
                yt_dlp_forbidden: None,
                github_issue_submission: GitHubIssueSubmissionView::Idle,
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render(frame, &view, &UiSettings::default(), &mut hit_map);
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("Playback failed"));
        assert!(!rendered.contains("FIRST_MARKER"));
        assert!(rendered.contains("filler diagnostic line 10"));
        assert!(rendered.contains("Lines 11–32 of 62"), "{rendered}");
        let scrollbar_thumb_cells = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.symbol() == "█")
            .count();
        assert!(
            scrollbar_thumb_cells > 0,
            "a scrollable report must render a visible thumb"
        );
        assert!(rendered.contains("[c] Copy"));
        assert!(rendered.contains("[i] Copy + open issue"));
        assert!(rendered.contains("[g] Submit GitHub issue"));
        assert!(rendered.contains("[Esc] Close"));
        assert_eq!(hit_map.error_buttons.len(), 4);
    }

    #[test]
    fn diagnostic_popup_keeps_confirmation_visible_and_contains_submitted_controls() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        let mut view = ViewModel {
            external_opener_available: true,
            error_popup: Some(ErrorPopupView {
                title: "Playback failed".to_owned(),
                report: "complete report".to_owned(),
                gh_available: true,
                action_status: Some("Copied with wl-copy".to_owned()),
                github_issue_submission: GitHubIssueSubmissionView::Confirming,
                ..ErrorPopupView::default()
            }),
            ..ViewModel::default()
        };

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw confirmation");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("This creates a public issue"));
        assert!(rendered.contains("Copied with wl-copy"));
        assert!(rendered.contains("[Enter] Submit"));
        assert!(rendered.contains("[Esc] Cancel"));
        assert!(!rendered.contains("Copy + open issue"));

        let url = "https://github.com/vitaly-zdanevich/youta/issues/123";
        view.error_popup = Some(ErrorPopupView {
            title: "Playback failed".to_owned(),
            report: "complete report".to_owned(),
            gh_available: true,
            action_status: Some("Copied with wl-copy".to_owned()),
            github_issue_submission: GitHubIssueSubmissionView::Submitted {
                url: url.to_owned(),
            },
            ..ErrorPopupView::default()
        });
        hit_map.error_buttons.clear();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw submitted issue");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains(url));
        assert!(rendered.contains("[o] Open issue"));
        assert!(!rendered.contains(&format!("[o] {url}")));
        assert!(!rendered.contains("Submit GitHub issue"));
        assert!(
            hit_map
                .error_buttons
                .iter()
                .any(|(action, _)| action == &UiAction::OpenGitHubIssueSubmissionTarget)
        );
        let popup = centered_rect(92, 88, Rect::new(0, 0, 80, 24));
        let inner = popup.inner(ratatui::layout::Margin {
            horizontal: 1,
            vertical: 1,
        });
        assert!(hit_map.error_buttons.iter().all(|(_, target)| {
            target.x >= inner.x
                && target.right() <= inner.right()
                && target.y >= inner.y
                && target.bottom() <= inner.bottom()
        }));

        view.error_popup = Some(ErrorPopupView {
            title: "Playback failed".to_owned(),
            report: "complete report".to_owned(),
            gh_available: true,
            action_status: Some("Copied with wl-copy".to_owned()),
            github_issue_submission: GitHubIssueSubmissionView::Failed {
                message: "gh rejected the request".to_owned(),
            },
            ..ErrorPopupView::default()
        });
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw failed submission");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("gh rejected the request"));
        assert!(rendered.contains("Copied with wl-copy"));
        assert!(rendered.contains("[g] Retry submission"));
    }

    #[test]
    fn yt_dlp_forbidden_popup_renders_progressive_versions_links_and_compact_actions() {
        let backend = TestBackend::new(140, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            external_opener_available: true,
            error_popup: Some(yt_dlp_forbidden_error(true)),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw structured yt-dlp error");
        let rendered = rendered_text(&terminal);

        assert!(rendered.contains("403 from yt-dlp — try later or update it."));
        assert!(rendered.contains("A 403 can be temporary or authentication-related."));
        assert!(rendered.contains("Installed: 2026.07.04 (released 2026-07-04)"));
        assert!(rendered.contains("GitHub latest: Loading…"));
        assert!(
            rendered.contains(
                "Gentoo latest stable (amd64): Unavailable (package metadata unavailable)"
            )
        );
        assert!(rendered.contains(YT_DLP_PROJECT_URL));
        assert!(rendered.contains(GENTOO_YT_DLP_PACKAGE_URL));
        assert!(!rendered.contains("COPY_ONLY_DIAGNOSTIC_REPORT"));
        assert!(rendered.contains("[u] Project"));
        assert!(rendered.contains("[p] Gentoo package"));
        assert!(rendered.contains("[c] Copy report"));
        assert!(rendered.contains("[Esc] Close"));
        assert!(!rendered.contains("GitHub issue"));

        let actions = hit_map
            .error_buttons
            .iter()
            .map(|(action, _)| action)
            .collect::<Vec<_>>();
        assert_eq!(
            actions,
            vec![
                &UiAction::OpenYtDlpProject,
                &UiAction::OpenGentooYtDlpPackage,
                &UiAction::CopyErrorReport,
                &UiAction::DismissErrorPopup,
            ]
        );

        let (_, project_area) = hit_map
            .error_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::OpenYtDlpProject)
            .expect("project button");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: project_area.x,
                    row: project_area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::OpenYtDlpProject)
        );

        let non_gentoo = ViewModel {
            external_opener_available: true,
            error_popup: Some(yt_dlp_forbidden_error(false)),
            ..ViewModel::default()
        };
        let mut non_gentoo_hit_map = HitMap::default();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    &non_gentoo,
                    &UiSettings::default(),
                    &mut non_gentoo_hit_map,
                );
            })
            .expect("draw non-Gentoo yt-dlp error");
        let rendered = rendered_text(&terminal);
        assert!(!rendered.contains("Gentoo latest stable"));
        assert!(!rendered.contains(GENTOO_YT_DLP_PACKAGE_URL));
        assert!(
            non_gentoo_hit_map
                .error_buttons
                .iter()
                .all(|(action, _)| action != &UiAction::OpenGentooYtDlpPackage)
        );
    }

    #[test]
    fn diagnostic_scrollbar_tracks_the_clamped_final_viewport() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let report = (1..=62)
            .map(|line| format!("diagnostic line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let view = ViewModel {
            error_popup: Some(ErrorPopupView {
                title: "Playback failed".to_owned(),
                report,
                scroll_offset: usize::MAX,
                ..ErrorPopupView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw final report viewport");
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("Lines 41–62 of 62"), "{rendered}");

        let width = usize::from(buffer.area().width);
        let thumb_positions = buffer
            .content()
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.symbol() == "█")
            .map(|(index, _)| (index % width, index / width))
            .collect::<Vec<_>>();
        let &(scrollbar_x, _) = thumb_positions.first().expect("scrollbar thumb");
        let thumb_top = thumb_positions
            .iter()
            .map(|(_, y)| *y)
            .min()
            .expect("scrollbar thumb top");
        let track_top = buffer
            .content()
            .iter()
            .enumerate()
            .filter(|(index, cell)| {
                index % width == scrollbar_x && matches!(cell.symbol(), "█" | "│")
            })
            .map(|(index, _)| index / width)
            .min()
            .expect("scrollbar track top");
        assert!(
            thumb_top > track_top,
            "the clamped final viewport must move the thumb below the start"
        );
    }

    #[test]
    fn diagnostic_popup_renders_copy_and_open_fallback_button() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            error_popup: Some(ErrorPopupView {
                title: "Network error".to_owned(),
                report: "full report".to_owned(),
                ..ErrorPopupView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render(frame, &view, &UiSettings::default(), &mut hit_map);
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("[i] Copy + open issue"));
        assert!(!rendered.contains("[g] Fill GitHub issue"));
    }

    #[test]
    fn youtube_setup_popup_masks_secret_and_renders_storage_notice_and_controls() {
        let backend = TestBackend::new(140, 34);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let secret = "AIzaActualSecretNeverRender";
        let view = ViewModel {
            youtube_setup_popup: Some(YouTubeSetupPopupView {
                selected_field: YouTubeSetupField::ApiKey,
                api_key: secret.to_owned(),
                invidious_url: "https://invidious.example".to_owned(),
                api_key_path: "/home/listener/.config/youta/secrets/credentials.toml".to_owned(),
                invidious_path: "/home/listener/.config/youta/config.toml".to_owned(),
                validation_error: Some("API key was rejected".to_owned()),
            }),
            ..ViewModel::default()
        };
        let debug_view = format!("{view:?}");
        assert!(!debug_view.contains(secret));
        assert!(debug_view.contains("[REDACTED]"));
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render(frame, &view, &UiSettings::default(), &mut hit_map);
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        let printable = rendered
            .chars()
            .map(
                |character| {
                    if character.is_ascii() { character } else { ' ' }
                },
            )
            .collect::<String>();
        let normalized = printable.split_whitespace().collect::<Vec<_>>().join(" ");

        assert!(rendered.contains("Configure YouTube metadata"));
        assert!(rendered.contains("YouTube API key (masked)"));
        assert!(!rendered.contains(secret));
        assert!(rendered.contains(&"*".repeat(secret.len())));
        assert!(rendered.contains("https://invidious.example"));
        assert!(normalized.contains("create/select a Google Cloud project"));
        assert!(normalized.contains("enable YouTube Data API v3"));
        assert!(normalized.contains("Create credentials > API key"));
        assert!(normalized.contains("API restrictions > Restrict key"));
        assert!(normalized.contains("Restriction blocks other Google APIs"));
        assert!(normalized.contains("[F1] Google guide"));
        assert!(normalized.contains(YOUTUBE_API_KEY_GUIDE_URL.trim_start_matches("https://")));
        assert!(normalized.contains("[F2] Google Cloud"));
        assert!(normalized.contains(GOOGLE_CLOUD_CREDENTIALS_URL.trim_start_matches("https://")));
        assert!(normalized.contains("choose a public instance from the official list"));
        assert!(normalized.contains("[F3] Instance list"));
        assert!(normalized.contains(INVIDIOUS_INSTANCES_URL.trim_start_matches("https://")));
        assert!(normalized.contains("/home/listener/.config/youta/secrets/credentials.toml"));
        assert!(normalized.contains("/home/listener/.config/youta/config.toml"));
        assert!(normalized.contains("API key saves to:"));
        assert!(normalized.contains("Invidious URL saves to:"));
        assert!(normalized.contains("API keys are plaintext"));
        assert!(normalized.contains("directories 0700, files 0600"));
        assert!(normalized.contains("Environment variables override"));
        assert!(normalized.contains("Error: API key was rejected"));
        assert!(normalized.contains("[Enter] Save and retry"));
        assert!(normalized.contains("[Esc] Cancel"));
        assert_eq!(hit_map.youtube_setup_fields.len(), 2);
        assert_eq!(hit_map.youtube_setup_buttons.len(), 5);
    }

    #[test]
    fn yandex_music_setup_masks_token_and_exposes_private_storage_controls() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let secret = "yandex-oauth-token-never-render";
        let view = ViewModel {
            yandex_music_setup_popup: Some(YandexMusicSetupPopupView {
                token: secret.to_owned(),
                token_path: "/home/listener/.config/youta/secrets/credentials.toml".to_owned(),
                validating: false,
                validation_error: Some("token was rejected".to_owned()),
            }),
            ..ViewModel::default()
        };
        let debug_view = format!("{view:?}");
        assert!(!debug_view.contains(secret));
        assert!(debug_view.contains("[REDACTED]"));
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render(frame, &view, &UiSettings::default(), &mut hit_map);
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        let normalized = rendered.split_whitespace().collect::<Vec<_>>().join(" ");

        assert!(rendered.contains("Configure Yandex Music"));
        assert!(rendered.contains("OAuth token (masked)"));
        assert!(!rendered.contains(secret));
        assert!(rendered.contains(&"*".repeat(secret.len())));
        assert!(normalized.contains("not an API key"));
        assert!(normalized.contains(YANDEX_OAUTH_GUIDE_URL.trim_start_matches("https://")));
        assert!(normalized.contains("secrets/credentials.toml"));
        assert!(normalized.contains("YOUTA_PROVIDERS__YANDEX_MUSIC_TOKEN"));
        assert!(normalized.contains("Error: token was rejected"));
        assert!(normalized.contains("[Enter] Save and load"));
        assert!(normalized.contains("[Esc] Cancel"));
        assert!(hit_map.yandex_music_setup_field.is_some());
        assert_eq!(hit_map.yandex_music_setup_buttons.len(), 3);
    }

    #[test]
    fn yandex_music_setup_is_read_only_while_the_token_is_validating() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            yandex_music_setup_popup: Some(YandexMusicSetupPopupView {
                token: "candidate-token".to_owned(),
                token_path: "/home/listener/.config/youta/secrets/credentials.toml".to_owned(),
                validating: true,
                validation_error: None,
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw validating Yandex Music setup");
        let rendered = rendered_text(&terminal);
        let normalized = rendered.split_whitespace().collect::<Vec<_>>().join(" ");

        assert!(normalized.contains("Validating Yandex Music"));
        assert!(normalized.contains("OAuth token (masked; read-only)"));
        assert!(normalized.contains("Validating the candidate token before saving"));
        assert!(normalized.contains("[Enter] Validating"));
        assert!(!normalized.contains("[Enter] Save and load"));
        assert!(hit_map.yandex_music_setup_field.is_none());
        assert!(
            !hit_map
                .yandex_music_setup_buttons
                .iter()
                .any(|(action, _)| action == &UiAction::SubmitYandexMusicSetup)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &view),
            None
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), &view),
            None
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &view),
            None
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &view),
            Some(UiAction::DismissYandexMusicSetup)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), &view),
            Some(UiAction::OpenYandexOAuthGuide)
        );
    }

    #[test]
    fn rss_subscription_popup_renders_storage_validation_and_mouse_targets() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            screen: Screen::Subscriptions,
            rss_subscription_popup: Some(RssSubscriptionPopupView {
                url: "https://podcasts.example/private-feed.xml".to_owned(),
                validation_error: Some("feed URL must use HTTP or HTTPS".to_owned()),
                config_path: "/home/listener/.config/youta/subscriptions.opml".to_owned(),
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw RSS subscription popup");
        let normalized = rendered_text(&terminal)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for expected in [
            "Add RSS podcast feed",
            "Saves a portable audio/video podcast feed subscription to OPML",
            "https://podcasts.example/private-feed.xml",
            "Will save to: /home/listener/.config/youta/subscriptions.opml",
            "Error: feed URL must use HTTP or HTTPS",
            "[Enter] Add feed",
            "[Esc] Cancel",
        ] {
            assert!(
                normalized.contains(expected),
                "RSS popup omitted `{expected}`:\n{normalized}"
            );
        }

        let field = hit_map
            .rss_subscription_field
            .expect("RSS URL field target");
        assert_eq!(hit_map.rss_subscription_buttons.len(), 2);
        let click = |area: Rect| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click(field), &hit_map, &view),
            Some(UiAction::OpenRssSubscriptionPopup)
        );
        for expected in [
            UiAction::SubmitRssSubscription,
            UiAction::DismissRssSubscriptionPopup,
        ] {
            let target = hit_map
                .rss_subscription_buttons
                .iter()
                .find(|(action, _)| action == &expected)
                .map(|(_, target)| *target)
                .expect("RSS popup button target");
            assert_eq!(mouse_action(click(target), &hit_map, &view), Some(expected));
        }
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 1,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            None,
            "the modal popup must block underlying tabs"
        );
    }

    #[test]
    fn playlist_chooser_exposes_membership_and_keyboard_equivalent_mouse_targets() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            playlist_popup: Some(PlaylistPopupView {
                item_title: "Fixture episode".to_owned(),
                playlists: vec![
                    PlaylistChoiceView {
                        playlist_id: "todo".to_owned(),
                        name: "todo".to_owned(),
                        contains_item: true,
                    },
                    PlaylistChoiceView {
                        playlist_id: "research".to_owned(),
                        name: "Research".to_owned(),
                        contains_item: false,
                    },
                ],
                selected: 0,
                ..PlaylistPopupView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw playlist chooser");
        let normalized = rendered_text(&terminal)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for expected in [
            "Add to playlist",
            "Item: Fixture episode",
            "[✓] todo",
            "[ ] Research",
            "[Enter] Add/remove",
            "[n] New playlist",
            "[Esc] Close",
        ] {
            assert!(
                normalized.contains(expected),
                "playlist chooser omitted `{expected}`:\n{normalized}"
            );
        }

        assert_eq!(hit_map.playlist_popup_first_index, 0);
        assert!(hit_map.playlist_popup_rows.height >= 2);
        let second_row_click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hit_map.playlist_popup_rows.x,
            row: hit_map.playlist_popup_rows.y.saturating_add(1),
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(second_row_click, &hit_map, &view),
            Some(UiAction::SelectPlaylistPopupRow(1))
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: hit_map.playlist_popup_rows.x,
                    row: hit_map.playlist_popup_rows.y.saturating_add(3),
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            None,
            "blank chooser rows must not manufacture model indices"
        );
        for expected in [
            UiAction::ToggleSelectedPlaylistMembership,
            UiAction::BeginNewPlaylist,
            UiAction::DismissPlaylistPopup,
        ] {
            let target = hit_map
                .playlist_popup_buttons
                .iter()
                .find(|(action, _)| action == &expected)
                .map(|(_, area)| *area)
                .expect("playlist chooser button");
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: target.x,
                        row: target.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                Some(expected)
            );
        }
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column: hit_map.playlist_popup_rows.x,
                    row: hit_map.playlist_popup_rows.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::MovePlaylistPopupSelection(1))
        );
    }

    #[test]
    fn playlist_editor_shows_bounded_fields_errors_and_hidden_hotkey_mouse_actions() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            playlist_popup: Some(PlaylistPopupView {
                mode: PlaylistPopupMode::Create,
                editor_field: PlaylistEditorField::Description,
                editor_name: "Road trip".to_owned(),
                editor_description: "Episodes for the train".to_owned(),
                name_limit: 80,
                description_limit: 500,
                validation_error: Some("playlist name already exists".to_owned()),
                ..PlaylistPopupView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw playlist editor");
        let normalized = rendered_text(&terminal)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for expected in [
            "New playlist",
            "Name bytes (9/80)",
            "Description (22/500)",
            "Error: playlist name already exists",
            "[Enter] Create and add",
            "[Esc] Back/close",
        ] {
            assert!(
                normalized.contains(expected),
                "playlist editor omitted `{expected}`:\n{normalized}"
            );
        }
        assert_eq!(hit_map.playlist_popup_fields.len(), 2);
        let name_target = hit_map
            .playlist_popup_fields
            .iter()
            .find(|(field, _)| *field == PlaylistEditorField::Name)
            .map(|(_, area)| *area)
            .expect("name field target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: name_target.x,
                    row: name_target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SelectPlaylistEditorField(
                PlaylistEditorField::Name
            ))
        );

        terminal
            .draw(|frame| {
                render(
                    frame,
                    &view,
                    &UiSettings {
                        show_hotkeys: false,
                        ..UiSettings::default()
                    },
                    &mut hit_map,
                );
            })
            .expect("draw playlist editor without hotkey values");
        let hidden = rendered_text(&terminal);
        assert!(hidden.contains("Create and add"));
        assert!(!hidden.contains("[Enter]"));
        assert!(!hidden.contains("[Esc]"));
        let submit = hit_map
            .playlist_popup_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::CreatePlaylistAndAdd)
            .map(|(_, area)| *area)
            .expect("hidden-hotkey submit target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: submit.x,
                    row: submit.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::CreatePlaylistAndAdd)
        );

        view.playlist_popup.as_mut().expect("popup").mode = PlaylistPopupMode::Edit;
        view.playlist_popup
            .as_mut()
            .expect("popup")
            .editing_playlist_id = Some("reserved:todo".to_owned());
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw playlist editor");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Edit playlist"));
        assert!(rendered.contains("[Enter] Save changes"));
        assert!(!rendered.contains("reserved:todo"));
    }

    #[test]
    fn rss_subscription_popup_redacts_private_url_from_debug_output() {
        let private_url =
            "https://listener:secret@podcasts.example/private.xml?token=unprintable-secret";
        let view = ViewModel {
            rss_subscription_popup: Some(RssSubscriptionPopupView {
                url: private_url.to_owned(),
                validation_error: Some(format!("invalid private feed: {private_url}")),
                ..RssSubscriptionPopupView::default()
            }),
            ..ViewModel::default()
        };

        let debug_view = format!("{view:?}");
        assert!(debug_view.contains("[REDACTED]"));
        assert!(!debug_view.contains("listener"));
        assert!(!debug_view.contains("secret"));
        assert!(!debug_view.contains("token"));
        assert!(!debug_view.contains(private_url));
    }

    #[test]
    fn youtube_setup_popup_keeps_provider_instructions_on_an_80_by_24_terminal() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            youtube_setup_popup: Some(YouTubeSetupPopupView {
                api_key_path: "/home/listener/.config/youta/secrets/credentials.toml".to_owned(),
                invidious_path: "/home/listener/.config/youta/config.toml".to_owned(),
                ..YouTubeSetupPopupView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render(frame, &view, &UiSettings::default(), &mut hit_map);
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        let printable = rendered
            .chars()
            .map(
                |character| {
                    if character.is_ascii() { character } else { ' ' }
                },
            )
            .collect::<String>();
        let normalized = printable.split_whitespace().collect::<Vec<_>>().join(" ");

        for expected in [
            "create/select a Google Cloud project",
            "enable YouTube Data API v3",
            "Create credentials > API key",
            "API restrictions > Restrict key > YouTube Data API v3 > Save",
            "Restriction blocks other Google APIs",
            "[F1] Google guide",
            "developers.google.com/youtube/registering_an_application",
            "[F2] Google Cloud",
            "console.cloud.google.com/apis/credentials",
            "choose a public instance from the official list",
            "[F3] Instance list",
            "docs.invidious.io/instances/",
            "API key saves to: /home/listener/.config/youta/secrets/credentials.toml",
            "Invidious URL saves to: /home/listener/.config/youta/config.toml",
            "API keys are plaintext",
            "[Enter] Save and retry",
            "[Esc] Cancel",
        ] {
            assert!(
                normalized.contains(expected),
                "80x24 popup omitted `{expected}`:\n{normalized}"
            );
        }
        assert_eq!(hit_map.youtube_setup_buttons.len(), 5);
    }

    #[test]
    fn render_shows_selectable_external_links_and_records_hit_areas() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            details: Some(DetailView {
                title: "Mock video".to_owned(),
                description: "Mock description".to_owned(),
                wikidata: "loaded".to_owned(),
                links: vec![DetailLinkView {
                    label: "Douglas Adams (Q42)".to_owned(),
                    url: "https://www.wikidata.org/wiki/Q42".to_owned(),
                    wikidata_item_id: Some("Q42".to_owned()),
                    ..DetailLinkView::default()
                }],
                ..DetailView::default()
            }),
            selected_detail_link: Some(0),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render(frame, &view, &UiSettings::default(), &mut hit_map);
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(!rendered.contains("External links"));
        assert!(!rendered.contains('›'));
        assert!(rendered.contains("Douglas Adams (Q42)"));
        assert!(rendered.contains("[W] ▸"));
        assert!(!rendered.contains('🧾'));
        assert!(!rendered.contains("instance of (P31)"));
        assert_eq!(hit_map.detail_links.len(), 1);
        assert_eq!(hit_map.detail_links[0].0, 0);
        let (_, disclosure_area) = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::ToggleWikidataStatements(0))
            .expect("Wikidata disclosure hit target");
        let (_, link_area) = hit_map.detail_links[0];
        assert_eq!(
            link_area.x,
            disclosure_area.right().saturating_add(1),
            "the disclosure must not overlap the exact external-link hitbox"
        );
        let label_cell = &terminal.backend().buffer()
            [(disclosure_area.right().saturating_add(1), disclosure_area.y)];
        assert_eq!(label_cell.symbol(), "D");
        assert_eq!(label_cell.fg, Color::Reset);
        assert!(!label_cell.modifier.contains(Modifier::BOLD));
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: disclosure_area.x,
                    row: disclosure_area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ToggleWikidataStatements(0))
        );

        let details = view.details.as_mut().expect("fixture details");
        details.expanded_wikidata_item = Some("Q42".to_owned());
        let text = "instance of (P31): human (Q5)".to_owned();
        let start_byte = text.find("human (Q5)").expect("linked value");
        details.wikidata_entities.push(DetailWikidataEntityView {
            item_id: "Q42".to_owned(),
            text,
            value_links: vec![DetailWikidataValueLinkView {
                start_byte,
                end_byte: start_byte + "human (Q5)".len(),
                url: "https://www.wikidata.org/wiki/Q5".to_owned(),
            }],
            media_controls: Vec::new(),
            image_url: None,
        });
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw expanded Wikidata properties");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("[W] ▾"));
        assert!(!rendered.contains('🧾'));
        assert!(rendered.contains("instance of (P31): human (Q5)"));
        let (_, value_area) = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| {
                action
                    == &UiAction::OpenWikidataValue("https://www.wikidata.org/wiki/Q5".to_owned())
            })
            .expect("Wikidata value hit target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: value_area.x,
                    row: value_area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::OpenWikidataValue(
                "https://www.wikidata.org/wiki/Q5".to_owned()
            ))
        );
    }

    #[test]
    fn external_link_mouse_target_excludes_surrounding_blank_cells() {
        let backend = TestBackend::new(160, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            details: Some(DetailView {
                links: vec![DetailLinkView {
                    label: "Home".to_owned(),
                    url: "https://example.org/".to_owned(),
                    ..DetailLinkView::default()
                }],
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw short external link");

        let (_, link_area) = hit_map.detail_links[0];
        assert_eq!(link_area.height, 1);
        assert_eq!(
            link_area.width,
            terminal_text_width("Home — https://example.org/"),
            "the removed marker must not leave invisible clickable columns"
        );
        assert!(
            link_area.right() < hit_map.details_panel.right(),
            "a short link must not make blank trailing panel cells clickable"
        );
        let click = |column, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(
                click(link_area.right().saturating_sub(1), link_area.y),
                &hit_map,
                &view
            ),
            Some(UiAction::ActivateDetailLink(0))
        );
        assert_eq!(
            mouse_action(
                click(link_area.x, link_area.y.saturating_sub(1)),
                &hit_map,
                &view
            ),
            Some(UiAction::SetDetailsFocus(true)),
            "the row before a link must not select it"
        );
        assert_eq!(
            mouse_action(
                click(hit_map.details_panel.right().saturating_sub(1), link_area.y),
                &hit_map,
                &view
            ),
            Some(UiAction::SetDetailsFocus(true)),
            "blank cells after a short link must only focus Details"
        );
    }

    #[test]
    fn wrapped_unicode_wikipedia_article_keeps_every_fragment_clickable() {
        let backend = TestBackend::new(56, 36);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let value = "be-tarask.wikipedia.org — Дуглас Адамз з доўгай назвай";
        let text = format!("Wikipedia articles:\n  {value}");
        let start_byte = text.find(value).expect("linked Unicode value");
        let article_url = "https://be-tarask.wikipedia.org/wiki/%D0%94%D1%83%D0%B3%D0%BB%D0%B0%D1%81_%D0%90%D0%B4%D0%B0%D0%BC%D0%B7";
        let mut view = ViewModel {
            details: Some(DetailView {
                links: vec![DetailLinkView {
                    label: "Fixture creator (Q61113)".to_owned(),
                    url: "https://www.wikidata.org/wiki/Q61113".to_owned(),
                    wikidata_item_id: Some("Q61113".to_owned()),
                    ..DetailLinkView::default()
                }],
                expanded_wikidata_item: Some("Q61113".to_owned()),
                wikidata_entities: vec![DetailWikidataEntityView {
                    item_id: "Q61113".to_owned(),
                    text,
                    value_links: vec![DetailWikidataValueLinkView {
                        start_byte,
                        end_byte: start_byte + value.len(),
                        url: article_url.to_owned(),
                    }],
                    media_controls: Vec::new(),
                    image_url: None,
                }],
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw wrapped Wikidata value");

        let value_areas = hit_map
            .detail_buttons
            .iter()
            .filter_map(|(action, area)| {
                (action == &UiAction::OpenWikidataValue(article_url.to_owned())).then_some(*area)
            })
            .collect::<Vec<_>>();
        assert!(
            value_areas.len() >= 2,
            "the fixture must wrap one Unicode value across clickable rows"
        );
        assert!(
            value_areas.windows(2).any(|areas| areas[0].y != areas[1].y),
            "wrapped fragments must occupy distinct terminal rows"
        );
        for area in &value_areas {
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: area.x,
                        row: area.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                Some(UiAction::OpenWikidataValue(article_url.to_owned()))
            );
        }

        view.text_selection_mode = true;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw Wikipedia article in Details text-selection mode");
        let article_area = hit_map
            .detail_buttons
            .iter()
            .find_map(|(action, area)| {
                (action == &UiAction::OpenWikidataValue(article_url.to_owned())).then_some(*area)
            })
            .expect("visible Wikipedia article target");
        assert!(matches!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: article_area.x,
                    row: article_area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::BeginDetailsTextSelection(_))
        ));
    }

    #[test]
    fn trash_confirmation_names_the_action_and_full_source_path() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            local_file_popup: Some(LocalFilePopupView::Trash {
                name: "06 - 500 рублей.flac".to_owned(),
                path: "/home/listener/Music/06 - 500 рублей.flac".to_owned(),
                error: None,
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw Trash confirmation");
        let rendered = rendered_text(&terminal);

        assert!(rendered.contains("Move to trash?"));
        assert!(rendered.contains("Move “06 - 500 рублей.flac”"));
        assert!(rendered.contains("From: /home/listener/Music/06 - 500 рублей.flac"));
        assert!(
            rendered
                .contains("Destination: recoverable system Trash (chosen by the operating system)")
        );
        assert!(
            hit_map
                .local_file_buttons
                .iter()
                .any(|(action, _)| action == &UiAction::ConfirmLocalTrash)
        );
    }

    #[test]
    fn downloaded_trash_confirmation_exposes_destination_and_exact_action() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let path = "/home/listener/.config/youta/downloads/offline.opus";
        let view = ViewModel {
            screen: Screen::Downloaded,
            local_file_popup: Some(LocalFilePopupView::DownloadedTrash {
                name: "offline.opus".to_owned(),
                path: path.to_owned(),
                error: None,
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw downloaded Trash confirmation");
        let rendered = rendered_text(&terminal);

        assert!(rendered.contains("Move downloaded item “offline.opus”"));
        assert!(rendered.contains(&format!("From: {path}")));
        assert!(
            rendered
                .contains("Destination: recoverable system Trash (chosen by the operating system)")
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &view),
            Some(UiAction::ConfirmDownloadedTrash)
        );
        assert!(
            hit_map
                .local_file_buttons
                .iter()
                .any(|(action, _)| action == &UiAction::ConfirmDownloadedTrash)
        );
    }

    #[test]
    fn trash_confirmation_wraps_a_long_path_on_a_narrow_terminal() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let path = "/home/listener/Music/a-long-album-directory/another-long-directory/06-500-rubles-final-master.flac";
        let view = ViewModel {
            local_file_popup: Some(LocalFilePopupView::Trash {
                name: "06-500-rubles-final-master.flac".to_owned(),
                path: path.to_owned(),
                error: None,
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw narrow Trash confirmation");
        let rendered = rendered_text(&terminal);
        let message = format!(
            "Move “06-500-rubles-final-master.flac” to recoverable system Trash?\nFrom: {path}"
        );
        let expected_lines = wrap_text_lines(&message, 70);

        assert!(rendered.contains("Move to trash?"));
        for line in expected_lines {
            assert!(
                rendered.contains(&line),
                "wrapped popup line must remain visible: {line:?}"
            );
        }
        assert!(
            hit_map
                .local_file_buttons
                .iter()
                .any(|(action, _)| action == &UiAction::ConfirmLocalTrash)
        );
    }

    #[cfg(feature = "local-move")]
    #[test]
    fn local_move_shortcuts_are_scoped_to_the_local_screen() {
        let local = ViewModel {
            screen: Screen::Local,
            ..ViewModel::default()
        };
        for (key, expected) in [
            (
                KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
                UiAction::BeginLocalMove,
            ),
            (
                KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT),
                UiAction::ExtendLocalMoveSelection(1),
            ),
            (
                KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT),
                UiAction::ExtendLocalMoveSelection(-1),
            ),
        ] {
            assert_eq!(key_action(key, &local), Some(expected));
        }

        let search = ViewModel::default();
        assert_ne!(
            key_action(
                KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
                &search,
            ),
            Some(UiAction::BeginLocalMove)
        );
        assert_ne!(
            key_action(
                KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT),
                &search,
            ),
            Some(UiAction::ExtendLocalMoveSelection(1))
        );
        assert_ne!(
            key_action(
                KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT),
                &search,
            ),
            Some(UiAction::ExtendLocalMoveSelection(-1))
        );
    }

    #[cfg(feature = "local-move")]
    #[test]
    fn local_move_popup_renders_and_exposes_keyboard_and_mouse_controls() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            screen: Screen::Local,
            local_file_popup: Some(LocalFilePopupView::Move {
                source_names: vec!["first.flac".to_owned(), "Album".to_owned()],
                destination: "/home/listener/Music".to_owned(),
                directories: vec![
                    LocalMoveDestinationView {
                        name: "..".to_owned(),
                        path: "/home/listener".to_owned(),
                    },
                    LocalMoveDestinationView {
                        name: "Archive".to_owned(),
                        path: "/home/listener/Music/Archive".to_owned(),
                    },
                ],
                selected: 1,
                pending: false,
                error: Some("destination already contains first.flac".to_owned()),
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw Move destination popup");
        let rendered = rendered_text(&terminal);
        for expected in [
            "Move",
            "Moving 2 entries: first.flac, Album",
            "Destination:",
            "/home/listener/Music",
            "Archive",
            "destination already contains first.flac",
            "[Enter] Open folder",
            "[M] Move here",
            "[Esc] Cancel",
        ] {
            assert!(
                rendered.contains(expected),
                "Move popup omitted {expected:?}:\n{rendered}"
            );
        }

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &view),
            Some(UiAction::ActivateLocalMoveDestination)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT),
                &view,
            ),
            Some(UiAction::ConfirmLocalMoveHere)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &view),
            Some(UiAction::MoveLocalMoveDestination(1))
        );

        let selected_row = Rect::new(
            hit_map.local_move_rows.x,
            hit_map.local_move_rows.y.saturating_add(1),
            1,
            1,
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: selected_row.x,
                    row: selected_row.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SelectLocalMoveDestination(
                hit_map.local_move_first_index.saturating_add(1)
            ))
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    column: selected_row.x,
                    row: selected_row.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::MoveLocalMoveDestination(-1))
        );
    }

    #[test]
    fn channel_panel_renders_description_wikidata_and_external_links() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            right_panel_mode: RightPanelMode::Channel,
            details: Some(DetailView {
                title: "Mock channel".to_owned(),
                source: "Bilibili channel".to_owned(),
                channel_subscriber_count: Some(13_045),
                channel_video_count: Some(412),
                channel_total_view_count: Some(987_654_321),
                channel_created: "2018 May 4".to_owned(),
                channel_country: "Georgia".to_owned(),
                channel_webpage_url: Some(
                    url::Url::parse("https://www.youtube.com/channel/UCfixture")
                        .expect("fixture channel URL"),
                ),
                length: "must not render".to_owned(),
                likes: "must not render".to_owned(),
                views: "must not render".to_owned(),
                description: "Full channel description".to_owned(),
                wikidata: "Douglas Adams (Q42): https://www.wikidata.org/wiki/Q42".to_owned(),
                links: vec![
                    DetailLinkView {
                        label: "Telegram: Fixture".to_owned(),
                        url: "https://t.me/fixture".to_owned(),
                        wikidata_item_id: None,
                        ..DetailLinkView::default()
                    },
                    DetailLinkView {
                        label: "Douglas Adams (Q42)".to_owned(),
                        url: "https://www.wikidata.org/wiki/Q42".to_owned(),
                        wikidata_item_id: Some("Q42".to_owned()),
                        ..DetailLinkView::default()
                    },
                ],
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render(frame, &view, &UiSettings::default(), &mut hit_map);
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("Mock channel"));
        assert!(rendered.contains("Subscribers: 13,045"));
        assert!(rendered.contains("Joined: 2018 May 4"));
        assert!(rendered.contains("Videos: 412"));
        assert!(rendered.contains("Total views: 987,654,321"));
        assert!(rendered.contains("Country: Georgia"));
        assert!(!rendered.contains("Length:"));
        assert!(!rendered.contains("Likes:"));
        assert!(!rendered.contains("Views:"));
        assert!(rendered.contains(&format!(
            "[O] {} channel · Mock channel",
            system_url_opener_name()
        )));
        assert!(!rendered.contains("Load channel info"));
        assert!(rendered.contains("Full channel description"));
        assert!(rendered.contains("Douglas Adams (Q42)"));
        assert_eq!(rendered.matches("Douglas Adams (Q42)").count(), 1);
        assert!(!rendered.contains("Wikidata:"));
        assert!(rendered.contains("Telegram: Fixture"));
        assert_eq!(hit_map.detail_links.len(), 2);
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .all(|(action, _)| action != &UiAction::ShowChannel)
        );
    }

    #[test]
    fn channel_panel_clears_trailing_cells_when_external_links_shrink() {
        use ratatui::backend::Backend;

        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            right_panel_mode: RightPanelMode::Channel,
            details: Some(DetailView {
                title: "First channel".to_owned(),
                channel_id: "UCfirst".to_owned(),
                description: concat!(
                    "Long first-channel description\n",
                    "🔴 contact contact contact semaha_help\n",
                    "🔴 contact contact contact OGMz"
                )
                .to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw first channel");

        // Some terminals measure the leading emoji differently from Ratatui.
        // Model the resulting out-of-sync physical cells without changing
        // Ratatui's previous-frame buffer.
        let stale_cells = {
            let buffer = terminal.backend().buffer();
            ["semaha_help", "OGMz"].map(|ending| {
                let final_symbol = &ending[ending.len() - 1..];
                let (x, y) = (0..buffer.area.height)
                    .find_map(|y| {
                        let row = (0..buffer.area.width)
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>();
                        row.contains(ending).then(|| {
                            let x = (0..buffer.area.width)
                                .rev()
                                .find(|x| buffer[(*x, y)].symbol() == final_symbol)
                                .expect("old line-ending cell");
                            (x.saturating_add(1), y)
                        })
                    })
                    .expect("old emoji-prefixed description line");
                assert_eq!(buffer[(x, y)].symbol(), " ");
                (x, y, final_symbol)
            })
        };
        for (x, y, symbol) in stale_cells {
            let mut cell = ratatui::buffer::Cell::default();
            cell.set_symbol(symbol);
            terminal
                .backend_mut()
                .draw(std::iter::once((x, y, &cell)))
                .expect("inject terminal-width divergence");
        }

        view.details = Some(DetailView {
            title: "Second channel".to_owned(),
            channel_id: "UCsecond".to_owned(),
            description: "Short description".to_owned(),
            links: vec![
                DetailLinkView {
                    label: "X/Twitter".to_owned(),
                    url: "https://x.co/x".to_owned(),
                    ..DetailLinkView::default()
                },
                DetailLinkView {
                    label: "TikTok".to_owned(),
                    url: "https://t.co/z".to_owned(),
                    ..DetailLinkView::default()
                },
            ],
            ..DetailView::default()
        });
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw second channel");

        let buffer = terminal.backend().buffer();
        for (x, y, _) in stale_cells {
            assert_eq!(
                buffer[(x, y)].symbol(),
                " ",
                "the shorter channel must clear cells left outside Ratatui's prior model"
            );
        }
        assert!(!rendered_text(&terminal).contains("semaha_help"));
        assert!(!rendered_text(&terminal).contains("OGMz"));

        let unchanged_frame = terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("redraw unchanged second channel");
        let panel = hit_map.details_panel;
        assert!(
            (panel.top()..panel.bottom()).all(|y| {
                (panel.left()..panel.right()).all(|x| {
                    unchanged_frame.buffer[(x, y)].diff_option != CellDiffOption::AlwaysUpdate
                })
            }),
            "pane invalidation must not remain active after the owner-change frame"
        );
    }

    #[test]
    fn channel_panel_omits_unavailable_subscriber_count() {
        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            right_panel_mode: RightPanelMode::Channel,
            details: Some(DetailView {
                title: "Hidden statistics".to_owned(),
                description: "Channel description".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw channel without public subscribers");

        assert!(!rendered_text(&terminal).contains("Subscribers:"));
    }

    #[test]
    fn video_details_keep_public_counts_in_requested_order() {
        let backend = TestBackend::new(160, 18);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            right_panel_mode: RightPanelMode::Details,
            details: Some(DetailView {
                title: "Mock video".to_owned(),
                length: "4:05".to_owned(),
                likes: "13,045".to_owned(),
                views: "887,263".to_owned(),
                comments: "20".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw video details");
        let rendered = rendered_text(&terminal);

        assert!(rendered.contains("Length: 4:05  Views: 887,263  Likes: 13,045  Comments: 20"));
        assert!(!rendered.contains("Load channel info"));
    }

    #[cfg(feature = "qr")]
    #[test]
    fn youtube_qr_shortcut_is_help_only_and_scoped_to_selected_video() {
        let youtube = ViewModel {
            details: Some(DetailView {
                media_id: Some(MediaId::new(SourceKind::YouTube, "dQw4w9WgXcQ")),
                title: "Fixture video".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT),
                &youtube,
            ),
            Some(UiAction::OpenVideoQr),
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                &youtube
            ),
            Some(UiAction::Quit),
            "the new shortcut must not replace lowercase q quit"
        );
        for unsupported in [
            ViewModel::default(),
            ViewModel {
                details: Some(DetailView {
                    title: "YouTube channel without a selected video".to_owned(),
                    ..DetailView::default()
                }),
                ..ViewModel::default()
            },
            ViewModel {
                details: Some(DetailView {
                    media_id: Some(MediaId::new(SourceKind::Local, "/music/local.flac")),
                    ..DetailView::default()
                }),
                ..ViewModel::default()
            },
        ] {
            assert_eq!(
                key_action(
                    KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT),
                    &unsupported,
                ),
                None,
                "the QR shortcut must require an exact selected YouTube video"
            );
        }

        let mut terminal =
            Terminal::new(TestBackend::new(160, 18)).expect("YouTube QR fixture terminal");
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &youtube, &UiSettings::default(), &mut hit_map))
            .expect("draw YouTube details without a visible QR control");
        let rendered = rendered_text(&terminal);
        assert!(!rendered.contains("Show QR"));
        assert!(!rendered.contains("[Q]"));
        assert!(!rendered.contains("QR code"));
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .all(|(action, _)| action != &UiAction::OpenVideoQr)
        );
        assert!(
            hit_map
                .buttons
                .iter()
                .all(|(action, _)| action != &UiAction::OpenVideoQr)
        );
        assert_minimal_footer_actions(&hit_map);
    }

    #[cfg(feature = "qr")]
    #[test]
    fn youtube_qr_popup_renders_conventional_modules_and_captures_input() {
        let matrix = QrMatrix::encode("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
            .expect("canonical YouTube URL QR");
        let symbol_modules = matrix.width() + QR_QUIET_ZONE_MODULES * 2;
        let popup = VideoQrPopupView {
            video_id: "dQw4w9WgXcQ".to_owned(),
            video_title: "Fixture video".to_owned(),
            url: "https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_owned(),
            matrix,
        };
        let view = ViewModel {
            video_qr_popup: Some(popup),
            ..ViewModel::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("QR terminal");
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw QR popup");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("YouTube video QR"));
        assert!(rendered.contains("[Esc] Close"));
        assert!(rendered.contains('█') || rendered.contains('▀') || rendered.contains('▄'));

        let required_width = u16::try_from(symbol_modules).expect("fixture QR width") + 2;
        let required_height =
            u16::try_from(symbol_modules.div_ceil(2)).expect("fixture QR height") + 3;
        let popup_area =
            centered_sized_rect(required_width, required_height, Rect::new(0, 0, 80, 24));
        let quiet_zone_cell = &terminal.backend().buffer()[(popup_area.x + 1, popup_area.y + 1)];
        assert_eq!(quiet_zone_cell.symbol(), " ");
        assert_eq!(quiet_zone_cell.bg, Color::White);
        let finder_cell = &terminal.backend().buffer()[(
            popup_area.x + 1 + u16::try_from(QR_QUIET_ZONE_MODULES).expect("quiet zone"),
            popup_area.y + 1 + u16::try_from(QR_QUIET_ZONE_MODULES / 2).expect("finder row"),
        )];
        assert_eq!(finder_cell.fg, Color::Black);
        assert_eq!(finder_cell.bg, Color::White);
        assert_ne!(finder_cell.symbol(), " ");

        for key in [KeyCode::Esc, KeyCode::Char('Q')] {
            assert_eq!(
                key_action(KeyEvent::new(key, KeyModifiers::NONE), &view),
                Some(UiAction::DismissVideoQr)
            );
        }
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &view),
            None,
            "lowercase quit must not leak through the modal"
        );
        let close_area = hit_map
            .video_qr_buttons
            .iter()
            .find_map(|(action, area)| (action == &UiAction::DismissVideoQr).then_some(*area))
            .expect("QR close button");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: close_area.x,
                    row: close_area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::DismissVideoQr)
        );
        let (_, underlying_tab) = hit_map.tabs.first().expect("underlying tab");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: underlying_tab.x,
                    row: underlying_tab.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            None,
            "the QR modal must capture clicks over underlying controls"
        );
    }

    #[cfg(feature = "qr")]
    #[test]
    fn youtube_qr_popup_suppresses_the_virtual_pointer_overlay() {
        let view = ViewModel {
            video_qr_popup: Some(VideoQrPopupView {
                video_id: "dQw4w9WgXcQ".to_owned(),
                video_title: "Fixture video".to_owned(),
                url: "https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_owned(),
                matrix: QrMatrix::encode("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
                    .expect("canonical YouTube URL QR"),
            }),
            ..ViewModel::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("QR terminal");
        let mut hit_map = HitMap::default();
        let mut virtual_cursor = VirtualCursor {
            active: true,
            column: 40,
            row: 12,
            bounds: Rect::new(0, 0, 80, 24),
            ..VirtualCursor::default()
        };

        terminal
            .draw(|frame| {
                render_frame(frame, &view, &UiSettings::default(), &mut hit_map, None);
                render_virtual_cursor_overlay(frame, &view, &mut virtual_cursor);
            })
            .expect("draw QR popup with an active virtual pointer");

        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| cell.symbol() != "■"),
            "the virtual pointer must not overwrite a QR module"
        );
    }

    #[cfg(feature = "qr")]
    #[test]
    fn youtube_qr_popup_requests_resize_instead_of_clipping_the_symbol() {
        let view = ViewModel {
            video_qr_popup: Some(VideoQrPopupView {
                video_id: "dQw4w9WgXcQ".to_owned(),
                video_title: "Fixture video".to_owned(),
                url: "https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_owned(),
                matrix: QrMatrix::encode("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
                    .expect("canonical YouTube URL QR"),
            }),
            ..ViewModel::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).expect("small terminal");
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw QR resize fallback");

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Terminal is too small"));
        assert!(rendered.contains("Resize to at least"));
        assert!(
            hit_map
                .video_qr_buttons
                .iter()
                .any(|(action, _)| action == &UiAction::DismissVideoQr)
        );
    }

    #[cfg(not(feature = "qr"))]
    #[test]
    fn no_qr_build_omits_the_youtube_shortcut_and_help_entry() {
        let youtube = ViewModel {
            details: Some(DetailView {
                media_id: Some(MediaId::new(SourceKind::YouTube, "dQw4w9WgXcQ")),
                title: "Fixture video".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT),
                &youtube,
            ),
            None,
            "a build without QR support must leave uppercase Q unbound"
        );

        let help = ViewModel {
            help_open: true,
            ..ViewModel::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).expect("help terminal");
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &help, &UiSettings::default(), &mut hit_map))
            .expect("draw help without QR support");
        assert!(!rendered_text(&terminal).contains("YouTube video QR code"));
    }

    #[test]
    fn comments_control_and_f6_require_supported_youtube_details() {
        let youtube = ViewModel {
            video_comments_available: true,
            details: Some(DetailView {
                media_id: Some(MediaId::new(SourceKind::YouTube, "dQw4w9WgXcQ")),
                title: "Fixture video".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE), &youtube),
            Some(UiAction::OpenVideoComments)
        );
        let mut youtube_terminal =
            Terminal::new(TestBackend::new(160, 18)).expect("YouTube terminal");
        let mut youtube_hit_map = HitMap::default();
        youtube_terminal
            .draw(|frame| {
                render(
                    frame,
                    &youtube,
                    &UiSettings::default(),
                    &mut youtube_hit_map,
                );
            })
            .expect("draw supported YouTube comments control");
        assert!(rendered_text(&youtube_terminal).contains("[F6] Twenty comments"));
        let comments_target = youtube_hit_map
            .detail_buttons
            .iter()
            .find_map(|(action, target)| {
                (action == &UiAction::OpenVideoComments).then_some(*target)
            })
            .expect("Twenty comments hit target");
        assert_eq!(
            comments_target.width,
            terminal_text_width("[F6] Twenty comments"),
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: comments_target.x,
                    row: comments_target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &youtube_hit_map,
                &youtube,
            ),
            Some(UiAction::OpenVideoComments),
            "the renamed label must retain its exact mouse action"
        );

        let local = ViewModel {
            details: Some(DetailView {
                media_id: Some(MediaId::new(SourceKind::Local, "/music/fixture.flac")),
                ..youtube.details.clone().expect("YouTube details")
            }),
            ..youtube.clone()
        };
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE), &local),
            None,
            "provider capability alone must not expose comments for non-YouTube media"
        );
        let mut local_terminal = Terminal::new(TestBackend::new(160, 18)).expect("Local terminal");
        let mut local_hit_map = HitMap::default();
        local_terminal
            .draw(|frame| render(frame, &local, &UiSettings::default(), &mut local_hit_map))
            .expect("draw unsupported Local comments control");
        assert!(!rendered_text(&local_terminal).contains("[F6] Twenty comments"));
        assert!(
            local_hit_map
                .detail_buttons
                .iter()
                .all(|(action, _)| action != &UiAction::OpenVideoComments)
        );

        let unsupported_youtube = ViewModel {
            video_comments_available: false,
            ..youtube
        };
        let mut unsupported_terminal =
            Terminal::new(TestBackend::new(160, 18)).expect("unsupported YouTube terminal");
        let mut unsupported_hit_map = HitMap::default();
        unsupported_terminal
            .draw(|frame| {
                render(
                    frame,
                    &unsupported_youtube,
                    &UiSettings::default(),
                    &mut unsupported_hit_map,
                );
            })
            .expect("draw unsupported YouTube comments control");
        assert!(!rendered_text(&unsupported_terminal).contains("[F6] Twenty comments"));
        assert!(
            unsupported_hit_map
                .detail_buttons
                .iter()
                .all(|(action, _)| action != &UiAction::OpenVideoComments)
        );
    }

    #[test]
    fn comments_popup_scrolls_with_bounded_keyboard_mouse_and_close_controls() {
        let comments = (0..20)
            .map(|index| VideoCommentView {
                author_name: format!("Author {index}"),
                like_count: u64::try_from(index).expect("fixture index"),
                published: Some("2026 July 30".to_owned()),
                text: format!(
                    "Comment {index} contains enough plain text to wrap across several terminal rows."
                ),
            })
            .collect();
        let view = ViewModel {
            video_comments_popup: Some(VideoCommentsPopupView {
                video_id: "dQw4w9WgXcQ".to_owned(),
                video_title: "Fixture video".to_owned(),
                state: VideoCommentsPopupState::Ready,
                comments,
                scroll_offset: usize::MAX,
            }),
            ..ViewModel::default()
        };
        let backend = TestBackend::new(72, 22);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw comments popup");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("YouTube comments"));
        assert!(rendered.contains("[Esc] Close"));
        assert!(
            !rendered.contains("20 comments"),
            "the fixed popup limit is already advertised by the F6 control"
        );
        assert!(
            !rendered.contains("Lines 1–") && !rendered.contains("lines 1–"),
            "the scrollbar already communicates the visible comments viewport"
        );
        assert!(hit_map.video_comments_scroll_maximum > 0);
        assert_eq!(
            hit_map.video_comments_scroll_offset, hit_map.video_comments_scroll_maximum,
            "renderer must clamp an oversized restored offset"
        );
        let scrollbar_x = hit_map.video_comments_text_area.right();
        let scrollbar_bottom = hit_map.video_comments_text_area.bottom().saturating_sub(1);
        assert_eq!(
            terminal.backend().buffer()[(scrollbar_x, scrollbar_bottom)].symbol(),
            "█",
            "the comments scrollbar thumb must reach the bottom at the final content offset"
        );

        assert_eq!(
            video_comments_key_action(
                KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
                hit_map.video_comments_scroll_offset,
                hit_map.video_comments_scroll_maximum,
                hit_map.video_comments_page_lines,
            ),
            Some(UiAction::SetVideoCommentsScroll(
                hit_map
                    .video_comments_scroll_offset
                    .saturating_sub(hit_map.video_comments_page_lines)
            ))
        );
        assert_eq!(
            video_comments_key_action(
                KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
                hit_map.video_comments_scroll_offset,
                hit_map.video_comments_scroll_maximum,
                hit_map.video_comments_page_lines,
            ),
            Some(UiAction::SetVideoCommentsScroll(0))
        );
        assert_eq!(
            video_comments_key_action(
                KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
                0,
                hit_map.video_comments_scroll_maximum,
                hit_map.video_comments_page_lines,
            ),
            Some(UiAction::SetVideoCommentsScroll(
                hit_map.video_comments_scroll_maximum
            ))
        );

        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    column: hit_map.video_comments_text_area.x,
                    row: hit_map.video_comments_text_area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SetVideoCommentsScroll(
                hit_map.video_comments_scroll_offset.saturating_sub(3)
            ))
        );
        let close_area = hit_map
            .video_comments_buttons
            .iter()
            .find_map(|(action, area)| (action == &UiAction::DismissVideoComments).then_some(*area))
            .expect("comments close button");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: close_area.x,
                    row: close_area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::DismissVideoComments)
        );
    }

    #[test]
    fn project_history_popup_preserves_full_messages_provenance_and_scroll_controls() {
        let current_hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned();
        let commits = (0..10)
            .map(|index| ProjectCommitView {
                hash: if index == 0 {
                    current_hash.clone()
                } else {
                    format!("{index:040x}")
                },
                committed_at: format!("2026-07-{:02}T12:34:56+04:00", 31 - index),
                message: format!(
                    "Commit title {index}\n\nComplete explanatory body {index}.\n\nCo-authored-by: OpenAI ChatGPT <noreply@openai.com>"
                ),
            })
            .collect();
        let view = ViewModel {
            project_history_popup: Some(ProjectHistoryPopupView {
                commits,
                current_hash: Some(current_hash),
                installation: "Portage binary package (media-sound/youta-bin)".to_owned(),
                executable_path: "/usr/bin/youta".to_owned(),
                started_in: "/home/alice/Music".to_owned(),
                build_source: None,
                remote_state: ProjectHistoryRemoteState::Updated,
                scroll_offset: 0,
            }),
            ..ViewModel::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(92, 30)).expect("terminal");
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw project history popup");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Recent Youta commits"));
        assert!(rendered.contains("Portage binary package"));
        assert!(rendered.contains("Executable: /usr/bin/youta"));
        assert!(rendered.contains("Started in: /home/alice/Music"));
        assert!(rendered.contains("aaaaaaaaaaaa · 2026-07-31 · current version"));
        assert!(rendered.contains("Complete explanatory body 0."));
        assert!(rendered.contains("Co-authored-by: OpenAI ChatGPT"));
        assert!(rendered.contains("newer commits are cached in RAM"));
        assert!(hit_map.project_history_scroll_maximum > 0);
        assert_eq!(
            project_history_key_action(
                KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
                0,
                hit_map.project_history_scroll_maximum,
                hit_map.project_history_page_lines,
            ),
            Some(UiAction::SetProjectHistoryScroll(
                hit_map.project_history_scroll_maximum
            ))
        );
        assert_eq!(
            project_history_key_action(
                KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE),
                0,
                hit_map.project_history_scroll_maximum,
                hit_map.project_history_page_lines,
            ),
            Some(UiAction::DismissProjectHistory)
        );
        let close_area = hit_map
            .project_history_buttons
            .iter()
            .find_map(|(action, area)| {
                (action == &UiAction::DismissProjectHistory).then_some(*area)
            })
            .expect("project history close button");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: close_area.x,
                    row: close_area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::DismissProjectHistory)
        );
    }

    #[test]
    fn project_history_hotkey_is_global_but_documented_only_in_help() {
        let view = ViewModel::default();
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE), &view),
            Some(UiAction::OpenProjectHistory)
        );
        let mut help = view.clone();
        help.help_open = true;
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE), &help),
            Some(UiAction::OpenProjectHistory)
        );

        let mut terminal = Terminal::new(TestBackend::new(240, 1)).expect("terminal");
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw footer");
        assert!(!rendered_text(&terminal).contains("F9"));
    }

    #[test]
    fn show_all_files_hotkey_is_scoped_to_local() {
        let local = ViewModel {
            screen: Screen::Local,
            ..ViewModel::default()
        };
        for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
            assert_eq!(
                key_action(KeyEvent::new(KeyCode::Char('H'), modifiers), &local),
                Some(UiAction::ToggleLocalAllFiles)
            );
        }

        let youtube = ViewModel {
            screen: Screen::Search,
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('H'), KeyModifiers::SHIFT),
                &youtube
            ),
            None,
            "the Local visibility shortcut must not leak to other sources"
        );
    }

    #[test]
    fn local_details_expose_the_current_file_visibility_toggle() {
        for (show_all_local_files, expected, unexpected, highlighted) in [
            (false, "[H] Media files only", "[H] Show all files", false),
            (true, "[H] Show all files", "[H] Media files only", true),
        ] {
            let backend = TestBackend::new(100, 18);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let view = ViewModel {
                screen: Screen::Local,
                show_all_local_files,
                details: Some(DetailView {
                    title: "notes.txt".to_owned(),
                    source: "Local text".to_owned(),
                    description: "Full path: /music/notes.txt".to_owned(),
                    ..DetailView::default()
                }),
                ..ViewModel::default()
            };
            let mut hit_map = HitMap::default();

            terminal
                .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
                .expect("draw Local text details");
            let rendered = rendered_text(&terminal);

            assert!(rendered.contains(expected));
            assert!(!rendered.contains(unexpected));
            let target = hit_map
                .detail_buttons
                .iter()
                .find_map(|(action, target)| {
                    (action == &UiAction::ToggleLocalAllFiles).then_some(*target)
                })
                .expect("Local file-visibility control hit target");
            let label_cell = &terminal.backend().buffer()[(target.x, target.y)];
            if highlighted {
                assert_eq!(label_cell.fg, Color::Black);
                assert_eq!(label_cell.bg, Color::Cyan);
                assert!(label_cell.modifier.contains(Modifier::BOLD));
            } else {
                assert_eq!(label_cell.fg, Color::Cyan);
                assert_eq!(label_cell.bg, Color::Reset);
                assert!(!label_cell.modifier.contains(Modifier::BOLD));
            }
        }
    }

    #[test]
    fn local_details_offer_move_for_files_and_folders_but_not_parent_navigation() {
        for (title, requested_rename, requested_move, requested_trash) in [
            ("Album", false, true, true),
            ("01 - Track.flac", true, true, true),
            ("..", false, false, false),
        ] {
            let renamable = requested_rename && cfg!(feature = "local-rename");
            let movable = requested_move && cfg!(feature = "local-move");
            let trashable = requested_trash && cfg!(feature = "local-trash");
            let backend = TestBackend::new(100, 18);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let view = ViewModel {
                screen: Screen::Local,
                details: Some(DetailView {
                    title: title.to_owned(),
                    source: "Local folder".to_owned(),
                    description: format!("Full path: /music/{title}"),
                    local_renamable: requested_rename,
                    local_movable: requested_move,
                    local_trashable: requested_trash,
                    ..DetailView::default()
                }),
                ..ViewModel::default()
            };
            let mut hit_map = HitMap::default();

            terminal
                .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
                .expect("draw Local entry details");
            let rendered = rendered_text(&terminal);
            assert_eq!(rendered.contains("[r] Rename"), renamable);
            assert_eq!(
                rendered.matches("[m] Move").count(),
                usize::from(movable),
                "only movable Local Details entries expose the Move action"
            );
            assert_eq!(rendered.contains("[Delete] Move to Trash"), trashable);

            let rename_area = hit_map.detail_buttons.iter().find_map(|(action, area)| {
                (action == &UiAction::BeginLocalRename).then_some(*area)
            });
            let move_area = hit_map
                .detail_buttons
                .iter()
                .find_map(|(action, area)| (action == &UiAction::BeginLocalMove).then_some(*area));
            let trash_area = hit_map.detail_buttons.iter().find_map(|(action, area)| {
                (action == &UiAction::RequestLocalTrash).then_some(*area)
            });
            assert_eq!(rename_area.is_some(), renamable);
            assert_eq!(move_area.is_some(), movable);
            assert_eq!(trash_area.is_some(), trashable);

            if let Some(move_area) = move_area {
                assert_eq!(move_area.width, terminal_text_width("[m] Move"));
                assert_eq!(move_area.y, hit_map.details_panel.y);
                assert_eq!(
                    move_area.right(),
                    hit_map.details_panel.right(),
                    "Move must be right-aligned"
                );
                if let Some(trash_area) = trash_area {
                    assert_eq!(trash_area.y, move_area.y.saturating_add(1));
                    assert_eq!(
                        trash_area.right(),
                        hit_map.details_panel.right(),
                        "Move to Trash must be right-aligned"
                    );
                    if let Some(rename_area) = rename_area {
                        assert_eq!(
                            rename_area.y, move_area.y,
                            "Rename should reuse the free space before Move"
                        );
                        assert!(
                            rename_area.right().saturating_add(2) <= move_area.x,
                            "paired Local controls must retain a two-cell gap"
                        );
                    }
                }
                let click = |column| MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row: move_area.y,
                    modifiers: KeyModifiers::NONE,
                };
                assert_eq!(
                    mouse_action(click(move_area.x), &hit_map, &view),
                    Some(UiAction::BeginLocalMove)
                );
                assert_eq!(
                    mouse_action(click(move_area.right().saturating_sub(1)), &hit_map, &view,),
                    Some(UiAction::BeginLocalMove)
                );
                assert_ne!(
                    mouse_action(click(move_area.right()), &hit_map, &view),
                    Some(UiAction::BeginLocalMove),
                    "the Move target must not extend beyond its rendered label"
                );
            }
        }
    }

    #[test]
    fn local_audio_fingerprint_action_is_scoped_clickable_and_animated() {
        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            screen: Screen::Local,
            local_fingerprint_animation_frame: 1,
            details: Some(DetailView {
                title: "01 - Track.flac".to_owned(),
                source: "Local audio".to_owned(),
                description: "Full path: /music/01 - Track.flac".to_owned(),
                local_fingerprint_available: true,
                local_fingerprint_pending: true,
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw Local audio fingerprint action");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("[f] Fingerprinting /"));
        let fingerprint_area = hit_map
            .detail_buttons
            .iter()
            .find_map(|(action, area)| {
                (action == &UiAction::FingerprintLocalAudio).then_some(*area)
            })
            .expect("fingerprint control");
        assert_eq!(fingerprint_area.right(), hit_map.details_panel.right());
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE), &view),
            Some(UiAction::FingerprintLocalAudio)
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: fingerprint_area.x,
                    row: fingerprint_area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::FingerprintLocalAudio)
        );

        let details = view.details.as_mut().expect("Local audio details");
        details.local_fingerprint_available = false;
        details.local_fingerprint_pending = false;
        details.links.push(DetailLinkView {
            label: "MusicBrainz recording 1".to_owned(),
            url: "https://musicbrainz.org/recording/11111111-1111-4111-8111-111111111111"
                .to_owned(),
            wikidata_item_id: None,
            ..DetailLinkView::default()
        });
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw fingerprinted Local audio details");
        let rendered = rendered_text(&terminal);
        assert!(!rendered.contains("[f] Fingerprint"));
        assert!(rendered.contains("MusicBrainz recording 1"));
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .all(|(action, _)| action != &UiAction::FingerprintLocalAudio)
        );
        assert_ne!(
            key_action(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE), &view),
            Some(UiAction::FingerprintLocalAudio)
        );

        let folder = ViewModel {
            screen: Screen::Local,
            details: Some(DetailView {
                title: "Album".to_owned(),
                source: "Local folder".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        assert_ne!(
            key_action(
                KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
                &folder
            ),
            Some(UiAction::FingerprintLocalAudio)
        );
    }

    #[test]
    fn local_fingerprint_lastfm_description_joins_the_scrollable_details_body() {
        let backend = TestBackend::new(110, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            screen: Screen::Local,
            details: Some(DetailView {
                title: "Track.flac".to_owned(),
                source: "Local audio".to_owned(),
                description: "Full path: /music/Track.flac".to_owned(),
                lastfm_artist_description: concat!(
                    "самая конфликтная, самая нищебродская и самая сексистская группа.\n",
                    "новейший дип-хоп - местами абстракт хип-хоп\n",
                    "калька на весь бомонд авангардного хип-хопа с примесью женской страдальческой эстетики"
                )
                .to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw Last.fm artist description");

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Full path: /music/Track.flac"));
        assert!(rendered.contains("Last.fm artist description:"));
        assert!(rendered.contains("самая конфликтная"));
        assert!(hit_map.details_scroll_maximum > 0);
    }

    #[cfg(feature = "local-trash")]
    #[test]
    fn downloaded_details_offer_top_right_recoverable_removal() {
        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            screen: Screen::Downloaded,
            rows: vec![RowView {
                title: "offline.opus".to_owned(),
                ..RowView::default()
            }],
            details: Some(DetailView {
                title: "offline.opus".to_owned(),
                source: "Local download".to_owned(),
                description: "Full path: /downloads/offline.opus".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw Downloaded Details");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("[x] Move to Trash"));
        let trash_area = hit_map
            .detail_buttons
            .iter()
            .find_map(|(action, area)| {
                (action == &UiAction::RequestDownloadedTrash).then_some(*area)
            })
            .expect("Downloaded Trash control");
        assert_eq!(trash_area.y, hit_map.details_panel.y);
        assert_eq!(trash_area.right(), hit_map.details_panel.right());
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), &view),
            Some(UiAction::RequestDownloadedTrash)
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: trash_area.x,
                    row: trash_area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::RequestDownloadedTrash)
        );

        let local = ViewModel {
            screen: Screen::Local,
            ..ViewModel::default()
        };
        assert_ne!(
            key_action(
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                &local
            ),
            Some(UiAction::RequestDownloadedTrash),
            "the x shortcut must remain scoped to Downloaded"
        );
    }

    #[test]
    fn mouse_seek_maps_horizontal_position_to_percent() {
        let view = ViewModel {
            playback: PlaybackStatus {
                idle: false,
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };
        let hit_map = HitMap {
            seek_bar: Rect::new(10, 20, 101, 2),
            now_playing: Some(Rect::new(10, 21, 20, 1)),
            ..HitMap::default()
        };
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 60,
            row: 20,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(mouse, &hit_map, &view),
            Some(UiAction::SeekPercent(50.0))
        );
    }

    #[test]
    fn idle_playback_ignores_stale_keyboard_and_mouse_seek_targets() {
        let view = ViewModel::default();
        for code in [
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Char('5'),
            KeyCode::Char('['),
            KeyCode::Char(']'),
        ] {
            assert_eq!(
                key_action(KeyEvent::new(code, KeyModifiers::NONE), &view),
                None
            );
        }
        let hit_map = HitMap {
            seek_bar: Rect::new(10, 20, 101, 1),
            seek_markers: vec![(UiAction::SeekPercent(25.0), Rect::new(35, 19, 1, 1))],
            ..HitMap::default()
        };
        for (column, row) in [(35, 19), (60, 20)] {
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column,
                        row,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                None
            );
        }
    }

    #[test]
    fn live_playback_ignores_stale_mouse_seek_targets() {
        let mut view = ViewModel {
            playback: PlaybackStatus {
                idle: false,
                live: true,
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };
        let hit_map = HitMap {
            seek_bar: Rect::new(10, 20, 101, 1),
            seek_markers: vec![(UiAction::SeekPercent(25.0), Rect::new(35, 19, 1, 1))],
            ..HitMap::default()
        };

        for (column, row) in [(35, 19), (60, 20)] {
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column,
                        row,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                None
            );
        }

        view.playback.live_seekable_range = Some(crate::playback::BufferedRange {
            start: Duration::from_secs(100),
            end: Duration::from_secs(200),
        });
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 60,
                    row: 20,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SeekPercent(50.0))
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 35,
                    row: 19,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            None,
            "live streams never expose finite-media chapter markers"
        );
    }

    #[test]
    fn mouse_click_on_tracker_button_opens_tracker_screen() {
        let view = ViewModel::default();
        let hit_map = HitMap {
            buttons: vec![(
                UiAction::ShowScreen(Screen::TrackerMusic),
                Rect::new(20, 30, 21, 1),
            )],
            now_playing: Some(Rect::new(20, 30, 21, 1)),
            ..HitMap::default()
        };
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 25,
            row: 30,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(mouse, &hit_map, &view),
            Some(UiAction::ShowScreen(Screen::TrackerMusic)),
            "button targets must retain priority over the now-playing target"
        );
    }

    #[test]
    fn top_tabs_follow_expected_order_with_exact_click_targets() {
        let backend = TestBackend::new(180, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel::default();
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| {
                render_tabs(frame, frame.area(), &view, &Theme::new(false), &mut hit_map);
            })
            .expect("draw tabs");

        let screens = hit_map
            .tabs
            .iter()
            .map(|(screen, _)| *screen)
            .collect::<Vec<_>>();
        assert_eq!(
            screens,
            vec![
                Screen::Search,
                #[cfg(feature = "youtube-music")]
                Screen::YouTubeMusic,
                #[cfg(feature = "yandex-music")]
                Screen::YandexMusic,
                #[cfg(feature = "bandcamp")]
                Screen::Bandcamp,
                #[cfg(feature = "apple-podcasts")]
                Screen::ApplePodcasts,
                #[cfg(feature = "librivox")]
                Screen::LibriVox,
                #[cfg(feature = "radio")]
                Screen::Radio,
                Screen::TrackerMusic,
                Screen::Subscriptions,
                Screen::Local,
                Screen::Playlists,
                Screen::Downloaded,
                Screen::History,
                Screen::Statistics,
            ]
        );
        for (screen, area) in &hit_map.tabs {
            for column in [area.x, area.right().saturating_sub(1)] {
                let click = MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row: area.y,
                    modifiers: KeyModifiers::NONE,
                };
                assert_eq!(
                    mouse_action(click, &hit_map, &view),
                    Some(UiAction::ShowScreen(*screen))
                );
            }
        }
        for adjacent in hit_map.tabs.windows(2) {
            assert!(
                adjacent[0].1.right() <= adjacent[1].1.x,
                "tab hit targets must never overlap"
            );
            for column in adjacent[0].1.right()..adjacent[1].1.x {
                let divider_click = MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row: adjacent[0].1.y,
                    modifiers: KeyModifiers::NONE,
                };
                assert_eq!(mouse_action(divider_click, &hit_map, &view), None);
            }
        }

        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).expect("compact terminal");
        let mut compact_hit_map = HitMap::default();
        terminal
            .draw(|frame| {
                render_tabs(
                    frame,
                    frame.area(),
                    &view,
                    &Theme::new(false),
                    &mut compact_hit_map,
                );
            })
            .expect("draw compact tabs");
        #[cfg(feature = "youtube-music")]
        assert!(rendered_text(&terminal).contains("YT Music"));
        let compact_screens = Screen::ALL
            .into_iter()
            .filter(|screen| screen.enabled())
            .collect::<Vec<_>>();
        let compact_divider_width = terminal_text_width("│");
        let visible = active_tab_window(&compact_screens, view.screen, 80, compact_divider_width);
        assert_eq!(
            compact_hit_map
                .tabs
                .iter()
                .map(|(screen, _)| *screen)
                .collect::<Vec<_>>(),
            compact_screens[visible].to_vec()
        );
        let mut expected_x = 0_u16;
        for (index, (screen, area)) in compact_hit_map.tabs.iter().enumerate() {
            if index > 0 {
                expected_x = expected_x.saturating_add(compact_divider_width);
            }
            let expected_width = terminal_text_width(screen.compact_label());
            assert_eq!(
                *area,
                Rect::new(expected_x, 0, expected_width, 1),
                "{screen:?} must have an exact label-only hit target"
            );
            expected_x = expected_x.saturating_add(expected_width);
        }
        for (screen, area) in &compact_hit_map.tabs {
            for column in [area.x, area.right().saturating_sub(1)] {
                let click = MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row: area.y,
                    modifiers: KeyModifiers::NONE,
                };
                assert_eq!(
                    mouse_action(click, &compact_hit_map, &view),
                    Some(UiAction::ShowScreen(*screen))
                );
            }
        }
        for adjacent in compact_hit_map.tabs.windows(2) {
            assert!(
                adjacent[0].1.right() <= adjacent[1].1.x,
                "compact tab hit targets must never overlap"
            );
            for column in adjacent[0].1.right()..adjacent[1].1.x {
                let divider_click = MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row: adjacent[0].1.y,
                    modifiers: KeyModifiers::NONE,
                };
                assert_eq!(mouse_action(divider_click, &compact_hit_map, &view), None);
            }
        }
    }

    #[test]
    fn source_tabs_use_space_saving_labels_and_keep_librivox_order() {
        assert_eq!(Screen::Search.label(), "YT");
        assert_eq!(Screen::Search.compact_label(), "YT");
        assert_eq!(Screen::YouTubeMusic.label(), "YT Music");
        assert_eq!(Screen::YouTubeMusic.compact_label(), "YT Music");
        assert_eq!(Screen::LibriVox.label(), "LibriVox");
        assert_eq!(Screen::LibriVox.compact_label(), "LibriVox");
        let librivox_index = Screen::ALL
            .iter()
            .position(|screen| *screen == Screen::LibriVox)
            .expect("LibriVox tab");
        assert_eq!(Screen::ALL[librivox_index - 1], Screen::ApplePodcasts);
        assert_eq!(Screen::ALL[librivox_index + 1], Screen::Radio);
        assert_eq!(SourceKind::YouTube.to_string(), "youtube");
    }

    #[test]
    fn narrow_tab_strip_keeps_the_active_tab_visible_and_clickable() {
        let active_screens = [
            #[cfg(feature = "apple-podcasts")]
            Screen::ApplePodcasts,
            Screen::Statistics,
        ];
        for active in active_screens {
            let backend = TestBackend::new(40, 1);
            let mut terminal = Terminal::new(backend).expect("narrow terminal");
            let view = ViewModel {
                screen: active,
                ..ViewModel::default()
            };
            let mut hit_map = HitMap::default();

            terminal
                .draw(|frame| {
                    render_tabs(frame, frame.area(), &view, &Theme::new(false), &mut hit_map);
                })
                .expect("draw narrow tabs");

            let (_, active_area) = hit_map
                .tabs
                .iter()
                .find(|(screen, _)| *screen == active)
                .expect("active tab hit target");
            assert!(active_area.right() <= 40);
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: active_area.x,
                        row: active_area.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                Some(UiAction::ShowScreen(active))
            );
            assert!(
                rendered_text(&terminal).contains(active.compact_label()),
                "the active compact label must stay on-screen"
            );
        }
    }

    #[test]
    fn diagnostic_popup_captures_mouse_buttons_and_wheel() {
        let view = ViewModel {
            error_popup: Some(ErrorPopupView {
                title: "Error".to_owned(),
                report: "report".to_owned(),
                gh_available: true,
                ..ErrorPopupView::default()
            }),
            ..ViewModel::default()
        };
        let hit_map = HitMap {
            tabs: vec![(Screen::History, Rect::new(0, 0, 20, 2))],
            error_buttons: vec![
                (UiAction::CopyErrorReport, Rect::new(20, 20, 8, 1)),
                (
                    UiAction::RequestGitHubIssueSubmission,
                    Rect::new(31, 20, 21, 1),
                ),
            ],
            ..HitMap::default()
        };
        let click_copy = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 22,
            row: 20,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click_copy, &hit_map, &view),
            Some(UiAction::CopyErrorReport)
        );

        let click_underlying_tab = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(mouse_action(click_underlying_tab, &hit_map, &view), None);

        let scroll = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 2,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(scroll, &hit_map, &view),
            Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Lines(3)))
        );
    }

    #[test]
    fn youtube_setup_popup_captures_mouse_fields_and_buttons() {
        let view = ViewModel {
            youtube_setup_popup: Some(YouTubeSetupPopupView::default()),
            ..ViewModel::default()
        };
        let hit_map = HitMap {
            tabs: vec![(Screen::History, Rect::new(0, 0, 20, 2))],
            youtube_setup_fields: vec![(YouTubeSetupField::InvidiousUrl, Rect::new(20, 8, 60, 3))],
            youtube_setup_buttons: vec![
                (UiAction::OpenYouTubeApiKeyGuide, Rect::new(20, 14, 60, 2)),
                (
                    UiAction::OpenGoogleCloudCredentials,
                    Rect::new(20, 16, 60, 2),
                ),
                (UiAction::OpenInvidiousInstances, Rect::new(20, 18, 60, 2)),
                (UiAction::SubmitYouTubeSetup, Rect::new(30, 22, 22, 1)),
            ],
            ..HitMap::default()
        };
        let click_field = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 25,
            row: 9,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click_field, &hit_map, &view),
            Some(UiAction::SelectYouTubeSetupField(
                YouTubeSetupField::InvidiousUrl
            ))
        );

        let click_guide = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 25,
            row: 14,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click_guide, &hit_map, &view),
            Some(UiAction::OpenYouTubeApiKeyGuide)
        );

        let click_cloud = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 25,
            row: 16,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click_cloud, &hit_map, &view),
            Some(UiAction::OpenGoogleCloudCredentials)
        );

        let click_instances = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 25,
            row: 18,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click_instances, &hit_map, &view),
            Some(UiAction::OpenInvidiousInstances)
        );

        let click_submit = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 35,
            row: 22,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click_submit, &hit_map, &view),
            Some(UiAction::SubmitYouTubeSetup)
        );

        let click_underlying_tab = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(mouse_action(click_underlying_tab, &hit_map, &view), None);
    }

    #[test]
    fn details_mouse_focus_and_scroll_preserve_link_targets_and_shift_selection() {
        let view = ViewModel::default();
        let hit_map = HitMap {
            details_panel: Rect::new(70, 5, 40, 12),
            details_scroll_offset: 4,
            details_scroll_maximum: 10,
            detail_links: vec![(2, Rect::new(70, 8, 30, 1))],
            ..HitMap::default()
        };
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 75,
            row: 8,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click, &hit_map, &view),
            Some(UiAction::ActivateDetailLink(2))
        );

        let scroll = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 75,
            row: 8,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(scroll, &hit_map, &view),
            Some(UiAction::SetDetailsScroll(7))
        );

        let click_panel = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 105,
            row: 12,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click_panel, &hit_map, &view),
            Some(UiAction::SetDetailsFocus(true))
        );

        let shift_drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 105,
            row: 12,
            modifiers: KeyModifiers::SHIFT,
        };
        assert_eq!(
            mouse_action(shift_drag, &hit_map, &view),
            None,
            "Shift-drag remains reserved for terminal-native text selection"
        );
    }

    #[test]
    fn details_selection_clips_reverse_drag_and_copies_multiple_visible_rows() {
        let hit_map = HitMap {
            detail_text_rows: vec![
                SelectableDetailsRow {
                    x: 70,
                    y: 5,
                    cells: "alpha"
                        .chars()
                        .map(|character| character.to_string())
                        .collect(),
                },
                SelectableDetailsRow {
                    x: 70,
                    y: 6,
                    cells: "beta"
                        .chars()
                        .map(|character| character.to_string())
                        .collect(),
                },
            ],
            ..HitMap::default()
        };
        let view = ViewModel {
            text_selection_mode: true,
            details_text_selection: Some(DetailsTextSelection {
                anchor: DetailsTextPosition { row: 1, column: 3 },
                focus: DetailsTextPosition { row: 1, column: 3 },
                dragging: true,
            }),
            ..ViewModel::default()
        };
        let release_beyond_top_left = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(
            mouse_action(release_beyond_top_left, &hit_map, &view),
            Some(UiAction::FinishDetailsTextSelection {
                focus: DetailsTextPosition { row: 0, column: 0 },
                text: "alpha\nbeta".to_owned(),
            })
        );
    }

    #[test]
    fn text_selection_mode_keeps_left_result_panel_mouse_controls_normal() {
        let view = ViewModel {
            text_selection_mode: true,
            rows: vec![RowView::default(), RowView::default()],
            ..ViewModel::default()
        };
        let hit_map = HitMap {
            rows: Rect::new(1, 4, 30, 6),
            details_panel: Rect::new(50, 3, 40, 10),
            detail_text_rows: vec![SelectableDetailsRow {
                x: 51,
                y: 4,
                cells: vec!["D".to_owned()],
            }],
            ..HitMap::default()
        };

        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 2,
                    row: 6,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SelectRow(1))
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column: 2,
                    row: 6,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::MoveSelection(1))
        );
    }
}
