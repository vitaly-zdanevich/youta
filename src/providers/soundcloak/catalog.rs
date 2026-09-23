//! Public artist catalogues and lazy album metadata through the configured proxy.
//!
//! Artist track cursors are opaque, unlike numeric search offsets. Album pages
//! may be empty while still carrying a continuation. Upstream URLs are validated
//! and discarded; only bounded owner-bound query values survive between calls.

use super::*;

const MAX_ALBUM_TRACKS: usize = 1_000;
const MAX_CATALOG_PAGES: usize = 1_000;
const MAX_CURSOR_BYTES: usize = 1_024;

/// Distinguishes cursor ownership even when two lists belong to the same profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CatalogKind {
    Tracks,
    Albums,
}

impl CatalogKind {
    /// Adapter-owned API suffix; never supplied by a response URL.
    const fn path(self) -> &'static str {
        match self {
            Self::Tracks => "tracks",
            Self::Albums => "albums",
        }
    }
}

/// Public profile identity, separate from a possibly spaced or changing display name.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SoundcloakArtist {
    /// Positive upstream numeric user identifier.
    pub id: String,
    /// Terminal-safe public display name.
    pub name: String,
    /// Canonical original profile URL.
    pub webpage_url: Url,
    /// Public uploaded-track count, when reported.
    pub track_count: Option<u64>,
}

/// An explicitly classified public album, EP, or compilation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SoundcloakAlbum {
    /// Positive upstream numeric playlist identifier.
    pub id: String,
    /// Terminal-safe release title.
    pub title: String,
    /// Canonical original `/uploader/sets/release` page.
    pub webpage_url: Url,
    /// Public uploader display name; individual tracks may have different artists.
    pub artist: String,
    /// Canonical uploader profile, if safely advertised.
    pub artist_url: Option<Url>,
    /// Preview artwork routed through the configured instance.
    pub artwork_url: Option<Url>,
    /// Advertised track count, including a known zero.
    pub track_count: Option<u64>,
    /// Provider release classification, for example `album`, `ep`, or `compilation`.
    pub release_type: Option<String>,
}

/// Opaque in-memory continuation; no upstream URL or authentication is retained.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SoundcloakCatalogCursor {
    owner_id: String,
    owner_url: Url,
    kind: CatalogKind,
    limit: usize,
    offset: String,
    query_urn: Option<String>,
    visited_offsets: Vec<String>,
}

/// One raw artist-track page; an empty page can still have a valid continuation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SoundcloakArtistTrackPage {
    /// Normalized tracks in upstream order, including unavailable public tracks.
    pub items: Vec<SoundcloakTrack>,
    /// Validated continuation owned by this profile, list kind, and page size.
    pub next_cursor: Option<SoundcloakCatalogCursor>,
}

/// One raw artist-album page; short or empty pages do not imply exhaustion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SoundcloakArtistAlbumPage {
    /// Explicitly classified releases in upstream order.
    pub items: Vec<SoundcloakAlbum>,
    /// Validated continuation owned by this profile, list kind, and page size.
    pub next_cursor: Option<SoundcloakCatalogCursor>,
}

/// One exact advertised album slot, retaining unavailable tracks in their order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SoundcloakAlbumTrack {
    /// Stable positive numeric track identifier, even for missing metadata.
    pub id: String,
    /// Full metadata, or an unresolved stub in details / unavailable slot in a hydrated page.
    pub track: Option<SoundcloakTrack>,
}

/// Bounded release metadata; resolving it does not fetch any missing track bodies.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SoundcloakAlbumDetails {
    /// Public release facts.
    pub album: SoundcloakAlbum,
    /// Original ordered slots, some of which may require lazy hydration.
    pub tracks: Vec<SoundcloakAlbumTrack>,
}

/// One visible album slice, with at most two fifty-ID metadata requests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SoundcloakAlbumTrackPage {
    /// Ordered slots, retaining unavailable entries with `track: None`.
    pub items: Vec<SoundcloakAlbumTrack>,
    /// Next slice position, or no continuation at the end of the album.
    pub next_offset: Option<usize>,
}

