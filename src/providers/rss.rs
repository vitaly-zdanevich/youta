//! Bounded RSS, Atom, and JSON Feed podcast ingestion.
//!
//! [`RssPodcastProvider`] performs blocking network work and is intended for a
//! provider worker thread. Responses are read into a bounded byte buffer before
//! `feed-rs` parses them. Redirects are followed manually so every target is
//! validated before it is requested.

use std::collections::HashMap;
use std::time::Duration;

use feed_rs::model::{Category, Entry, Feed, FeedType, Link, Person, Text};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ureq::ResponseExt as _;
use url::Url;

use super::{DEFAULT_REQUEST_TIMEOUT, ProviderError};

const DEFAULT_MAX_FEED_BYTES: usize = 8 * 1024 * 1024;
const MAX_CONFIGURED_FEED_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_MAX_ENTRIES: usize = 1_000;
const MAX_CONFIGURED_ENTRIES: usize = 5_000;
const MAX_REDIRECTS: usize = 5;
const GENERATED_ID_PREFIX: &str = "urn:youta:generated:";

/// Resource limits and transport policy for podcast feeds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RssPodcastOptions {
    /// End-to-end timeout applied to each HTTP request.
    pub timeout: Duration,
    /// Maximum decoded response body size in bytes.
    pub max_response_bytes: usize,
    /// Maximum number of entries accepted from one feed.
    pub max_entries: usize,
    /// Whether plain HTTP feed, redirect, and media URLs are accepted.
    ///
    /// This defaults to `true` because some established podcast feeds and
    /// enclosures have not migrated to HTTPS.
    pub allow_http: bool,
}

impl Default for RssPodcastOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_REQUEST_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_FEED_BYTES,
            max_entries: DEFAULT_MAX_ENTRIES,
            allow_http: true,
        }
    }
}

impl RssPodcastOptions {
    fn validate(&self) -> Result<(), ProviderError> {
        if self.timeout.is_zero() {
            return Err(ProviderError::InvalidRequest(
                "podcast feed timeout must be greater than zero".to_owned(),
            ));
        }
        if !(1..=MAX_CONFIGURED_FEED_BYTES).contains(&self.max_response_bytes) {
            return Err(ProviderError::InvalidRequest(format!(
                "podcast feed response limit must be between 1 and \
                 {MAX_CONFIGURED_FEED_BYTES} bytes"
            )));
        }
        if !(1..=MAX_CONFIGURED_ENTRIES).contains(&self.max_entries) {
            return Err(ProviderError::InvalidRequest(format!(
                "podcast feed entry limit must be between 1 and {MAX_CONFIGURED_ENTRIES}"
            )));
        }
        Ok(())
    }
}

/// A normalized podcast feed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PodcastFeed {
    /// Stable feed-supplied or deterministically generated identifier.
    pub id: String,
    /// Final feed URL after validated redirects.
    pub source_url: Url,
    /// Human-readable feed title.
    pub title: Option<String>,
    /// Feed description, which may contain markup supplied by the publisher.
    pub description: Option<String>,
    /// Publisher or author names in source order.
    pub authors: Vec<String>,
    /// Feed language tag, when supplied.
    pub language: Option<String>,
    /// Flattened feed category labels in source order.
    pub categories: Vec<String>,
    /// Feed publication timestamp in RFC 3339 form.
    pub published_at: Option<String>,
    /// Feed update timestamp in RFC 3339 form.
    pub updated_at: Option<String>,
    /// Credential-free HTTP(S) publisher page.
    pub webpage_url: Option<Url>,
    /// Credential-free HTTP(S) feed artwork.
    pub artwork_url: Option<Url>,
    /// Entries in publisher order.
    pub episodes: Vec<PodcastEpisode>,
}

/// A normalized podcast episode.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PodcastEpisode {
    /// Stable feed-supplied or deterministically generated identifier.
    pub id: String,
    /// Episode title, when supplied.
    pub title: Option<String>,
    /// Episode description, which may contain publisher-supplied markup.
    pub description: Option<String>,
    /// Episode author names, falling back to feed authors when absent.
    pub authors: Vec<String>,
    /// Episode language tag, falling back to the feed language.
    pub language: Option<String>,
    /// Flattened episode categories, falling back to feed categories.
    pub categories: Vec<String>,
    /// Episode publication timestamp in RFC 3339 form.
    pub published_at: Option<String>,
    /// Episode update timestamp in RFC 3339 form.
    pub updated_at: Option<String>,
    /// Credential-free HTTP(S) episode page.
    pub webpage_url: Option<Url>,
    /// Credential-free HTTP(S) episode artwork, falling back to feed artwork.
    pub artwork_url: Option<Url>,
    /// Episode duration in whole seconds, when supplied.
    pub duration_seconds: Option<u64>,
    /// Deduplicated playable media candidates in source order.
    pub enclosures: Vec<PodcastEnclosure>,
}

/// One normalized podcast media enclosure.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PodcastEnclosure {
    /// Credential-free HTTP(S) media URL.
    pub url: Url,
    /// Advertised MIME type.
    pub mime_type: Option<String>,
    /// Advertised media size in bytes.
    pub byte_length: Option<u64>,
    /// Advertised duration in whole seconds.
    pub duration_seconds: Option<u64>,
}

