//! `DeArrow` alternate-title client.
//!
//! `DeArrow` titles are optional presentation metadata. `Youta` retains and
//! labels the original title so toggling `DeArrow` never overwrites source
//! metadata.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;

use super::{
    DEFAULT_MAX_JSON_BYTES, DEFAULT_REQUEST_TIMEOUT, ProviderError, get_bounded_json,
    provider_agent, validate_base_url, validate_youtube_video_id,
};

const MAX_CONFIGURED_JSON_BYTES: usize = 8 * 1024 * 1024;

/// `DeArrow`'s public API URL.
pub const DEFAULT_DEARROW_URL: &str = "https://sponsor.ajay.app/";

/// Provenance label for a displayed title.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    /// The title reported by the media provider.
    Original,
    /// A crowdsourced `DeArrow` anti-clickbait title.
    DeArrow,
}

/// A title coupled to an explicit provenance label.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LabeledTitle {
    /// Displayable title text.
    pub text: String,
    /// Where the title came from.
    pub source: TitleSource,
}

/// A trusted `DeArrow` title and its moderation metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeArrowTitle {
    /// Explicitly labelled title.
    pub labeled: LabeledTitle,
    /// Current `DeArrow` vote score.
    pub votes: i64,
    /// Whether a moderator locked the submission.
    pub locked: bool,
    /// Stable `DeArrow` submission identifier.
    pub uuid: String,
}

/// Original and optional `DeArrow` titles kept side by side.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DisplayTitles {
    /// Original provider title; this is never replaced or discarded.
    pub original: LabeledTitle,
    /// Best trusted `DeArrow` submission, when one exists.
    pub dearrow: Option<DeArrowTitle>,
}

impl DisplayTitles {
    /// Returns the title selected by the user's `DeArrow` toggle.
    #[must_use]
    pub fn preferred(&self, use_dearrow: bool) -> &LabeledTitle {
        if use_dearrow && let Some(dearrow) = &self.dearrow {
            return &dearrow.labeled;
        }
        &self.original
    }
}

/// Blocking read-only client for the `DeArrow` branding endpoint.
#[derive(Clone)]
pub struct DeArrowClient {
    base_url: Url,
    agent: ureq::Agent,
    max_json_bytes: usize,
}

impl DeArrowClient {
    /// Creates a client for a configured DeArrow-compatible service.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] when the base URL is invalid.
    pub fn new(base_url: Url) -> Result<Self, ProviderError> {
        Self::with_options(base_url, DEFAULT_REQUEST_TIMEOUT, DEFAULT_MAX_JSON_BYTES)
    }

    /// Creates a client with explicit timeout and JSON response bound.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] when the URL, timeout, or size limit is
    /// invalid.
    pub fn with_options(
        base_url: Url,
        timeout: Duration,
        max_json_bytes: usize,
    ) -> Result<Self, ProviderError> {
        let base_url = validate_base_url(base_url)?;
        if timeout.is_zero() {
            return Err(ProviderError::InvalidRequest(
                "DeArrow timeout must be greater than zero".to_owned(),
            ));
        }
        if !(1..=MAX_CONFIGURED_JSON_BYTES).contains(&max_json_bytes) {
            return Err(ProviderError::InvalidRequest(format!(
                "JSON response limit must be between 1 and {MAX_CONFIGURED_JSON_BYTES} bytes"
            )));
        }
        Ok(Self {
            base_url,
            agent: provider_agent(timeout),
            max_json_bytes,
        })
    }

    /// Returns the normalized configured service URL.
    #[must_use]
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// Fetches and labels the best trusted alternate title.
    ///
    /// The original title is returned even when `DeArrow` has no submission or
    /// responds with HTTP 404.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] for invalid input, transport failure, an
    /// unsuccessful non-404 status, or an invalid bounded response.
    pub fn titles(
        &self,
        video_id: &str,
        original_title: impl Into<String>,
    ) -> Result<DisplayTitles, ProviderError> {
        let original_title = original_title.into();
        validate_original_title(&original_title)?;
        let url = self.build_branding_url(video_id)?;
        let response: RawBrandingResponse =
            match get_bounded_json(&self.agent, &url, self.max_json_bytes) {
                Ok(response) => response,
                Err(ProviderError::HttpStatus(404)) => {
                    return Ok(DisplayTitles {
                        original: LabeledTitle {
                            text: original_title,
                            source: TitleSource::Original,
                        },
                        dearrow: None,
                    });
                }
                Err(error) => return Err(error),
            };
        Ok(select_titles(original_title, response))
    }

    fn build_branding_url(&self, video_id: &str) -> Result<Url, ProviderError> {
        validate_youtube_video_id(video_id)?;
        let mut url = self
            .base_url
            .join("api/branding")
            .map_err(|error| ProviderError::InvalidBaseUrl(error.to_string()))?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("videoID", video_id);
            query.append_pair("service", "YouTube");
            query.append_pair("fetchAll", "false");
        }
        Ok(url)
    }
}