impl SoundcloakClient {
    /// Resolves one canonical public profile without requesting tracks or artwork.
    ///
    /// # Errors
    /// Rejects unsafe profile URLs, mismatched identity, malformed metadata and HTTP failures.
    pub fn resolve_artist(&self, url: &Url) -> Result<SoundcloakArtist, ProviderError> {
        profile_slug(url)?;
        let value = self.resolve_catalog_value(url)?;
        if value["kind"].as_str() != Some("user") || !public_metadata(&value) {
            return Err(invalid_response("expected public artist metadata"));
        }
        let webpage_url = user_profile_url(&value)
            .filter(|actual| actual == url)
            .ok_or_else(|| invalid_response("artist identity does not match requested profile"))?;
        Ok(SoundcloakArtist {
            id: numeric_id(&value["id"]).ok_or_else(|| invalid_response("invalid artist ID"))?,
            name: value["username"]
                .as_str()
                .and_then(|text| safe_label(text, MAX_LABEL_BYTES))
                .ok_or_else(|| invalid_response("invalid artist display name"))?,
            webpage_url,
            track_count: value["track_count"].as_u64(),
        })
    }

    /// Constructs a profile's human-readable instance page without fetching it.
    ///
    /// # Errors
    /// Rejects anything other than a canonical public original profile URL.
    pub fn profile_page_url(&self, url: &Url) -> Result<Url, ProviderError> {
        self.endpoint(profile_slug(url)?)
    }

    /// Fetches one raw public artist-track page using a validated owned cursor.
    ///
    /// # Errors
    /// Rejects invalid owners, limits, cursors, envelopes and transport failures.
    pub fn artist_tracks(
        &self,
        artist: &SoundcloakArtist,
        limit: usize,
        cursor: Option<&SoundcloakCatalogCursor>,
    ) -> Result<SoundcloakArtistTrackPage, ProviderError> {
        let (value, next_cursor) =
            self.artist_catalog(artist, CatalogKind::Tracks, limit, cursor)?;
        let items = value["collection"]
            .as_array()
            .ok_or_else(|| invalid_response("missing artist track collection"))?
            .iter()
            .filter(|value| public_metadata(value))
            .filter_map(|value| self.normalize_track(value))
            .collect();
        Ok(SoundcloakArtistTrackPage { items, next_cursor })
    }

    /// Fetches one raw page of explicitly classified albums, EPs and compilations.
    ///
    /// # Errors
    /// Rejects invalid owners, limits, cursors, envelopes and transport failures.
    pub fn artist_albums(
        &self,
        artist: &SoundcloakArtist,
        limit: usize,
        cursor: Option<&SoundcloakCatalogCursor>,
    ) -> Result<SoundcloakArtistAlbumPage, ProviderError> {
        let (value, next_cursor) =
            self.artist_catalog(artist, CatalogKind::Albums, limit, cursor)?;
        let items = value["collection"]
            .as_array()
            .ok_or_else(|| invalid_response("missing artist album collection"))?
            .iter()
            .filter_map(|value| self.normalize_album(value))
            .collect();
        Ok(SoundcloakArtistAlbumPage { items, next_cursor })
    }

    /// Resolves a public album and preserves its ordered, possibly abbreviated track slots.
    ///
    /// # Errors
    /// Rejects unsafe/mismatched release identities, oversized albums and transport failures.
    pub fn resolve_album(&self, url: &Url) -> Result<SoundcloakAlbumDetails, ProviderError> {
        album_segments(url)?;
        let value = self.resolve_catalog_value(url)?;
        let album = self
            .normalize_album(&value)
            .filter(|album| album.webpage_url == *url)
            .ok_or_else(|| {
                invalid_response("album identity does not match requested public release")
            })?;
        let raw_tracks = value["tracks"]
            .as_array()
            .filter(|tracks| tracks.len() <= MAX_ALBUM_TRACKS)
            .ok_or_else(|| invalid_response("missing or oversized album track list"))?;
        let tracks = raw_tracks
            .iter()
            .map(|value| {
                let id = numeric_id(&value["id"])
                    .ok_or_else(|| invalid_response("invalid album track ID"))?;
                let track = public_metadata(value)
                    .then(|| self.normalize_track(value))
                    .flatten();
                Ok(SoundcloakAlbumTrack { id, track })
            })
            .collect::<Result<Vec<_>, ProviderError>>()?;
        Ok(SoundcloakAlbumDetails { album, tracks })
    }