/// Blocking bounded podcast-feed client.
#[derive(Clone)]
pub struct RssPodcastProvider {
    agent: ureq::Agent,
    options: RssPodcastOptions,
}

impl RssPodcastProvider {
    /// Creates a client with an eight-MiB response limit, a 1,000-entry limit,
    /// and legacy HTTP support.
    #[must_use]
    pub fn new() -> Self {
        let options = RssPodcastOptions::default();
        Self {
            agent: podcast_agent(options.timeout),
            options,
        }
    }

    /// Creates a client with explicit limits and transport policy.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] when the timeout or either
    /// resource bound is zero, or a bound exceeds its hard safety maximum.
    pub fn with_options(options: RssPodcastOptions) -> Result<Self, ProviderError> {
        options.validate()?;
        let agent = podcast_agent(options.timeout);
        Ok(Self { agent, options })
    }

    /// Fetches and normalizes one RSS, Atom, or JSON Feed document.
    ///
    /// Redirects are followed at most five times. Each redirect target is
    /// resolved against the prior URL and validated before a request is sent.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] for an unsafe URL, failed request,
    /// unsuccessful HTTP status, oversized response, malformed feed, or feed
    /// with more than the configured number of entries.
    pub fn fetch(&self, source_url: &Url) -> Result<PodcastFeed, ProviderError> {
        let mut current = validate_public_url(source_url.clone(), self.options.allow_http)?;

        for redirect_count in 0..=MAX_REDIRECTS {
            let mut response = self
                .agent
                .get(current.as_str())
                .header(
                    "Accept",
                    "application/atom+xml, application/feed+json, \
                     application/json;q=0.9, application/rss+xml;q=0.9, \
                     application/xml;q=0.8, text/xml;q=0.8, */*;q=0.1",
                )
                .call()
                .map_err(map_ureq_error)?;
            let status = response.status().as_u16();

            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                if redirect_count == MAX_REDIRECTS {
                    return Err(ProviderError::Transport(format!(
                        "podcast feed exceeded the {MAX_REDIRECTS}-redirect limit"
                    )));
                }
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| {
                        ProviderError::InvalidResponse(
                            "podcast feed redirect omitted a valid Location header".to_owned(),
                        )
                    })?;
                let target = current.join(location).map_err(|error| {
                    ProviderError::InvalidResponse(format!(
                        "invalid podcast feed redirect target: {error}"
                    ))
                })?;
                current = validate_public_url(target, self.options.allow_http)?;
                continue;
            }
            if (300..400).contains(&status) {
                return Err(ProviderError::HttpStatus(status));
            }

            // Redirects are disabled in ureq, so this should equal `current`.
            // Validate it independently to keep this invariant explicit.
            let final_url = Url::parse(&response.get_uri().to_string()).map_err(|error| {
                ProviderError::InvalidResponse(format!(
                    "HTTP client returned an invalid final feed URL: {error}"
                ))
            })?;
            let final_url = validate_public_url(final_url, self.options.allow_http)?;

            if response
                .body()
                .content_length()
                .is_some_and(|length| length > self.options.max_response_bytes as u64)
            {
                return Err(ProviderError::ResponseTooLarge {
                    limit: self.options.max_response_bytes,
                });
            }
            let bytes = response
                .body_mut()
                .with_config()
                .limit(
                    u64::try_from(self.options.max_response_bytes.saturating_add(1))
                        .unwrap_or(u64::MAX),
                )
                .read_to_vec()
                .map_err(|error| match error {
                    ureq::Error::BodyExceedsLimit(_) => ProviderError::ResponseTooLarge {
                        limit: self.options.max_response_bytes,
                    },
                    other => ProviderError::Transport(other.to_string()),
                })?;
            if bytes.len() > self.options.max_response_bytes {
                return Err(ProviderError::ResponseTooLarge {
                    limit: self.options.max_response_bytes,
                });
            }
            return self.parse(&final_url, &bytes);
        }

        unreachable!("the redirect loop always returns or continues")
    }

    /// Parses already-bounded feed bytes using `source_url` as the base URL.
    ///
    /// This entry point applies the same URL, response-size, entry-count, and
    /// child-link policy as [`Self::fetch`]. Missing source identifiers use a
    /// stable FNV-1a digest instead of `feed-rs`' random UUID fallback.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] when the source URL is unsafe, the input
    /// exceeds the configured response bound, parsing fails, or the feed has
    /// too many entries.
    pub fn parse(&self, source_url: &Url, bytes: &[u8]) -> Result<PodcastFeed, ProviderError> {
        let source_url = validate_public_url(source_url.clone(), self.options.allow_http)?;
        if bytes.len() > self.options.max_response_bytes {
            return Err(ProviderError::ResponseTooLarge {
                limit: self.options.max_response_bytes,
            });
        }

        let parsed = feed_rs::parser::Builder::new()
            .base_uri(Some(source_url.as_str()))
            .id_generator(|links, title, source_url| {
                deterministic_generated_id(links, title.as_ref(), source_url)
            })
            .build()
            .parse(bytes)
            .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
        if parsed.entries.len() > self.options.max_entries {
            return Err(ProviderError::InvalidResponse(format!(
                "podcast feed contains {} entries; configured limit is {}",
                parsed.entries.len(),
                self.options.max_entries
            )));
        }

        let is_json = parsed.feed_type == FeedType::JSON;
        let mut normalized = normalize_feed(&parsed, source_url, self.options.allow_http);
        uniquify_generated_episode_ids(&mut normalized.episodes);
        drop(parsed);

        if is_json {
            apply_json_attachment_durations(
                bytes,
                &normalized.source_url,
                self.options.allow_http,
                &mut normalized.episodes,
            );
        }
        Ok(normalized)
    }
}

