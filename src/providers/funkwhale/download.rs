//! Fresh public audio locators for explicit, original-pod manual downloads.

use super::*;

/// An advertised upload selected for one in-memory transfer, never durable intent.
pub(crate) struct FunkwhaleDownloadSource {
    pub(crate) url: Url,
    pub(crate) extension: Option<String>,
    pub(crate) mimetype: Option<String>,
    pub(crate) size: Option<u64>,
    pub(crate) thumbnail_url: Option<Url>,
    /// A provider metadata hint for the chooser, never proof of the downloaded codec.
    pub(crate) advertised_opus: bool,
}

impl FunkwhaleProvider {
    /// Resolves a fresh upload without passing the track's HTML page to an extractor.
    ///
    /// Opus-copy requests prefer an explicitly advertised Opus upload. This never
    /// requests a server-side transcode; its actual bytes are verified by the
    /// downloader before publication. A documented listen endpoint is pinned to
    /// the chosen upload UUID so its original encoding and size remain meaningful.
    pub(crate) fn download_source(
        &self,
        identity: &str,
        prefer_opus: bool,
    ) -> Result<FunkwhaleDownloadSource, ProviderError> {
        let track_id = self.numeric_track_id(identity)?;
        self.with_detected_api(|version| {
            let raw: RawTrack = get_bounded_json(
                &self.agent,
                &self.build_track_url(&track_id, version)?,
                self.max_json_bytes,
            )?;
            if raw.id.to_string() != track_id {
                return Err(ProviderError::InvalidResponse(
                    "Funkwhale returned a different track identity".to_owned(),
                ));
            }
            self.convert_download_source(&raw, prefer_opus)
        })
    }

    /// Retains original-upload metadata without inventing file extensions from URL paths.
    fn convert_download_source(
        &self,
        raw: &RawTrack,
        prefer_opus: bool,
    ) -> Result<FunkwhaleDownloadSource, ProviderError> {
        validate_raw_track(raw)?;
        if raw.is_playable == Some(false) {
            return Err(ProviderError::InvalidResponse(
                "This Funkwhale track has no public audio upload".to_owned(),
            ));
        }
        let selected = prefer_opus
            .then(|| raw.uploads.iter().find(|upload| advertised_opus(upload)))
            .flatten()
            .or_else(|| raw.uploads.first());
        let advertised = selected
            .and_then(|upload| upload.listen_url.as_deref())
            .or(raw.listen_url.as_deref())
            .ok_or_else(|| {
                ProviderError::InvalidResponse(
                    "This Funkwhale track does not advertise public audio".to_owned(),
                )
            })?;
        let mut url = resolve_http_url(&self.base_url, advertised)?;
        let own_listen = url.origin() == self.base_url.origin()
            && ["api/v1/listen/", "api/v2/listen/"].iter().any(|path| {
                self.base_url
                    .join(path)
                    .is_ok_and(|prefix| url.path().starts_with(prefix.path()))
            });
        if own_listen {
            let pairs = url
                .query_pairs()
                .filter(|(key, _)| !matches!(key.as_ref(), "upload" | "download" | "to"))
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect::<Vec<_>>();
            url.set_query(None);
            let mut query = url.query_pairs_mut();
            query.extend_pairs(pairs).append_pair("download", "true");
            if let Some(uuid) = selected
                .and_then(|upload| upload.uuid.as_deref())
                .filter(|uuid| valid_upload_uuid(uuid))
            {
                query.append_pair("upload", uuid);
            }
        }
        Ok(FunkwhaleDownloadSource {
            url,
            extension: selected.and_then(|upload| upload.extension.clone()),
            mimetype: selected.and_then(|upload| upload.mimetype.clone()),
            size: selected.and_then(|upload| upload.size),
            thumbnail_url: self
                .track_thumbnails(raw)
                .first()
                .map(|thumbnail| thumbnail.url.clone()),
            advertised_opus: selected.is_some_and(advertised_opus),
        })
    }
}

/// Only explicit encoding hints affect selection; Ogg alone does not imply Opus.
fn advertised_opus(upload: &RawUpload) -> bool {
    upload
        .extension
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("opus"))
        || upload
            .codec
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("opus"))
        || upload
            .mimetype
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("audio/opus"))
}

/// A bounded upload revision is data, not a free-form query fragment.
fn valid_upload_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extensionless signed listen endpoints remain transient and select one original upload.
    #[test]
    fn download_resolution_pins_the_selected_original_and_never_requests_transcoding() {
        let provider =
            FunkwhaleProvider::new(Url::parse("https://pod.example/prefix/").unwrap()).unwrap();
        let track: RawTrack = serde_json::from_value(serde_json::json!({
            "id": 27, "title": "Track", "listen_url": "/prefix/api/v2/listen/track/?signature=private&to=mp3&download=false",
            "uploads": [
                {"uuid": "12345678-1234-1234-1234-123456789abc", "extension": "flac", "size": 123},
                {"uuid": "abcdefab-abcd-abcd-abcd-abcdefabcdef", "extension": "opus", "size": 99}
            ]
        })).unwrap();
        let original = provider.convert_download_source(&track, false).unwrap();
        assert_eq!(original.extension.as_deref(), Some("flac"));
        assert_eq!(original.size, Some(123));
        assert!(!original.advertised_opus);
        let query: std::collections::HashMap<_, _> = original.url.query_pairs().collect();
        assert_eq!(
            query.get("upload").map(|value| value.as_ref()),
            Some("12345678-1234-1234-1234-123456789abc")
        );
        assert_eq!(
            query.get("download").map(|value| value.as_ref()),
            Some("true")
        );
        assert_eq!(
            query.get("signature").map(|value| value.as_ref()),
            Some("private")
        );
        assert!(!query.contains_key("to"));
        let opus = provider.convert_download_source(&track, true).unwrap();
        assert_eq!(opus.extension.as_deref(), Some("opus"));
        assert_eq!(opus.size, Some(99));
        assert!(opus.advertised_opus);
    }

    /// Direct CDN locators are not Funkwhale REST endpoints and must not receive API parameters.
    #[test]
    fn download_resolution_keeps_an_advertised_cdn_locator_unchanged() {
        let provider = FunkwhaleProvider::new(Url::parse("https://pod.example/").unwrap()).unwrap();
        let track: RawTrack = serde_json::from_value(serde_json::json!({
            "id": 27, "title": "Track", "listen_url": "https://cdn.example/object?signature=private",
            "uploads": [{"uuid": "12345678-1234-1234-1234-123456789abc", "extension": "mp3"}]
        })).unwrap();
        let source = provider.convert_download_source(&track, false).unwrap();
        assert_eq!(
            source.url.as_str(),
            "https://cdn.example/object?signature=private"
        );
    }
}