    /// Hydrates only one visible album slice, in batches of at most fifty missing IDs.
    ///
    /// # Errors
    /// Rejects invalid ranges/identities, malformed batch responses and transport failures.
    pub fn album_tracks(
        &self,
        album: &SoundcloakAlbumDetails,
        offset: usize,
        limit: usize,
    ) -> Result<SoundcloakAlbumTrackPage, ProviderError> {
        album_segments(&album.album.webpage_url)?;
        if !(1..=100).contains(&limit)
            || offset > album.tracks.len()
            || album.tracks.len() > MAX_ALBUM_TRACKS
            || !valid_id(&album.album.id)
        {
            return Err(ProviderError::InvalidRequest(
                "invalid album page range or identity".into(),
            ));
        }
        let end = offset.saturating_add(limit).min(album.tracks.len());
        let mut items = album.tracks[offset..end].to_vec();
        if items.iter().any(|slot| {
            !valid_id(&slot.id)
                || slot.track.as_ref().is_some_and(|track| {
                    track.id != slot.id || canonical_segments(&track.webpage_url).is_err()
                })
        }) {
            return Err(ProviderError::InvalidRequest(
                "invalid album track slot".into(),
            ));
        }
        let mut missing = Vec::new();
        for slot in &items {
            if slot.track.is_none() && !missing.contains(&slot.id) {
                missing.push(slot.id.clone());
            }
        }
        for ids in missing.chunks(50) {
            let mut endpoint = self.endpoint("_/api/v2/tracks")?;
            endpoint
                .query_pairs_mut()
                .append_pair("ids", &ids.join(","));
            let value = self.fetch_json(&endpoint)?;
            let tracks = value
                .as_array()
                .filter(|tracks| tracks.len() <= ids.len())
                .ok_or_else(|| invalid_response("invalid hydrated track batch"))?;
            let mut seen = HashSet::new();
            for value in tracks {
                let id = numeric_id(&value["id"])
                    .filter(|id| ids.contains(id))
                    .ok_or_else(|| invalid_response("hydrated track was not requested"))?;
                if !seen.insert(id.clone()) {
                    return Err(invalid_response("duplicate hydrated track identity"));
                }
                let track = public_metadata(value)
                    .then(|| self.normalize_track(value))
                    .flatten();
                for slot in items.iter_mut().filter(|slot| slot.id == id) {
                    slot.track.clone_from(&track);
                }
            }
        }
        Ok(SoundcloakAlbumTrackPage {
            items,
            next_offset: (end < album.tracks.len()).then_some(end),
        })
    }

    /// Resolves only an already-validated public identity through the configured instance.
    fn resolve_catalog_value(&self, url: &Url) -> Result<Value, ProviderError> {
        let mut endpoint = self.endpoint("_/api/v2/resolve")?;
        endpoint.query_pairs_mut().append_pair("url", url.as_str());
        self.fetch_json(&endpoint)
    }

    /// Shares bounded raw artist-list retrieval while keeping pagination source-specific.
    fn artist_catalog(
        &self,
        artist: &SoundcloakArtist,
        kind: CatalogKind,
        limit: usize,
        cursor: Option<&SoundcloakCatalogCursor>,
    ) -> Result<(Value, Option<SoundcloakCatalogCursor>), ProviderError> {
        profile_slug(&artist.webpage_url)?;
        if !valid_id(&artist.id)
            || !(1..=100).contains(&limit)
            || cursor.is_some_and(|cursor| {
                cursor.owner_id != artist.id
                    || cursor.owner_url != artist.webpage_url
                    || cursor.kind != kind
                    || cursor.limit != limit
            })
        {
            return Err(ProviderError::InvalidRequest(
                "invalid artist catalogue owner, limit or cursor".into(),
            ));
        }
        let path = format!("/users/{}/{}", artist.id, kind.path());
        let mut endpoint = self.endpoint(&format!("_/api/v2{path}"))?;
        {
            let mut query = endpoint.query_pairs_mut();
            query.append_pair("limit", &limit.to_string());
            if let Some(cursor) = cursor {
                query.append_pair("offset", &cursor.offset);
                if let Some(token) = &cursor.query_urn {
                    query.append_pair("query_urn", token);
                }
            }
        }
        let value = self.fetch_json(&endpoint)?;
        value["collection"]
            .as_array()
            .filter(|items| items.len() <= limit)
            .ok_or_else(|| invalid_response("missing or oversized artist collection"))?;
        let next = catalog_continuation(&value, artist, kind, limit, cursor, &path)?;
        Ok((value, next))
    }

