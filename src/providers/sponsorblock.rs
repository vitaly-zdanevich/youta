//! `SponsorBlock` segment client and deterministic skip calculations.
//!
//! `SponsorBlock` skips creator-inserted segments such as sponsorships. It
//! cannot remove `YouTube`'s separately delivered platform advertisements.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;

use super::{
    DEFAULT_MAX_JSON_BYTES, DEFAULT_REQUEST_TIMEOUT, ProviderError, get_bounded_json,
    provider_agent, validate_base_url, validate_youtube_video_id,
};

const MAX_CONFIGURED_JSON_BYTES: usize = 16 * 1024 * 1024;

/// `SponsorBlock`'s public service URL.
pub const DEFAULT_SPONSORBLOCK_URL: &str = "https://sponsor.ajay.app/";

/// Crowdsourced segment classification.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SponsorCategory {
    /// Paid promotion or product placement.
    Sponsor,
    /// Unpaid or self promotion.
    SelfPromotion,
    /// Reminder to subscribe, like, or comment.
    Interaction,
    /// Intermission or opening animation.
    Intro,
    /// End cards or credits.
    Outro,
    /// Preview or recap.
    Preview,
    /// Non-music material in a music video.
    MusicOfftopic,
    /// A viewer-highlighted point of interest.
    PointOfInterest,
    /// Tangential filler.
    Filler,
    /// A category introduced by a newer server.
    Unknown(String),
}

impl SponsorCategory {
    /// Returns the category value used by `SponsorBlock`'s API.
    #[must_use]
    pub fn as_api_value(&self) -> &str {
        match self {
            Self::Sponsor => "sponsor",
            Self::SelfPromotion => "selfpromo",
            Self::Interaction => "interaction",
            Self::Intro => "intro",
            Self::Outro => "outro",
            Self::Preview => "preview",
            Self::MusicOfftopic => "music_offtopic",
            Self::PointOfInterest => "poi_highlight",
            Self::Filler => "filler",
            Self::Unknown(value) => value,
        }
    }

    fn from_api_value(value: String) -> Self {
        match value.as_str() {
            "sponsor" => Self::Sponsor,
            "selfpromo" => Self::SelfPromotion,
            "interaction" => Self::Interaction,
            "intro" => Self::Intro,
            "outro" => Self::Outro,
            "preview" => Self::Preview,
            "music_offtopic" => Self::MusicOfftopic,
            "poi_highlight" => Self::PointOfInterest,
            "filler" => Self::Filler,
            _ => Self::Unknown(value),
        }
    }
}

/// Action attached to a `SponsorBlock` segment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentAction {
    /// Seek to the end of the segment.
    Skip,
    /// Silence the segment without seeking.
    Mute,
    /// The submission describes the whole video.
    FullVideo,
    /// Seek to a single highlighted point.
    PointOfInterest,
    /// An action introduced by a newer server.
    Unknown(String),
}

impl SegmentAction {
    fn from_api_value(value: String) -> Self {
        match value.as_str() {
            "skip" => Self::Skip,
            "mute" => Self::Mute,
            "full" => Self::FullVideo,
            "poi" => Self::PointOfInterest,
            _ => Self::Unknown(value),
        }
    }
}

/// One validated `SponsorBlock` submission selected by the server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SponsorSegment {
    /// Inclusive segment start in seconds.
    pub start_seconds: f64,
    /// Exclusive segment end in seconds.
    pub end_seconds: f64,
    /// Crowdsourced classification.
    pub category: SponsorCategory,
    /// Playback action recommended by the server.
    pub action: SegmentAction,
    /// Stable submission identifier.
    pub uuid: String,
    /// Current vote score.
    pub votes: i64,
    /// Whether a moderator locked the submission.
    pub locked: bool,
    /// Optional chapter or point-of-interest title.
    pub description: Option<String>,
    /// Video duration at submission time, when known.
    pub submitted_video_duration: Option<f64>,
}

/// Result of applying skip logic at the current playback position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SkipDecision {
    /// Current position that triggered the decision.
    pub from_seconds: f64,
    /// End of the active segment or overlapping segment group.
    pub to_seconds: f64,
}

/// Blocking `SponsorBlock` API client.
#[derive(Clone)]
pub struct SponsorBlockClient {
    base_url: Url,
    agent: ureq::Agent,
    max_json_bytes: usize,
}

impl SponsorBlockClient {
    /// Creates a client for a configured SponsorBlock-compatible service.
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
                "SponsorBlock timeout must be greater than zero".to_owned(),
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

