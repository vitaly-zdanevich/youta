//! Exact artist album pages and explicitly requested album-less track browsing.

use super::*;

/// A catalogue album, deliberately distinct from a playable track identity.
#[derive(Clone, Debug)]
pub struct FunkwhaleAlbum {
    /// Canonical original-pod album page, never an audio locator.
    pub url: Url,
    /// Album title normalized for display.
    pub title: String,
    /// Public album description converted to safe plain text.
    pub description: String,
    /// Advertised track count, when the pod supplies it.
    pub tracks_count: Option<u64>,
    /// Advertised album release date, without substituting an upload date.
    pub release_date: Option<String>,
    /// Advertised album artwork at available sizes.
    pub thumbnails: Vec<Thumbnail>,
}

/// One explicitly requested album page and its advertised continuation.
#[derive(Clone, Debug)]
pub struct FunkwhaleAlbumPage {
    /// One-based requested page number.
    pub page: u32,
    /// Playable catalogue albums in the pod's returned order.
    pub items: Vec<FunkwhaleAlbum>,
    /// A later page, only when pagination indicates more results.
    pub next_page: Option<u32>,
}

#[derive(Deserialize)]
struct RawCatalogueAlbum {
    #[serde(default, deserialize_with = "deserialize_u64")]
    id: u64,
    title: String,
    #[serde(default)]
    description: Option<RawContent>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    tracks_count: Option<u64>,
    #[serde(default)]
    release_date: Option<String>,
    #[serde(default)]
    cover: Option<RawCover>,
    #[serde(default)]
    is_playable: Option<Value>,
}

impl FunkwhaleProvider {
    /// Lists an exact artist's albums without flattening them into track rows.
    ///
    /// # Errors
    ///
    /// Rejects invalid pagination and cross-pod artist identities, and propagates bounded API failures.
    pub fn artist_albums(
        &self,
        artist: &str,
        page: u32,
    ) -> Result<FunkwhaleAlbumPage, ProviderError> {
        self.with_detected_api(|version| {
            let url = self.build_artist_albums_url(artist, page, version)?;
            let raw = get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
            self.convert_album_page(raw, page)
        })
    }

    /// Checks for album-less tracks with one bounded exact-artist request.
    ///
    /// Funkwhale uses PostgreSQL, whose descending order puts NULL albums first.
    /// The documented `-album__title` ordering therefore finds a standalone track
    /// even when ordinary album ordering would place it on a later page. No
    /// unsupported null filter or catalogue scan is needed.
    ///
    /// # Errors
    ///
    /// Rejects cross-pod artists, inconsistent visibility or pagination, and API failures.
    pub fn artist_has_standalone_tracks(&self, artist: &str) -> Result<bool, ProviderError> {
        self.with_detected_api(|version| {
            let url = self.build_standalone_presence_url(artist, version)?;
            let raw: RawPage<RawTrack> = get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
            Self::standalone_presence(raw)
        })
    }

    /// Requests only the first null-first record on the original artist's pod.
    fn build_standalone_presence_url(
        &self,
        artist: &str,
        version: u8,
    ) -> Result<Url, ProviderError> {
        let id = self.artist_numeric_id(artist)?;
        let mut url = self.api_url(version, "tracks")?;
        url.query_pairs_mut()
            .append_pair("artist", &id)
            .append_pair("playable", "true")
            .append_pair("include_channels", "true")
            .append_pair("page", "1")
            .append_pair("page_size", "1")
            .append_pair("ordering", "-album__title");
        Ok(url)
    }

    /// Never mistakes an inconsistent or explicitly unplayable result for proven emptiness.
    fn standalone_presence(raw: RawPage<RawTrack>) -> Result<bool, ProviderError> {
        let Some(track) = raw.results.first() else {
            if raw.count > 0
                || raw
                    .next
                    .as_ref()
                    .is_some_and(|next| !next.trim().is_empty())
            {
                return Err(ProviderError::InvalidResponse(
                    "Cannot determine standalone tracks from an empty non-final page".to_owned(),
                ));
            }
            return Ok(false);
        };
        if track.album.is_none() && track.is_playable == Some(false) {
            return Err(ProviderError::InvalidResponse(
                "Cannot determine standalone tracks: pod ignored playable filter".to_owned(),
            ));
        }
        Ok(track.album.is_none())
    }