    /// Keeps album facts separate from track admission and discards non-album playlists.
    fn normalize_album(&self, value: &Value) -> Option<SoundcloakAlbum> {
        if value["kind"].as_str() != Some("playlist")
            || value["is_album"].as_bool() != Some(true)
            || !public_metadata(value)
        {
            return None;
        }
        let webpage_url = Url::parse(value["permalink_url"].as_str()?).ok()?;
        album_segments(&webpage_url).ok()?;
        Some(SoundcloakAlbum {
            id: numeric_id(&value["id"])?,
            title: safe_label(value["title"].as_str()?, MAX_LABEL_BYTES)?,
            webpage_url,
            artist: safe_label(value["user"]["username"].as_str()?, MAX_LABEL_BYTES)?,
            artist_url: user_profile_url(&value["user"]),
            artwork_url: value["artwork_url"]
                .as_str()
                .and_then(|raw| self.artwork_url(raw, "t500x500")),
            track_count: value["track_count"].as_u64(),
            release_type: value["set_type"]
                .as_str()
                .and_then(|text| safe_label(text, 64)),
        })
    }
}

/// Extracts only canonical public profile identity, never a display-name-derived URL.
pub(super) fn user_profile_url(user: &Value) -> Option<Url> {
    let permalink = user.get("permalink").filter(|value| !value.is_null());
    let url = if let Some(raw) = user.get("permalink_url").filter(|value| !value.is_null()) {
        Url::parse(raw.as_str()?).ok()?
    } else {
        let slug = permalink?.as_str()?;
        if !valid_profile_slug(slug) {
            return None;
        }
        Url::parse(&format!("https://soundcloud.com/{slug}")).ok()?
    };
    let slug = profile_slug(&url).ok()?;
    if permalink.is_some_and(|value| value.as_str() != Some(slug)) {
        return None;
    }
    Some(url)
}

/// Public JSON can omit visibility flags, but explicit private/token-bearing records are refused.
fn public_metadata(value: &Value) -> bool {
    value["public"].as_bool() != Some(false)
        && value["sharing"]
            .as_str()
            .is_none_or(|sharing| sharing == "public")
        && value
            .get("secret_token")
            .is_none_or(|token| token.is_null() || token.as_str() == Some(""))
}

/// Numeric API identifiers are canonical decimal strings, not arbitrary path components.
fn valid_id(id: &str) -> bool {
    id.len() <= 20
        && !id.starts_with('0')
        && id.bytes().all(|byte| byte.is_ascii_digit())
        && id.parse::<u64>().is_ok_and(|id| id > 0)
}

/// Extracts only positive integer JSON identifiers.
fn numeric_id(value: &Value) -> Option<String> {
    value.as_u64().filter(|id| *id > 0).map(|id| id.to_string())
}

/// Rejects site routes that cannot denote public artist profiles.
fn valid_profile_slug(slug: &str) -> bool {
    valid_slug(slug)
        && !matches!(
            slug,
            "search"
                | "discover"
                | "you"
                | "stream"
                | "upload"
                | "charts"
                | "stations"
                | "settings"
                | "people"
                | "groups"
                | "tags"
                | "pages"
        )
}