    /// Fetches skip actions for a `YouTube` video.
    ///
    /// An empty category slice asks for `SponsorBlock`'s default `sponsor`
    /// category. HTTP 404 means that no matching submissions exist.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] for invalid input, transport failure, an
    /// unsuccessful non-404 status, or an invalid bounded response.
    pub fn segments(
        &self,
        video_id: &str,
        categories: &[SponsorCategory],
    ) -> Result<Vec<SponsorSegment>, ProviderError> {
        let url = self.build_segments_url(video_id, categories)?;
        let raw: Vec<RawSponsorSegment> =
            match get_bounded_json(&self.agent, &url, self.max_json_bytes) {
                Ok(raw) => raw,
                Err(ProviderError::HttpStatus(404)) => return Ok(Vec::new()),
                Err(error) => return Err(error),
            };
        raw.into_iter().map(TryInto::try_into).collect()
    }

    fn build_segments_url(
        &self,
        video_id: &str,
        categories: &[SponsorCategory],
    ) -> Result<Url, ProviderError> {
        validate_youtube_video_id(video_id)?;
        let mut url = self
            .base_url
            .join("api/skipSegments")
            .map_err(|error| ProviderError::InvalidBaseUrl(error.to_string()))?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("videoID", video_id);
            query.append_pair("actionType", "skip");
            query.append_pair("service", "YouTube");
            for category in categories {
                let value = category.as_api_value();
                if value.is_empty() || value.len() > 64 || !is_api_token(value) {
                    return Err(ProviderError::InvalidRequest(
                        "SponsorBlock category contains invalid characters".to_owned(),
                    ));
                }
                query.append_pair("category", value);
            }
        }
        Ok(url)
    }
}

impl TryFrom<RawSponsorSegment> for SponsorSegment {
    type Error = ProviderError;

    fn try_from(raw: RawSponsorSegment) -> Result<Self, Self::Error> {
        let [start_seconds, end_seconds] = raw.segment;
        if !start_seconds.is_finite()
            || !end_seconds.is_finite()
            || start_seconds < 0.0
            || end_seconds <= start_seconds
        {
            return Err(ProviderError::InvalidResponse(
                "SponsorBlock segment must have finite increasing times".to_owned(),
            ));
        }
        let submitted_video_duration = raw
            .video_duration
            .filter(|duration| duration.is_finite() && *duration > 0.0);

        Ok(Self {
            start_seconds,
            end_seconds,
            category: SponsorCategory::from_api_value(raw.category),
            action: SegmentAction::from_api_value(raw.action_type),
            uuid: raw.uuid,
            votes: raw.votes,
            locked: raw.locked,
            description: raw.description.filter(|value| !value.trim().is_empty()),
            submitted_video_duration,
        })
    }
}

/// Determines whether playback at `position_seconds` should seek forward.
///
/// Adjacent or overlapping skip segments are coalesced, preventing a burst of
/// repeated seeks when submissions overlap. The function performs no I/O and
/// does not mutate the supplied segment list.
#[must_use]
pub fn skip_decision_at(
    segments: &[SponsorSegment],
    position_seconds: f64,
) -> Option<SkipDecision> {
    if !position_seconds.is_finite() || position_seconds < 0.0 {
        return None;
    }

    let mut target = segments
        .iter()
        .filter(|segment| segment.action == SegmentAction::Skip)
        .filter(|segment| {
            position_seconds >= segment.start_seconds && position_seconds < segment.end_seconds
        })
        .map(|segment| segment.end_seconds)
        .reduce(f64::max)?;

    loop {
        let extended = segments
            .iter()
            .filter(|segment| segment.action == SegmentAction::Skip)
            .filter(|segment| segment.start_seconds <= target && segment.end_seconds > target)
            .map(|segment| segment.end_seconds)
            .reduce(f64::max);
        match extended {
            Some(new_target) if new_target > target => target = new_target,
            _ => break,
        }
    }

    Some(SkipDecision {
        from_seconds: position_seconds,
        to_seconds: target,
    })
}

/// Finds the next skippable segment at or after the current position.
///
/// A currently active segment sorts before all future segments. Segment input
/// need not be pre-sorted.
#[must_use]
pub fn next_skip_segment(
    segments: &[SponsorSegment],
    position_seconds: f64,
) -> Option<&SponsorSegment> {
    if !position_seconds.is_finite() || position_seconds < 0.0 {
        return None;
    }
    segments
        .iter()
        .filter(|segment| {
            segment.action == SegmentAction::Skip && segment.end_seconds > position_seconds
        })
        .min_by(|left, right| {
            let left_key = if left.start_seconds <= position_seconds {
                position_seconds
            } else {
                left.start_seconds
            };
            let right_key = if right.start_seconds <= position_seconds {
                position_seconds
            } else {
                right.start_seconds
            };
            left_key
                .total_cmp(&right_key)
                .then_with(|| left.end_seconds.total_cmp(&right.end_seconds))
        })
}