fn validate_original_title(title: &str) -> Result<(), ProviderError> {
    if title.trim().is_empty() {
        return Err(ProviderError::InvalidRequest(
            "original video title cannot be empty".to_owned(),
        ));
    }
    if title.len() > 4096 {
        return Err(ProviderError::InvalidRequest(
            "original video title cannot exceed 4096 bytes".to_owned(),
        ));
    }
    Ok(())
}

fn select_titles(original_title: String, response: RawBrandingResponse) -> DisplayTitles {
    let dearrow = response.titles.into_iter().find_map(|candidate| {
        if candidate.original || (!candidate.locked && candidate.votes < 0) {
            return None;
        }
        let text = normalize_dearrow_title(&candidate.title)?;
        Some(DeArrowTitle {
            labeled: LabeledTitle {
                text,
                source: TitleSource::DeArrow,
            },
            votes: candidate.votes,
            locked: candidate.locked,
            uuid: candidate.uuid,
        })
    });

    DisplayTitles {
        original: LabeledTitle {
            text: original_title,
            source: TitleSource::Original,
        },
        dearrow,
    }
}

fn normalize_dearrow_title(title: &str) -> Option<String> {
    let normalized = title.replace('>', "");
    let normalized = normalized.trim();
    (!normalized.is_empty()).then(|| normalized.to_owned())
}

#[derive(Debug, Deserialize)]
struct RawBrandingResponse {
    #[serde(default)]
    titles: Vec<RawTitleSubmission>,
}

#[derive(Debug, Deserialize)]
struct RawTitleSubmission {
    title: String,
    #[serde(default)]
    original: bool,
    #[serde(default)]
    votes: i64,
    #[serde(default)]
    locked: bool,
    #[serde(rename = "UUID")]
    uuid: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRANDING_FIXTURE: &str = r#"{
		"titles": [
			{"title": "Original from API", "original": true, "votes": 100, "locked": true, "UUID": "original"},
			{"title": "Untrusted", "original": false, "votes": -1, "locked": false, "UUID": "untrusted"},
			{"title": "A >Clearer Title", "original": false, "votes": 4, "locked": false, "UUID": "trusted"}
		],
		"thumbnails": [],
		"randomTime": 0.5,
		"videoDuration": null
	}"#;

    fn client() -> DeArrowClient {
        DeArrowClient::new(
            Url::parse("https://dearrow.example.test/prefix").expect("fixture URL should parse"),
        )
        .expect("fixture client should construct")
    }

    #[test]
    fn branding_url_is_bounded_to_read_only_lookup_parameters() {
        let url = client()
            .build_branding_url("dQw4w9WgXcQ")
            .expect("video ID should be valid");
        let pairs = url.query_pairs().collect::<Vec<_>>();

        assert_eq!(url.path(), "/prefix/api/branding");
        assert!(pairs.contains(&("videoID".into(), "dQw4w9WgXcQ".into())));
        assert!(pairs.contains(&("fetchAll".into(), "false".into())));
    }

    #[test]
    fn trusted_alternate_is_selected_and_original_is_retained() {
        let response = serde_json::from_str(BRANDING_FIXTURE).expect("fixture should deserialize");
        let titles = select_titles("Provider Original".to_owned(), response);

        assert_eq!(titles.original.text, "Provider Original");
        assert_eq!(titles.original.source, TitleSource::Original);
        let alternate = titles.dearrow.expect("trusted alternate should exist");
        assert_eq!(alternate.labeled.text, "A Clearer Title");
        assert_eq!(alternate.labeled.source, TitleSource::DeArrow);
        assert_eq!(alternate.uuid, "trusted");
    }

    #[test]
    fn preference_toggle_returns_explicitly_labelled_title() {
        let response = serde_json::from_str(BRANDING_FIXTURE).expect("fixture should deserialize");
        let titles = select_titles("Provider Original".to_owned(), response);

        assert_eq!(titles.preferred(false).source, TitleSource::Original);
        assert_eq!(titles.preferred(true).source, TitleSource::DeArrow);
    }

    #[test]
    fn all_untrusted_titles_leave_original_untouched() {
        let response = serde_json::from_str(
			r#"{"titles":[{"title":"Rejected","original":false,"votes":-2,"locked":false,"UUID":"no"}]}"#,
		)
		.expect("fixture should deserialize");
        let titles = select_titles("Original".to_owned(), response);

        assert!(titles.dearrow.is_none());
        assert_eq!(titles.preferred(true).text, "Original");
    }

    #[test]
    fn locked_negative_title_is_still_trusted() {
        let response = serde_json::from_str(
			r#"{"titles":[{"title":"Locked","original":false,"votes":-10,"locked":true,"UUID":"locked"}]}"#,
		)
		.expect("fixture should deserialize");
        let titles = select_titles("Original".to_owned(), response);

        assert_eq!(
            titles
                .dearrow
                .expect("locked title should be selected")
                .labeled
                .text,
            "Locked"
        );
    }
}