/// Shares the strict origin policy without weakening the existing track URL validator.
fn public_origin(url: &Url) -> bool {
    url.as_str().len() <= MAX_URL_BYTES
        && url.scheme() == "https"
        && url.host_str() == Some("soundcloud.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

/// Returns one canonical public profile segment, rejecting aliases and reserved site routes.
fn profile_slug(url: &Url) -> Result<&str, ProviderError> {
    let slug = url.path().strip_prefix('/').unwrap_or_default();
    if !public_origin(url) || !valid_profile_slug(slug) {
        return Err(ProviderError::InvalidRequest(
            "expected a canonical public SoundCloud profile URL".into(),
        ));
    }
    Ok(slug)
}

/// Validates an original release identity while keeping `/sets/` invalid for track playback.
fn album_segments(url: &Url) -> Result<(&str, &str), ProviderError> {
    let invalid =
        || ProviderError::InvalidRequest("expected a canonical public SoundCloud album URL".into());
    if !public_origin(url) {
        return Err(invalid());
    }
    let mut parts = url.path_segments().ok_or_else(invalid)?;
    let artist = parts.next().ok_or_else(invalid)?;
    let sets = parts.next().ok_or_else(invalid)?;
    let album = parts.next().ok_or_else(invalid)?;
    if sets != "sets" || !valid_profile_slug(artist) || !valid_slug(album) || parts.next().is_some()
    {
        return Err(invalid());
    }
    Ok((artist, album))
}

/// Keeps provider cursors bounded query values, never URLs or query-string fragments.
fn valid_offset(offset: &str) -> bool {
    !offset.is_empty()
        && offset.len() <= MAX_CURSOR_BYTES
        && offset.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.' | b',')
        })
}

/// Validates continuation ownership, then discards the upstream URL and client credentials.
fn catalog_continuation(
    value: &Value,
    artist: &SoundcloakArtist,
    kind: CatalogKind,
    limit: usize,
    cursor: Option<&SoundcloakCatalogCursor>,
    path: &str,
) -> Result<Option<SoundcloakCatalogCursor>, ProviderError> {
    let Some(raw) = value.get("next_href").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let raw = raw
        .as_str()
        .filter(|raw| raw.len() <= MAX_URL_BYTES)
        .ok_or_else(|| invalid_response("invalid artist continuation URL"))?;
    if raw.is_empty() {
        return Ok(None);
    }
    let url = Url::parse(raw).map_err(|_| invalid_response("invalid artist continuation URL"))?;
    if url.scheme() != "https"
        || url.host_str() != Some("api-v2.soundcloud.com")
        || url.path() != path
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid_response(
            "untrusted artist continuation owner or origin",
        ));
    }
    let mut offset = None;
    let mut next_limit = None;
    let mut token = value
        .get("query_urn")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .filter(|token| valid_query_urn(token))
                .map(str::to_owned)
                .ok_or_else(|| invalid_response("invalid artist continuation token"))
        })
        .transpose()?;
    let mut seen = HashSet::new();
    for (key, value) in url.query_pairs() {
        if matches!(key.as_ref(), "offset" | "limit" | "query_urn") && !seen.insert(key.to_string())
        {
            return Err(invalid_response("duplicate artist continuation parameter"));
        }
        match key.as_ref() {
            "offset" if valid_offset(&value) => offset = Some(value.into_owned()),
            "limit" => next_limit = value.parse::<usize>().ok(),
            "query_urn" => {
                if !valid_query_urn(&value) || token.as_ref().is_some_and(|token| token != &value) {
                    return Err(invalid_response("invalid artist continuation token"));
                }
                token = Some(value.into_owned());
            }
            _ => {} // Never forward client_id or other upstream request parameters.
        }
    }
    let offset =
        offset.ok_or_else(|| invalid_response("missing or invalid artist continuation offset"))?;
    let mut visited_offsets = cursor.map_or_else(Vec::new, |cursor| cursor.visited_offsets.clone());
    if next_limit != Some(limit) || visited_offsets.contains(&offset) {
        return Err(invalid_response(
            "artist continuation changed its limit or repeated a cursor",
        ));
    }
    if kind == CatalogKind::Albums {
        let previous = cursor.map_or(Some(0), |cursor| cursor.offset.parse::<usize>().ok());
        if offset.parse::<usize>().ok() != previous.and_then(|old| old.checked_add(limit)) {
            return Err(invalid_response(
                "album continuation has an unexpected offset",
            ));
        }
    }
    if visited_offsets.len() >= MAX_CATALOG_PAGES {
        return Ok(None);
    }
    visited_offsets.push(offset.clone());
    Ok(Some(SoundcloakCatalogCursor {
        owner_id: artist.id.clone(),
        owner_url: artist.webpage_url.clone(),
        kind,
        limit,
        offset,
        query_urn: token.or_else(|| cursor.and_then(|cursor| cursor.query_urn.clone())),
        visited_offsets,
    }))
}

#[cfg(test)]
mod tests;
