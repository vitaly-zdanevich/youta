//! BBC Sounds discovery helpers that use documented public entry points.
//!
//! BBC does not document a general-purpose Sounds catalogue API for third
//! parties. Youta therefore exposes stable station page presets and the BBC's
//! published podcast OPML/RSS entry points, then delegates media extraction to
//! the explicitly enabled `yt-dlp` worker. Availability remains subject to BBC
//! geographic and programme-rights restrictions.

use url::Url;

use super::ProviderError;

/// BBC's published OPML index of radio and podcast feeds.
pub const PODCAST_OPML_URL: &str = "https://www.bbc.co.uk/radio/opml/bbc_podcast_opml.opml";

/// BBC's public podcast directory.
pub const PODCAST_DIRECTORY_URL: &str = "https://www.bbc.co.uk/sounds/podcasts";

/// One built-in BBC Sounds live-station entry point.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BbcStationPreset {
    /// Stable BBC Sounds service identifier.
    pub id: &'static str,
    /// Name displayed in the station picker.
    pub name: &'static str,
}

impl BbcStationPreset {
    /// Returns the public BBC Sounds page resolved later by `yt-dlp`.
    ///
    /// # Errors
    ///
    /// Returns an error only if the compile-time BBC URL template is invalid.
    pub fn sounds_url(self) -> Result<Url, ProviderError> {
        Url::parse(&format!(
            "https://www.bbc.co.uk/sounds/play/live:{}",
            self.id
        ))
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))
    }
}

/// Conservative national and international BBC station presets.
///
/// Local and national-region stations can still be opened through the generic
/// URL source without requiring Youta releases when the BBC changes its list.
pub const STATIONS: &[BbcStationPreset] = &[
    BbcStationPreset {
        id: "bbc_radio_one",
        name: "BBC Radio 1",
    },
    BbcStationPreset {
        id: "bbc_radio_1xtra",
        name: "BBC Radio 1Xtra",
    },
    BbcStationPreset {
        id: "bbc_radio_two",
        name: "BBC Radio 2",
    },
    BbcStationPreset {
        id: "bbc_radio_three",
        name: "BBC Radio 3",
    },
    BbcStationPreset {
        id: "bbc_radio_fourfm",
        name: "BBC Radio 4",
    },
    BbcStationPreset {
        id: "bbc_radio_four_extra",
        name: "BBC Radio 4 Extra",
    },
    BbcStationPreset {
        id: "bbc_radio_five_live",
        name: "BBC Radio 5 Live",
    },
    BbcStationPreset {
        id: "bbc_radio_five_live_sports_extra",
        name: "BBC Radio 5 Sports Extra",
    },
    BbcStationPreset {
        id: "bbc_6music",
        name: "BBC Radio 6 Music",
    },
    BbcStationPreset {
        id: "bbc_asian_network",
        name: "BBC Asian Network",
    },
    BbcStationPreset {
        id: "bbc_world_service",
        name: "BBC World Service",
    },
];

/// Finds a built-in station by its stable BBC Sounds identifier.
#[must_use]
pub fn station_by_id(id: &str) -> Option<BbcStationPreset> {
    STATIONS.iter().copied().find(|station| station.id == id)
}

/// Builds a BBC podcast RSS URL from an eight-character programme ID.
///
/// BBC podcast pages advertise feeds from `podcasts.files.bbci.co.uk`. The
/// programme ID should be read from that page or imported from the BBC OPML
/// index; Youta does not scrape the undocumented Sounds catalogue.
///
/// # Errors
///
/// Returns [`ProviderError::InvalidRequest`] for a malformed programme ID.
pub fn podcast_feed_url(programme_id: &str) -> Result<Url, ProviderError> {
    if programme_id.len() != 8
        || !programme_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err(ProviderError::InvalidRequest(
            "BBC programme ID must contain eight lowercase ASCII letters or digits".to_owned(),
        ));
    }

    let mut url = Url::parse("https://podcasts.files.bbci.co.uk/")
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    url.path_segments_mut()
        .map_err(|()| ProviderError::InvalidResponse("invalid BBC podcast base URL".to_owned()))?
        .pop_if_empty()
        .push(&format!("{programme_id}.rss"));
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_resolves_to_a_public_sounds_page() {
        let station = station_by_id("bbc_6music").expect("fixture station should exist");

        assert_eq!(station.name, "BBC Radio 6 Music");
        assert_eq!(
            station
                .sounds_url()
                .expect("preset URL should parse")
                .as_str(),
            "https://www.bbc.co.uk/sounds/play/live:bbc_6music"
        );
    }

    #[test]
    fn programme_id_builds_documented_rss_url() {
        assert_eq!(
            podcast_feed_url("p02nq0gn")
                .expect("fixture programme ID should be valid")
                .as_str(),
            "https://podcasts.files.bbci.co.uk/p02nq0gn.rss"
        );
    }

    #[test]
    fn programme_id_rejects_paths_and_mixed_case() {
        for invalid in ["../feed!", "P02NQ0GN", "short"] {
            assert!(matches!(
                podcast_feed_url(invalid),
                Err(ProviderError::InvalidRequest(_))
            ));
        }
    }
}