fn is_api_token(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSponsorSegment {
    segment: [f64; 2],
    #[serde(rename = "UUID")]
    uuid: String,
    category: String,
    #[serde(default = "default_action_type")]
    action_type: String,
    #[serde(default)]
    votes: i64,
    #[serde(default, deserialize_with = "deserialize_boolish")]
    locked: bool,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    video_duration: Option<f64>,
}

fn default_action_type() -> String {
    "skip".to_owned()
}

fn deserialize_boolish<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Boolish {
        Bool(bool),
        Integer(i64),
    }

    match Boolish::deserialize(deserializer)? {
        Boolish::Bool(value) => Ok(value),
        Boolish::Integer(value) => Ok(value != 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEGMENTS_FIXTURE: &str = r#"[
		{
			"segment": [10.0, 20.5],
			"UUID": "first-segment",
			"category": "sponsor",
			"videoDuration": 120.0,
			"actionType": "skip",
			"locked": 1,
			"votes": 8,
			"description": ""
		},
		{
			"segment": [19.0, 30.0],
			"UUID": "second-segment",
			"category": "future_category",
			"actionType": "skip",
			"locked": false,
			"votes": 0,
			"description": "A chapter"
		}
	]"#;

    fn client() -> SponsorBlockClient {
        SponsorBlockClient::new(
            Url::parse("https://sponsor.example.test/prefix").expect("fixture URL should parse"),
        )
        .expect("fixture client should construct")
    }

    fn fixture_segments() -> Vec<SponsorSegment> {
        serde_json::from_str::<Vec<RawSponsorSegment>>(SEGMENTS_FIXTURE)
            .expect("fixture should deserialize")
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<_, _>>()
            .expect("fixture should validate")
    }

    #[test]
    fn url_contains_repeated_categories_without_json_escaping() {
        let url = client()
            .build_segments_url(
                "dQw4w9WgXcQ",
                &[SponsorCategory::Sponsor, SponsorCategory::Intro],
            )
            .expect("request should be valid");
        let pairs = url.query_pairs().collect::<Vec<_>>();

        assert_eq!(url.path(), "/prefix/api/skipSegments");
        assert!(pairs.contains(&("videoID".into(), "dQw4w9WgXcQ".into())));
        assert!(pairs.contains(&("category".into(), "sponsor".into())));
        assert!(pairs.contains(&("category".into(), "intro".into())));
    }

    #[test]
    fn fixture_parses_boolish_lock_and_unknown_category() {
        let segments = fixture_segments();

        assert!(segments[0].locked);
        assert_eq!(segments[0].description, None);
        assert_eq!(
            segments[1].category,
            SponsorCategory::Unknown("future_category".to_owned())
        );
        assert_eq!(segments[1].description.as_deref(), Some("A chapter"));
    }

    #[test]
    fn skip_logic_coalesces_overlapping_segments() {
        let decision =
            skip_decision_at(&fixture_segments(), 15.0).expect("position is in a segment");

        assert_eq!(
            decision,
            SkipDecision {
                from_seconds: 15.0,
                to_seconds: 30.0
            }
        );
    }

    #[test]
    fn skip_logic_uses_half_open_boundaries_and_ignores_invalid_positions() {
        let segments = fixture_segments();

        assert!(skip_decision_at(&segments, 30.0).is_none());
        assert!(skip_decision_at(&segments, f64::NAN).is_none());
        assert!(skip_decision_at(&segments, -1.0).is_none());
    }

    #[test]
    fn next_segment_works_with_unsorted_input() {
        let mut segments = fixture_segments();
        segments.reverse();

        let next = next_skip_segment(&segments, 0.0).expect("future segment should exist");
        assert_eq!(next.uuid, "first-segment");

        let active = next_skip_segment(&segments, 19.5).expect("active segment should exist");
        assert_eq!(active.uuid, "first-segment");
    }

    #[test]
    fn malformed_segment_is_rejected() {
        let raw = serde_json::from_str::<RawSponsorSegment>(
            r#"{"segment":[20,10],"UUID":"bad","category":"sponsor","actionType":"skip"}"#,
        )
        .expect("fixture should deserialize");

        assert!(matches!(
            SponsorSegment::try_from(raw),
            Err(ProviderError::InvalidResponse(_))
        ));
    }
}