impl Default for RssPodcastProvider {
    fn default() -> Self {
        Self::new()
    }
}

fn podcast_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .max_redirects(0)
        .user_agent(concat!(
            "youta/",
            env!("CARGO_PKG_VERSION"),
            " (+",
            env!("CARGO_PKG_REPOSITORY"),
            ")"
        ))
        .build()
        .into()
}

fn map_ureq_error(error: ureq::Error) -> ProviderError {
    match error {
        ureq::Error::StatusCode(code) => ProviderError::HttpStatus(code),
        ureq::Error::BodyExceedsLimit(limit) => ProviderError::ResponseTooLarge {
            limit: usize::try_from(limit).unwrap_or(usize::MAX),
        },
        other => ProviderError::Transport(other.to_string()),
    }
}

fn validate_public_url(mut url: Url, allow_http: bool) -> Result<Url, ProviderError> {
    let scheme_allowed = url.scheme() == "https" || (allow_http && url.scheme() == "http");
    if !scheme_allowed {
        let policy = if allow_http { "HTTP(S)" } else { "HTTPS" };
        return Err(ProviderError::InvalidRequest(format!(
            "podcast URL must use {policy}"
        )));
    }
    if url.host_str().is_none() || url.cannot_be_a_base() {
        return Err(ProviderError::InvalidRequest(
            "podcast URL must contain a host".to_owned(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ProviderError::InvalidRequest(
            "podcast URL must not contain embedded credentials".to_owned(),
        ));
    }
    url.set_fragment(None);
    Ok(url)
}

fn resolve_safe_url(raw: &str, base: &Url, allow_http: bool) -> Option<Url> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    let url = Url::parse(value).or_else(|_| base.join(value)).ok()?;
    validate_public_url(url, allow_http).ok()
}

fn normalize_feed(parsed: &Feed, source_url: Url, allow_http: bool) -> PodcastFeed {
    let authors = normalize_people(&parsed.authors);
    let categories = normalize_categories(&parsed.categories);
    let artwork_url = parsed
        .logo
        .as_ref()
        .and_then(|image| resolve_safe_url(&image.uri, &source_url, allow_http))
        .or_else(|| {
            parsed
                .icon
                .as_ref()
                .and_then(|image| resolve_safe_url(&image.uri, &source_url, allow_http))
        });
    let webpage_url = select_webpage(&parsed.links, &source_url, allow_http);
    let language = clean_string(parsed.language.as_deref());

    let episodes = parsed
        .entries
        .iter()
        .map(|entry| {
            normalize_episode(
                entry,
                &source_url,
                allow_http,
                &authors,
                language.as_ref(),
                &categories,
                artwork_url.as_ref(),
            )
        })
        .collect();

    PodcastFeed {
        id: clean_string(Some(&parsed.id))
            .unwrap_or_else(|| deterministic_text_id("feed", source_url.as_str())),
        source_url,
        title: normalize_text(parsed.title.as_ref()),
        description: normalize_text(parsed.description.as_ref()),
        authors,
        language,
        categories,
        published_at: parsed.published.map(|value| value.to_rfc3339()),
        updated_at: parsed.updated.map(|value| value.to_rfc3339()),
        webpage_url,
        artwork_url,
        episodes,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "explicit immutable feed fallbacks keep episode normalization allocation-light"
)]
fn normalize_episode(
    entry: &Entry,
    source_url: &Url,
    allow_http: bool,
    feed_authors: &[String],
    feed_language: Option<&String>,
    feed_categories: &[String],
    feed_artwork: Option<&Url>,
) -> PodcastEpisode {
    let mut authors = normalize_people(&entry.authors);
    for media in &entry.media {
        for credit in &media.credits {
            push_unique_clean(&mut authors, &credit.entity);
        }
    }
    if authors.is_empty() {
        authors.extend_from_slice(feed_authors);
    }

    let mut categories = normalize_categories(&entry.categories);
    if categories.is_empty() {
        categories.extend_from_slice(feed_categories);
    }

    let artwork_url = entry
        .media
        .iter()
        .flat_map(|media| &media.thumbnails)
        .find_map(|thumbnail| resolve_safe_url(&thumbnail.image.uri, source_url, allow_http))
        .or_else(|| feed_artwork.cloned());
    let mut enclosures = normalize_enclosures(entry, source_url, allow_http);
    let duration_seconds = entry
        .media
        .iter()
        .find_map(|media| media.duration.map(|duration| duration.as_secs()))
        .or_else(|| {
            enclosures
                .iter()
                .find_map(|enclosure| enclosure.duration_seconds)
        });
    if let Some(duration) = duration_seconds {
        for enclosure in &mut enclosures {
            enclosure.duration_seconds.get_or_insert(duration);
        }
    }

    PodcastEpisode {
        id: clean_string(Some(&entry.id))
            .unwrap_or_else(|| deterministic_text_id("episode", source_url.as_str())),
        title: normalize_text(entry.title.as_ref()),
        description: normalize_text(entry.summary.as_ref())
            .or_else(|| {
                entry
                    .content
                    .as_ref()
                    .and_then(|content| clean_string(content.body.as_deref()))
            })
            .or_else(|| {
                entry
                    .media
                    .iter()
                    .find_map(|media| normalize_text(media.description.as_ref()))
            }),
        authors,
        language: clean_string(entry.language.as_deref()).or_else(|| feed_language.cloned()),
        categories,
        published_at: entry.published.map(|value| value.to_rfc3339()),
        updated_at: entry.updated.map(|value| value.to_rfc3339()),
        webpage_url: select_webpage(&entry.links, source_url, allow_http),
        artwork_url,
        duration_seconds,
        enclosures,
    }
}

fn normalize_enclosures(
    entry: &Entry,
    source_url: &Url,
    allow_http: bool,
) -> Vec<PodcastEnclosure> {
    let mut result = Vec::new();
    let mut indexes = HashMap::<String, usize>::new();

    if let Some(content) = &entry.content
        && let Some(link) = &content.src
    {
        let mime_type = clean_string(link.media_type.as_deref())
            .or_else(|| clean_string(Some(content.content_type.as_str())));
        add_enclosure(
            &mut result,
            &mut indexes,
            resolve_safe_url(&link.href, source_url, allow_http),
            mime_type,
            content.length.or(link.length),
            None,
        );
    }

    for link in entry.links.iter().filter(|link| is_enclosure_link(link)) {
        add_enclosure(
            &mut result,
            &mut indexes,
            resolve_safe_url(&link.href, source_url, allow_http),
            clean_string(link.media_type.as_deref()),
            link.length,
            None,
        );
    }

    for media in &entry.media {
        for content in &media.content {
            add_enclosure(
                &mut result,
                &mut indexes,
                content
                    .url
                    .as_ref()
                    .and_then(|url| validate_public_url(url.clone(), allow_http).ok()),
                content
                    .content_type
                    .as_ref()
                    .and_then(|mime| clean_string(Some(mime.as_str()))),
                content.size,
                content
                    .duration
                    .or(media.duration)
                    .map(|duration| duration.as_secs()),
            );
        }
    }
    result
}

fn add_enclosure(
    result: &mut Vec<PodcastEnclosure>,
    indexes: &mut HashMap<String, usize>,
    url: Option<Url>,
    mime_type: Option<String>,
    byte_length: Option<u64>,
    duration_seconds: Option<u64>,
) {
    let Some(url) = url else {
        return;
    };
    let key = url.as_str().to_owned();
    if let Some(index) = indexes.get(&key).copied() {
        let existing = &mut result[index];
        if existing.mime_type.is_none() {
            existing.mime_type = mime_type;
        }
        if existing.byte_length.is_none() {
            existing.byte_length = byte_length;
        }
        if existing.duration_seconds.is_none() {
            existing.duration_seconds = duration_seconds;
        }
        return;
    }

    indexes.insert(key, result.len());
    result.push(PodcastEnclosure {
        url,
        mime_type,
        byte_length,
        duration_seconds,
    });
}

fn is_enclosure_link(link: &Link) -> bool {
    link.rel
        .as_deref()
        .is_some_and(|relation| relation.eq_ignore_ascii_case("enclosure"))
        || link
            .media_type
            .as_deref()
            .is_some_and(is_probable_media_mime)
}

fn is_probable_media_mime(mime: &str) -> bool {
    let mime = mime.trim();
    mime.get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("audio/"))
        || mime
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("video/"))
        || [
            "application/ogg",
            "application/octet-stream",
            "application/vnd.apple.mpegurl",
            "application/x-mpegurl",
        ]
        .iter()
        .any(|candidate| mime.eq_ignore_ascii_case(candidate))
}

