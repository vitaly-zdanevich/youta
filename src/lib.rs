//! Core library for the Youta terminal audio player.
//!
//! The crate keeps source discovery, persistence, playback, and terminal
//! rendering behind separate interfaces so distribution builds can omit
//! services they do not use.

#[cfg(test)]
pub(crate) mod test_support;

pub mod build_info;
pub mod child_process;
pub mod config;
pub mod diagnostics;
pub mod domain;
pub mod durability;
pub mod file_identity;
pub mod links;
pub mod local_browser;
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

#[cfg(feature = "tui")]
pub mod tui;

#[cfg(feature = "controller")]
pub mod view;

#[cfg(feature = "controller")]
pub mod keymap;
