//! Core library for the Youta terminal audio player.
//!
//! The crate keeps source discovery, persistence, playback, and terminal
//! rendering behind separate interfaces so distribution builds can omit
//! services they do not use.

pub mod config;
pub mod diagnostics;
pub mod domain;
pub mod links;
pub mod persistence;
pub mod playback;
pub mod providers;
pub mod report_actions;
pub mod subscriptions;
pub mod waveform;

#[cfg(feature = "thumbnails")]
pub mod thumbnails;

#[cfg(feature = "tui")]
pub mod app;

#[cfg(feature = "tui")]
pub mod tui;
