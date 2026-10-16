//! Authenticated VK catalogue browsing and playback with explicit cookie imports.
//!
//! Requests use a user-supplied Netscape cookie file, never automatic browser
//! discovery. Private cookies, reload keys, cursors, and signed playback URLs
//! remain in memory and redact their debug output. Only validated first-party
//! endpoints receive cookies; audio resolves to approved HTTPS media origins.

mod audio_url;
mod bootstrap;
mod catalog;
mod client;
mod cookies;
mod identity;

pub use audio_url::{VkPlaybackUrl, decode_audio_url};
pub use catalog::{VkPage, VkPageCursor, VkTrack};
pub use client::VkClient;
pub use cookies::{VkCookieHeader, VkCookieJar};
pub use identity::{ProfileReference, VkAudioId, VkQuery, parse_query};