    /// Requests one artist-track page and retains only tracks with no album.
    ///
    /// The official API has no album-less filter. This does not invent a query parameter or
    /// scan additional pages: even an empty filtered page retains its explicit continuation.
    ///
    /// # Errors
    ///
    /// Rejects invalid or cross-pod artist identities, and propagates bounded API failures.
    pub fn standalone_artist_tracks(
        &self,
        artist: &str,
        page: u32,
    ) -> Result<SearchPage, ProviderError> {
        self.with_detected_api(|version| {
            let url = self.build_catalogue_url(Some(artist), page, version)?;
            let raw: RawPage<RawTrack> = get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
            self.convert_standalone_page(raw, page)
        })
    }

    /// Filters one page while retaining pagination from its original unfiltered response.
    fn convert_standalone_page(
        &self,
        mut raw: RawPage<RawTrack>,
        page: u32,
    ) -> Result<SearchPage, ProviderError> {
        let has_more = page_has_more(
            page,
            raw.results.len(),
            raw.count,
            raw.next.as_deref(),
            self.page_size,
        );
        raw.results.retain(|track| track.album.is_none());
        let mut converted = self.convert_track_page(raw, page)?;
        converted.next_page = has_more.then_some(page.saturating_add(1));
        Ok(converted)
    }

    /// Builds the documented album collection query with an exact artist filter.
    fn build_artist_albums_url(
        &self,
        artist: &str,
        page: u32,
        version: u8,
    ) -> Result<Url, ProviderError> {
        if page == 0 {
            return Err(ProviderError::InvalidRequest(
                "Funkwhale page must be positive".to_owned(),
            ));
        }
        let id = self.artist_numeric_id(artist)?;
        let mut url = self.api_url(version, "albums")?;
        url.query_pairs_mut()
            .append_pair("artist", &id)
            .append_pair("playable", "true")
            .append_pair("page", &page.to_string())
            .append_pair("page_size", &self.page_size.to_string())
            .append_pair("ordering", "-release_date");
        Ok(url)
    }