fn select_webpage(links: &[Link], base: &Url, allow_http: bool) -> Option<Url> {
    links
        .iter()
        .filter(|link| {
            !is_enclosure_link(link)
                && link
                    .rel
                    .as_deref()
                    .is_none_or(|relation| relation.eq_ignore_ascii_case("alternate"))
        })
        .find_map(|link| resolve_safe_url(&link.href, base, allow_http))
}

fn normalize_people(people: &[Person]) -> Vec<String> {
    let mut result = Vec::new();
    for person in people {
        push_unique_clean(&mut result, &person.name);
    }
    result
}

fn normalize_categories(categories: &[Category]) -> Vec<String> {
    let mut result = Vec::new();
    let mut pending = categories.iter().rev().collect::<Vec<_>>();
    while let Some(category) = pending.pop() {
        let label = category
            .label
            .as_deref()
            .and_then(|value| clean_string(Some(value)))
            .or_else(|| clean_string(Some(&category.term)));
        if let Some(label) = label
            && !result.contains(&label)
        {
            result.push(label);
        }
        pending.extend(category.subcategories.iter().rev());
    }
    result
}

fn push_unique_clean(values: &mut Vec<String>, value: &str) {
    if let Some(value) = clean_string(Some(value))
        && !values.contains(&value)
    {
        values.push(value);
    }
}

fn normalize_text(text: Option<&Text>) -> Option<String> {
    text.and_then(|value| clean_string(Some(&value.content)))
}

