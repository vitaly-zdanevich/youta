//! Core library for the Youta terminal audio player.
//!
//! The crate keeps source discovery, persistence, playback, and terminal
//! rendering behind separate interfaces so distribution builds can omit
//! services they do not use.

/// Complete license notice embedded in every directly distributed executable.
///
/// Native desktop packages also carry the same repository file through
/// Tauri's `licenseFile` setting. Raw executables have no surrounding package,
/// so their command-line entry points expose this exact text through
/// `--license` instead.
pub const LICENSE_TEXT: &str = include_str!("../LICENSE");

#[cfg(test)]
pub(crate) mod test_support;

/// Canonical paths in the crate's one spelling; see the module's own account.
pub(crate) mod fs_path;

pub mod build_info;
pub mod child_process;
#[cfg(feature = "commons-upload")]
pub mod commons_upload;
pub mod config;
pub mod diagnostics;
pub mod domain;
pub mod durability;
#[cfg(feature = "evernote")]
pub mod evernote;
pub mod file_identity;
pub mod links;
#[cfg(feature = "local-archives")]
pub mod local_archive;
pub mod local_browser;
#[cfg(any(feature = "commons-upload", feature = "evernote"))]
pub mod opus_export;
pub mod persistence;
pub mod playback;
pub mod private_files;
pub mod providers;
#[cfg(feature = "qr")]
pub mod qr_code;
pub mod report_actions;
#[cfg(feature = "subscriptions")]
pub mod subscriptions;
#[cfg(feature = "tui")]
pub(crate) mod terminal_environment;
#[cfg(feature = "local-browser")]
pub mod text_file_open;
pub mod waveform;

#[cfg(feature = "audio-quality")]
pub mod audio_quality;

#[cfg(any(
    feature = "summary",
    feature = "evernote",
    feature = "youtube-captions"
))]
pub mod video_summary;

#[cfg(feature = "waveform")]
pub mod local_waveform;

#[cfg(feature = "tui")]
pub mod git_sync;

#[cfg(any(feature = "local-move", feature = "local-rename"))]
pub mod local_move;

#[cfg(feature = "acoustid")]
pub mod audio_identification;

#[cfg(all(feature = "gpm", target_os = "linux"))]
pub(crate) mod gpm;

#[cfg(feature = "tracker-music")]
pub mod tracker_media;

#[cfg(any(feature = "remote-artwork", feature = "local-artwork"))]
pub mod artwork;

#[cfg(feature = "local-artwork")]
pub(crate) mod local_artwork;

#[cfg(feature = "images")]
pub mod thumbnails;

#[cfg(feature = "controller")]
pub mod app;
#[cfg(feature = "ascii-visualizer")]
pub mod ascii_visualizer;

#[cfg(feature = "tui")]
pub mod tui;

#[cfg(feature = "controller")]
pub mod view;

#[cfg(feature = "controller")]
pub mod keymap;
