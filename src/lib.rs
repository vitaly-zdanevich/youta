//! Core library for the Youta terminal audio player.
//!
//! The crate keeps source discovery, persistence, playback, and terminal
//! rendering behind separate interfaces so distribution builds can omit
//! services they do not use.

pub mod config;
pub mod diagnostics;
pub mod domain;
pub mod links;
pub mod local_browser;
pub mod persistence;
pub mod playback;
pub mod providers;
pub mod report_actions;
#[cfg(feature = "subscriptions")]
pub mod subscriptions;
#[cfg(feature = "tui")]
pub(crate) mod terminal_environment;
pub mod waveform;

#[cfg(feature = "waveform")]
pub mod local_waveform;

#[cfg(feature = "tui")]
pub mod git_sync;

#[cfg(any(feature = "local-move", feature = "local-rename"))]
pub mod local_move;

#[cfg(feature = "network")]
pub mod audio_identification;

#[cfg(all(feature = "gpm", target_os = "linux"))]
pub(crate) mod gpm;

#[cfg(feature = "tracker-music")]
pub mod tracker_media;

#[cfg(feature = "images")]
pub mod thumbnails;

#[cfg(feature = "tui")]
pub mod app;

#[cfg(feature = "tui")]
pub mod tui;