fn clean_string(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn deterministic_generated_id(
    links: &[Link],
    title: Option<&Text>,
    source_url: Option<&str>,
) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for link in links {
        update_fnv1a(&mut hash, link.href.as_bytes());
        update_fnv1a(&mut hash, &[0]);
    }
    if let Some(title) = title {
        update_fnv1a(&mut hash, title.content.as_bytes());
    }
    update_fnv1a(&mut hash, &[0xff]);
    if let Some(source_url) = source_url {
        update_fnv1a(&mut hash, source_url.as_bytes());
    }
    format!("{GENERATED_ID_PREFIX}{hash:016x}")
}

fn deterministic_text_id(kind: &str, value: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    update_fnv1a(&mut hash, kind.as_bytes());
    update_fnv1a(&mut hash, &[0]);
    update_fnv1a(&mut hash, value.as_bytes());
    format!("{GENERATED_ID_PREFIX}{hash:016x}")
}

fn update_fnv1a(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

fn uniquify_generated_episode_ids(episodes: &mut [PodcastEpisode]) {
    let mut occurrences = HashMap::<String, usize>::new();
    for episode in episodes {
        if !episode.id.starts_with(GENERATED_ID_PREFIX) {
            continue;
        }
        let count = occurrences.entry(episode.id.clone()).or_default();
        *count += 1;
        if *count > 1 {
            episode.id.push(':');
            episode.id.push_str(&count.to_string());
        }
    }
}

fn apply_json_attachment_durations(
    bytes: &[u8],
    source_url: &Url,
    allow_http: bool,
    episodes: &mut [PodcastEpisode],
) {
    let Ok(document) = serde_json::from_slice::<Value>(bytes) else {
        return;
    };
    let Some(items) = document.get("items").and_then(Value::as_array) else {
        return;
    };

    for (episode, item) in episodes.iter_mut().zip(items) {
        let Some(attachments) = item.get("attachments").and_then(Value::as_array) else {
            continue;
        };
        for attachment in attachments {
            let Some(duration) = attachment
                .get("duration_in_seconds")
                .and_then(Value::as_u64)
            else {
                continue;
            };
            let Some(url) = attachment
                .get("url")
                .and_then(Value::as_str)
                .and_then(|raw| resolve_safe_url(raw, source_url, allow_http))
            else {
                continue;
            };
            if let Some(enclosure) = episode
                .enclosures
                .iter_mut()
                .find(|enclosure| enclosure.url == url)
            {
                enclosure.duration_seconds.get_or_insert(duration);
                episode.duration_seconds.get_or_insert(duration);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::thread::{self, JoinHandle};

    use super::*;

    const RSS_FIXTURE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"
     xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd"
     xmlns:media="http://search.yahoo.com/mrss/">
  <channel>
    <title>Rust Waves</title>
    <link>/shows/rust-waves</link>
    <description><![CDATA[Low-resource <b>audio</b>.]]></description>
    <language>en-US</language>
    <pubDate>Mon, 01 Jan 2024 10:00:00 +0000</pubDate>
    <lastBuildDate>Tue, 02 Jan 2024 11:00:00 +0000</lastBuildDate>
    <itunes:author>Example Network</itunes:author>
    <itunes:image href="/images/feed.jpg"/>
    <itunes:category text="Technology">
      <itunes:category text="Programming"/>
    </itunes:category>
    <category>Education</category>
    <item>
      <title>Memory without noise</title>
      <link>/episodes/memory</link>
      <description><![CDATA[An <i>episode</i> description.]]></description>
      <pubDate>Wed, 03 Jan 2024 12:00:00 +0000</pubDate>
      <itunes:author>Ada Host</itunes:author>
      <itunes:image href="/images/episode.jpg"/>
      <itunes:duration>01:02:03</itunes:duration>
      <category>Rust</category>
      <enclosure url="/media/memory.mp3" type="audio/mpeg" length="123456"/>
      <media:content url="/media/memory.mp3"
                     type="audio/mpeg"
                     fileSize="999999"
                     duration="10"/>
      <media:content url="/media/memory.webm"
                     type="video/webm"
                     fileSize="654321"
                     duration="3724"/>
    </item>
  </channel>
</rss>"#;

    const ATOM_FIXTURE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom"
      xmlns:media="http://search.yahoo.com/mrss/"
      xml:lang="en">
  <id>tag:example.test,2024:feed</id>
  <title>Relative Atom</title>
  <updated>2024-02-03T04:05:06Z</updated>
  <author><name>Feed Author</name></author>
  <link rel="alternate" href="../home"/>
  <entry xml:lang="fr">
    <id>tag:example.test,2024:episode</id>
    <title>Relative links</title>
    <updated>2024-02-04T05:06:07Z</updated>
    <published>2024-02-04T04:00:00Z</published>
    <summary>Relative media URLs are resolved.</summary>
    <link rel="alternate" href="../episodes/relative"/>
    <link rel="enclosure" href="../media/relative.ogg"
          type="audio/ogg" length="42"/>
    <content src="../media/relative.ogg" type="audio/ogg"/>
    <media:content url="../media/relative.ogg"
                   type="audio/ogg" fileSize="84" duration="90"/>
    <media:thumbnail url="../images/relative.png"/>
  </entry>
</feed>"#;

    const JSON_FEED_FIXTURE: &str = r#"{
  "version": "https://jsonfeed.org/version/1.1",
  "title": "JSON Audio",
  "home_page_url": "https://json.example.test/show",
  "feed_url": "https://json.example.test/feed.json",
  "description": "A JSON Feed podcast.",
  "icon": "https://json.example.test/cover.png",
  "language": "en-GB",
  "authors": [{"name": "JSON Host"}],
  "items": [{
    "id": "json-episode-1",
    "url": "https://json.example.test/episodes/1",
    "title": "Attachment metadata",
    "summary": "Duration comes from the JSON attachment.",
    "date_published": "2024-03-01T00:00:00Z",
    "tags": ["Technology", "Open audio"],
    "attachments": [{
      "url": "https://cdn.example.test/episode.opus",
      "mime_type": "audio/ogg",
      "title": "Opus",
      "size_in_bytes": 777,
      "duration_in_seconds": 321
    }]
  }]
}"#;

    #[test]
    fn rss_itunes_and_media_rss_are_normalized_and_deduplicated() {
        let provider = RssPodcastProvider::new();
        let source = Url::parse("https://feeds.example.test/podcasts/feed.xml").unwrap();

        let feed = provider.parse(&source, RSS_FIXTURE.as_bytes()).unwrap();

        assert_eq!(feed.title.as_deref(), Some("Rust Waves"));
        assert_eq!(
            feed.description.as_deref(),
            Some("Low-resource <b>audio</b>.")
        );
        assert_eq!(feed.authors, ["Example Network"]);
        assert_eq!(feed.language.as_deref(), Some("en-us"));
        assert_eq!(feed.categories, ["Technology", "Programming", "Education"]);
        assert_eq!(
            feed.webpage_url.as_ref().map(Url::as_str),
            Some("https://feeds.example.test/shows/rust-waves")
        );
        assert_eq!(
            feed.artwork_url.as_ref().map(Url::as_str),
            Some("https://feeds.example.test/images/feed.jpg")
        );
        assert_eq!(
            feed.published_at.as_deref(),
            Some("2024-01-01T10:00:00+00:00")
        );
        assert_eq!(
            feed.updated_at.as_deref(),
            Some("2024-01-02T11:00:00+00:00")
        );

        let episode = &feed.episodes[0];
        assert!(episode.id.starts_with(GENERATED_ID_PREFIX));
        assert_eq!(episode.title.as_deref(), Some("Memory without noise"));
        assert_eq!(episode.authors, ["Ada Host"]);
        assert_eq!(episode.language.as_deref(), Some("en-us"));
        assert_eq!(episode.categories, ["Rust"]);
        assert_eq!(episode.duration_seconds, Some(3_723));
        assert_eq!(
            episode.artwork_url.as_ref().map(Url::as_str),
            Some("https://feeds.example.test/images/episode.jpg")
        );
        assert_eq!(episode.enclosures.len(), 2);
        assert_eq!(
            episode.enclosures[0],
            PodcastEnclosure {
                url: Url::parse("https://feeds.example.test/media/memory.mp3").unwrap(),
                mime_type: Some("audio/mpeg".to_owned()),
                byte_length: Some(123_456),
                duration_seconds: Some(3_723),
            }
        );
        assert_eq!(
            episode.enclosures[1].url.as_str(),
            "https://feeds.example.test/media/memory.webm"
        );
        assert_eq!(episode.enclosures[1].byte_length, Some(654_321));
        assert_eq!(episode.enclosures[1].duration_seconds, Some(3_724));
    }

    #[test]
    fn atom_relative_urls_content_src_and_enclosure_links_are_supported() {
        let provider = RssPodcastProvider::new();
        let source = Url::parse("https://example.test/podcasts/feed.atom").unwrap();

        let feed = provider.parse(&source, ATOM_FIXTURE.as_bytes()).unwrap();

        assert_eq!(
            feed.webpage_url.as_ref().map(Url::as_str),
            Some("https://example.test/home")
        );
        let episode = &feed.episodes[0];
        assert_eq!(episode.language.as_deref(), Some("en"));
        assert_eq!(
            episode.webpage_url.as_ref().map(Url::as_str),
            Some("https://example.test/episodes/relative")
        );
        assert_eq!(
            episode.artwork_url.as_ref().map(Url::as_str),
            Some("https://example.test/images/relative.png")
        );
        assert_eq!(episode.enclosures.len(), 1);
        assert_eq!(
            episode.enclosures[0].url.as_str(),
            "https://example.test/media/relative.ogg"
        );
        assert_eq!(
            episode.enclosures[0].mime_type.as_deref(),
            Some("audio/ogg")
        );
        assert_eq!(episode.enclosures[0].byte_length, Some(42));
        assert_eq!(episode.enclosures[0].duration_seconds, Some(90));
    }

    #[test]
    fn json_feed_attachment_metadata_and_duration_are_preserved() {
        let provider = RssPodcastProvider::new();
        let source = Url::parse("https://json.example.test/feed.json").unwrap();

        let feed = provider
            .parse(&source, JSON_FEED_FIXTURE.as_bytes())
            .unwrap();

        assert_eq!(feed.authors, ["JSON Host"]);
        let episode = &feed.episodes[0];
        assert_eq!(episode.authors, ["JSON Host"]);
        assert_eq!(episode.categories, ["Technology", "Open audio"]);
        assert_eq!(episode.duration_seconds, Some(321));
        assert_eq!(episode.enclosures.len(), 1);
        assert_eq!(episode.enclosures[0].byte_length, Some(777));
        assert_eq!(episode.enclosures[0].duration_seconds, Some(321));
        assert_eq!(
            episode.enclosures[0].url.as_str(),
            "https://cdn.example.test/episode.opus"
        );
    }

    #[test]
    fn missing_ids_are_stable_and_duplicate_generated_ids_are_unique() {
        let fixture = br#"<?xml version="1.0"?>
<rss version="2.0"><channel>
  <title>No IDs</title><description>Testing</description>
  <item><description>same</description></item>
  <item><description>same</description></item>
</channel></rss>"#;
        let provider = RssPodcastProvider::new();
        let source = Url::parse("https://example.test/no-ids.xml").unwrap();

        let first = provider.parse(&source, fixture).unwrap();
        let second = provider.parse(&source, fixture).unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(first.episodes, second.episodes);
        assert!(first.id.starts_with(GENERATED_ID_PREFIX));
        assert!(first.episodes[0].id.starts_with(GENERATED_ID_PREFIX));
        assert_ne!(first.episodes[0].id, first.episodes[1].id);
        assert!(first.episodes[1].id.ends_with(":2"));
    }

    #[test]
    fn parser_rejects_malformed_oversized_and_excessive_entry_feeds() {
        let source = Url::parse("https://example.test/feed.xml").unwrap();
        let provider = RssPodcastProvider::new();
        assert!(matches!(
            provider.parse(&source, b"this is not a feed"),
            Err(ProviderError::InvalidResponse(_))
        ));

        let provider = RssPodcastProvider::with_options(RssPodcastOptions {
            max_response_bytes: 16,
            ..RssPodcastOptions::default()
        })
        .unwrap();
        assert!(matches!(
            provider.parse(&source, RSS_FIXTURE.as_bytes()),
            Err(ProviderError::ResponseTooLarge { limit: 16 })
        ));

        let provider = RssPodcastProvider::with_options(RssPodcastOptions {
            max_entries: 1,
            ..RssPodcastOptions::default()
        })
        .unwrap();
        let two_entries = br#"<?xml version="1.0"?>
<rss version="2.0"><channel>
  <title>Two</title><description>Two</description>
  <item><guid>one</guid></item><item><guid>two</guid></item>
</channel></rss>"#;
        assert!(matches!(
            provider.parse(&source, two_entries),
            Err(ProviderError::InvalidResponse(message))
                if message.contains("contains 2 entries")
        ));
    }

    #[test]
    fn options_enforce_nonzero_and_hard_bounds() {
        for options in [
            RssPodcastOptions {
                timeout: Duration::ZERO,
                ..RssPodcastOptions::default()
            },
            RssPodcastOptions {
                max_response_bytes: 0,
                ..RssPodcastOptions::default()
            },
            RssPodcastOptions {
                max_response_bytes: MAX_CONFIGURED_FEED_BYTES + 1,
                ..RssPodcastOptions::default()
            },
            RssPodcastOptions {
                max_entries: 0,
                ..RssPodcastOptions::default()
            },
            RssPodcastOptions {
                max_entries: MAX_CONFIGURED_ENTRIES + 1,
                ..RssPodcastOptions::default()
            },
        ] {
            assert!(matches!(
                RssPodcastProvider::with_options(options),
                Err(ProviderError::InvalidRequest(_))
            ));
        }
    }

    #[test]
    fn source_urls_reject_unsafe_schemes_credentials_and_disabled_http() {
        let provider = RssPodcastProvider::new();
        for raw in [
            "ftp://example.test/feed.xml",
            "https://user:secret@example.test/feed.xml",
            "file:///tmp/feed.xml",
        ] {
            let source = Url::parse(raw).unwrap();
            assert!(matches!(
                provider.parse(&source, RSS_FIXTURE.as_bytes()),
                Err(ProviderError::InvalidRequest(_))
            ));
        }

        let https_only = RssPodcastProvider::with_options(RssPodcastOptions {
            allow_http: false,
            ..RssPodcastOptions::default()
        })
        .unwrap();
        let http = Url::parse("http://example.test/feed.xml").unwrap();
        assert!(matches!(
            https_only.parse(&http, RSS_FIXTURE.as_bytes()),
            Err(ProviderError::InvalidRequest(_))
        ));
    }

    #[test]
    fn unsafe_child_urls_are_ignored_without_discarding_the_episode() {
        let fixture = br#"<?xml version="1.0"?>
<rss version="2.0"><channel>
  <title>Unsafe child</title><description>Unsafe child</description>
  <item><guid>one</guid><title>One</title>
    <link>javascript:alert(1)</link>
    <enclosure url="file:///tmp/secret.mp3" type="audio/mpeg" length="3"/>
  </item>
</channel></rss>"#;
        let provider = RssPodcastProvider::new();
        let source = Url::parse("https://example.test/feed.xml").unwrap();

        let feed = provider.parse(&source, fixture).unwrap();

        assert_eq!(feed.episodes.len(), 1);
        assert_eq!(feed.episodes[0].webpage_url, None);
        assert!(feed.episodes[0].enclosures.is_empty());
    }

    #[test]
    fn https_only_policy_drops_plain_http_child_media() {
        let fixture = br#"<?xml version="1.0"?>
<rss version="2.0"><channel>
  <title>Mixed</title><description>Mixed</description>
  <item><guid>one</guid>
    <enclosure url="http://cdn.example.test/one.mp3" type="audio/mpeg"/>
    <media:content xmlns:media="http://search.yahoo.com/mrss/"
      url="https://cdn.example.test/two.opus" type="audio/ogg"/>
  </item>
</channel></rss>"#;
        let provider = RssPodcastProvider::with_options(RssPodcastOptions {
            allow_http: false,
            ..RssPodcastOptions::default()
        })
        .unwrap();
        let source = Url::parse("https://example.test/feed.xml").unwrap();

        let feed = provider.parse(&source, fixture).unwrap();

        assert_eq!(feed.episodes[0].enclosures.len(), 1);
        assert_eq!(
            feed.episodes[0].enclosures[0].url.as_str(),
            "https://cdn.example.test/two.opus"
        );
    }

    #[test]
    fn fetch_follows_validated_relative_redirect_and_uses_final_base_url() {
        let feed = r#"<?xml version="1.0"?>
<rss version="2.0"><channel>
  <title>Redirected</title><description>Redirected</description>
  <item><guid>one</guid>
    <enclosure url="../media/one.opus" type="audio/ogg"/>
  </item>
</channel></rss>"#;
        let (base, server) = spawn_server(2, move |path| match path {
            "/start" => http_response("302 Found", &[("Location", "/feeds/final.xml")], ""),
            "/feeds/final.xml" => {
                http_response("200 OK", &[("Content-Type", "application/rss+xml")], feed)
            }
            other => panic!("unexpected request path: {other}"),
        });
        let provider = RssPodcastProvider::new();

        let result = provider.fetch(&base.join("start").unwrap()).unwrap();
        server.join().unwrap();

        assert_eq!(result.source_url.as_str(), format!("{base}feeds/final.xml"));
        assert_eq!(
            result.episodes[0].enclosures[0].url.as_str(),
            format!("{base}media/one.opus")
        );
    }

    #[test]
    fn fetch_checks_content_length_and_streamed_body_limits() {
        let body = "x".repeat(128);
        let (base, server) = spawn_server(1, move |_| {
            http_response("200 OK", &[("Content-Type", "application/rss+xml")], &body)
        });
        let provider = RssPodcastProvider::with_options(RssPodcastOptions {
            max_response_bytes: 32,
            ..RssPodcastOptions::default()
        })
        .unwrap();

        let error = provider.fetch(&base).unwrap_err();
        server.join().unwrap();

        assert!(matches!(
            error,
            ProviderError::ResponseTooLarge { limit: 32 }
        ));

        let body = "x".repeat(128);
        let (base, server) = spawn_server(1, move |_| {
            format!(
                "HTTP/1.1 200 OK\r\nConnection: close\r\n\
                 Content-Type: application/rss+xml\r\n\r\n{body}"
            )
        });
        let error = provider.fetch(&base).unwrap_err();
        server.join().unwrap();
        assert!(matches!(
            error,
            ProviderError::ResponseTooLarge { limit: 32 }
        ));
    }

    #[test]
    fn fetch_rejects_credentialed_redirect_before_requesting_it() {
        let (base, server) = spawn_server(1, |_| {
            http_response(
                "302 Found",
                &[("Location", "http://user:secret@example.test/feed.xml")],
                "",
            )
        });
        let provider = RssPodcastProvider::new();

        let error = provider.fetch(&base).unwrap_err();
        server.join().unwrap();

        assert!(matches!(error, ProviderError::InvalidRequest(message)
            if message.contains("embedded credentials")));
    }

    #[test]
    fn fetch_rejects_redirect_without_location_and_redirect_loops() {
        let (base, server) =
            spawn_server(1, |_| http_response("302 Found", &[], "missing location"));
        let provider = RssPodcastProvider::new();
        assert!(matches!(
            provider.fetch(&base),
            Err(ProviderError::InvalidResponse(message))
                if message.contains("Location")
        ));
        server.join().unwrap();

        let (base, server) = spawn_server(MAX_REDIRECTS + 1, |_| {
            http_response("307 Temporary Redirect", &[("Location", "/loop")], "")
        });
        let error = provider.fetch(&base.join("loop").unwrap()).unwrap_err();
        server.join().unwrap();
        assert!(matches!(error, ProviderError::Transport(message)
            if message.contains("redirect limit")));
    }

    fn spawn_server<F>(request_count: usize, handler: F) -> (Url, JoinHandle<()>)
    where
        F: Fn(&str) -> String + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().unwrap();
                let path = request_path(&stream);
                stream.write_all(handler(&path).as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });
        (Url::parse(&format!("http://{address}/")).unwrap(), server)
    }

    fn request_path(stream: &TcpStream) -> String {
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header == "\r\n" || header.is_empty() {
                break;
            }
        }
        request_line
            .split_ascii_whitespace()
            .nth(1)
            .unwrap()
            .to_owned()
    }

    fn http_response(status: &str, headers: &[(&str, &str)], body: &str) -> String {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            response.push_str(name);
            response.push_str(": ");
            response.push_str(value);
            response.push_str("\r\n");
        }
        response.push_str("\r\n");
        response.push_str(body);
        response
    }
}