    /// Converts both stable boolean and deployed string-valued playability without inventing audio.
    fn convert_album_page(
        &self,
        raw: RawPage<RawCatalogueAlbum>,
        page: u32,
    ) -> Result<FunkwhaleAlbumPage, ProviderError> {
        let has_more = page_has_more(
            page,
            raw.results.len(),
            raw.count,
            raw.next.as_deref(),
            self.page_size,
        );
        let items = raw
            .results
            .into_iter()
            .filter(|album| {
                !matches!(album.is_playable.as_ref(), Some(Value::Bool(false)))
                    && !album
                        .is_playable
                        .as_ref()
                        .and_then(Value::as_str)
                        .is_some_and(|value| value.eq_ignore_ascii_case("false"))
            })
            .map(|album| {
                if album.id == 0 || album.title.trim().is_empty() {
                    return Err(ProviderError::InvalidResponse(
                        "Funkwhale album requires a positive ID and title".to_owned(),
                    ));
                }
                Ok(FunkwhaleAlbum {
                    url: self.local_webpage("albums", album.id).ok_or_else(|| {
                        ProviderError::InvalidResponse(
                            "Funkwhale album has no canonical page".to_owned(),
                        )
                    })?,
                    title: display_text(&album.title),
                    description: content_text(album.description),
                    tracks_count: album.tracks_count,
                    release_date: album.release_date.filter(|value| !value.trim().is_empty()),
                    thumbnails: self.cover_thumbnails(album.cover.as_ref()),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(FunkwhaleAlbumPage {
            page,
            items,
            next_page: has_more.then_some(page.saturating_add(1)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Uses an isolated URL without any network request for parser and query contracts.
    fn provider() -> FunkwhaleProvider {
        FunkwhaleProvider::new(Url::parse("https://pod.example/").unwrap()).unwrap()
    }

    #[test]
    fn artist_album_query_uses_exact_artist_not_credit_identity() {
        let provider = provider().with_page_size(20);
        let url = provider
            .build_artist_albums_url("https://pod.example/library/artists/5796", 2, API_V2)
            .unwrap();
        assert_eq!(url.path(), "/api/v2/albums/");
        for (key, value) in [
            ("artist", "5796"),
            ("page", "2"),
            ("page_size", "20"),
            ("playable", "true"),
            ("ordering", "-release_date"),
        ] {
            assert!(
                url.query_pairs()
                    .any(|(actual_key, actual_value)| actual_key == key && actual_value == value)
            );
        }
        assert!(!url.query_pairs().any(|(key, _)| key == "artist_credit"));
        assert!(
            provider
                .build_artist_albums_url("https://other.example/library/artists/5796", 1, API_V2)
                .is_err()
        );
        assert!(
            provider
                .build_artist_albums_url("https://pod.example/library/artists/5796", 0, API_V2)
                .is_err()
        );
    }

    #[test]
    fn standalone_probe_is_one_null_first_record_for_the_exact_original_artist() {
        let provider = provider().with_page_size(100);
        let url = provider
            .build_standalone_presence_url("https://pod.example/library/artists/5796", API_V2)
            .unwrap();
        assert_eq!(url.path(), "/api/v2/tracks/");
        let query = url.query_pairs().collect::<BTreeMap<_, _>>();
        for (key, value) in [
            ("artist", "5796"),
            ("page", "1"),
            ("page_size", "1"),
            ("playable", "true"),
            ("include_channels", "true"),
            ("ordering", "-album__title"),
        ] {
            assert_eq!(query.get(key).map(|value| value.as_ref()), Some(value));
        }
        assert_eq!(query.len(), 6, "no guessed null-album filter");
        assert!(
            provider
                .build_standalone_presence_url("https://other.example/library/artists/5796", API_V2)
                .is_err()
        );
    }

    #[test]
    fn standalone_presence_distinguishes_proven_empty_present_and_inconsistent_pages() {
        let check =
            |value| FunkwhaleProvider::standalone_presence(serde_json::from_value(value).unwrap());
        assert!(!check(serde_json::json!({"count": 0, "results": []})).unwrap());
        assert!(!check(serde_json::json!({"count": 22, "results": [{"id": 1, "title": "Bound", "album": {"id": 44}}]})).unwrap());
        assert!(check(serde_json::json!({"count": 999, "results": [{"id": 1, "title": "Standalone", "album": null, "is_playable": true}]})).unwrap());
        assert!(check(serde_json::json!({"count": 1, "results": []})).is_err());
        assert!(check(serde_json::json!({"next": "https://pod.example/api/v2/tracks/?page=2", "results": []})).is_err());
        assert!(check(serde_json::json!({"results": [{"id": 1, "title": "Hidden", "album": null, "is_playable": false}]})).is_err());
    }

    #[test]
    fn album_page_supports_adl_string_playability_and_cursor_without_count() {
        let raw = serde_json::from_value(serde_json::json!({
            "next": "https://pod.example/api/v2/albums/?page=2",
            "results": [{"id": 4408, "title": "ADL", "tracks_count": 22, "is_playable": "true"}, {"id": 4409, "title": "Hidden", "is_playable": false}],
        })).unwrap();
        let page = provider().convert_album_page(raw, 1).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].title, "ADL");
        assert_eq!(page.items[0].tracks_count, Some(22));
        assert_eq!(
            page.items[0].url.as_str(),
            "https://pod.example/library/albums/4408"
        );
        assert_eq!(page.next_page, Some(2));
    }

    #[test]
    fn standalone_pages_exclude_bound_tracks_without_losing_empty_page_continuations() {
        let track = |id, album: Value| {
            serde_json::json!({
                "id": id, "title": format!("Track {id}"), "artist": {"id": 5794, "name": "Vortex"},
                "is_playable": true, "album": album,
            })
        };
        let provider = provider();
        let raw = serde_json::from_value(serde_json::json!({
            "count": 2,
            "results": [track(27084, Value::Null), track(27083, serde_json::json!({"id": 999, "title": "Bound"}))],
        })).unwrap();
        let page = provider.convert_standalone_page(raw, 1).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(
            page.next_page, None,
            "do not create a next page from filtered result count"
        );

        let raw = serde_json::from_value(serde_json::json!({
            "next": "https://pod.example/api/v2/tracks/?page=2",
            "results": [track(27083, serde_json::json!({"id": 999, "title": "Bound"}))],
        }))
        .unwrap();
        let page = provider.convert_standalone_page(raw, 1).unwrap();
        assert!(page.items.is_empty());
        assert_eq!(page.next_page, Some(2));
    }
}
