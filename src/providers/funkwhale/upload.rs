//! Optional public upload ownership, separate from shared track metadata.

use serde_json::Value;
use url::Url;

use super::uploader::FunkwhaleUploader;
use super::{ProviderError, get_bounded_json, parse_rfc3339_epoch};

/// Public facts about one advertised audio upload, not its shared track record.
#[derive(Clone, Debug, Default)]
pub struct FunkwhaleUploadMetadata {
    /// Uploader account from the advertised library actor, when published.
    pub uploader_name: Option<String>,
    /// Credential-free advertised profile or federation actor URL.
    pub uploader_url: Option<Url>,
    /// Exact advertised account and federation identity for in-app upload browsing.
    pub uploader_actor: Option<FunkwhaleUploader>,
    /// Unix seconds from this upload's creation date, never a catalogue/import date.
    pub created_at: Option<i64>,
}

/// Fetches optional ownership with the existing client's timeout and JSON bound.
///
/// The caller supplies the selected upload's pod-scoped endpoint and may omit
/// these supplementary facts when the pod restricts access to upload records.
pub(super) fn fetch_upload_metadata(
    agent: &ureq::Agent,
    url: &Url,
    max_json_bytes: usize,
) -> Result<FunkwhaleUploadMetadata, ProviderError> {
    let raw: Value = get_bounded_json(agent, url, max_json_bytes)?;
    Ok(parse_upload_metadata(&raw))
}

/// Accepts differing library representations without inventing owner information.
fn parse_upload_metadata(raw: &Value) -> FunkwhaleUploadMetadata {
    let actor = raw.get("library").and_then(|library| library.get("actor"));
    FunkwhaleUploadMetadata {
        uploader_name: actor.and_then(|actor| {
            ["full_username", "name", "preferred_username"]
                .into_iter()
                .find_map(|field| actor.get(field).and_then(public_name))
        }),
        uploader_url: actor.and_then(|actor| {
            ["url", "fid"]
                .into_iter()
                .find_map(|field| actor.get(field).and_then(public_url))
        }),
        uploader_actor: actor.and_then(FunkwhaleUploader::from_actor),
        created_at: raw
            .get("creation_date")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_epoch),
    }
}

/// Bounds display text and rejects terminal control sequences from remote names.
fn public_name(raw: &Value) -> Option<String> {
    let value = raw.as_str()?.trim();
    (!value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control))
        .then(|| value.to_owned())
}

/// Accepts only advertised absolute web links without embedded credentials.
fn public_url(raw: &Value) -> Option<Url> {
    let value = raw.as_str()?;
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return None;
    }
    let url = Url::parse(value).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The upload's date and owner never inherit the track's catalogue metadata.
    #[test]
    fn advertised_upload_owner_and_date_are_parsed_from_their_exact_fields() {
        let metadata = parse_upload_metadata(&json!({
            "creation_date": "2026-05-25T16:32:46.126602Z",
            "track": {
                "creation_date": "2026-05-25T16:35:23.629468Z",
                "attributed_to": {"name": "Metadata editor"}
            },
            "library": {
                "name": "Unrelated library title",
                "creation_date": "2026-05-25T16:25:19.430535Z",
                "actor": {
                    "full_username": "wadidubroffski@audio.pepemoss.com",
                    "name": "wadidubroffski",
                    "preferred_username": "wadidubroffski",
                    "url": null,
                    "fid": "https://audio.pepemoss.com/federation/actors/wadidubroffski"
                }
            }
        }));
        assert_eq!(
            metadata.uploader_name.as_deref(),
            Some("wadidubroffski@audio.pepemoss.com")
        );
        assert_eq!(
            metadata.uploader_url.unwrap().as_str(),
            "https://audio.pepemoss.com/federation/actors/wadidubroffski"
        );
        assert_eq!(
            metadata.created_at,
            parse_rfc3339_epoch("2026-05-25T16:32:46Z")
        );
    }

    /// A public profile is preferred to its federation object when both are safe.
    #[test]
    fn advertised_profile_url_precedes_federation_identity() {
        let metadata = parse_upload_metadata(&json!({
            "library": {"actor": {
                "name": "Uploader",
                "url": "https://pod.example/@uploader",
                "fid": "https://pod.example/federation/actors/uploader"
            }}
        }));
        assert_eq!(metadata.uploader_name.as_deref(), Some("Uploader"));
        assert_eq!(
            metadata.uploader_url.unwrap().as_str(),
            "https://pod.example/@uploader"
        );
    }

    /// Opaque libraries cannot identify an uploader, even when artist names exist.
    #[test]
    fn missing_or_opaque_libraries_do_not_guess_an_owner() {
        for library in [
            json!(null),
            json!("library-uuid"),
            json!({"name": "Artist name"}),
            json!([]),
        ] {
            let metadata = parse_upload_metadata(&json!({
                "library": library,
                "owner": {"name": "Unverified owner"},
                "attributed_to": {"name": "Metadata editor"},
                "track": {"artist": {"name": "Recording artist"}}
            }));
            assert!(metadata.uploader_name.is_none());
            assert!(metadata.uploader_url.is_none());
        }
        let metadata = parse_upload_metadata(&json!({}));
        assert!(metadata.uploader_name.is_none());
        assert!(metadata.created_at.is_none());
    }

    /// Invalid upload dates do not fall back to import, actor, or track dates.
    #[test]
    fn malformed_dates_are_omitted_without_substituting_other_dates() {
        for date in [
            json!(null),
            json!("not a date"),
            json!("2026-13-99T25:00:00Z"),
            json!(42),
        ] {
            let metadata = parse_upload_metadata(&json!({
                "creation_date": date,
                "import_date": "2026-05-25T16:32:46Z",
                "track": {"creation_date": "2026-05-25T16:32:46Z"},
                "library": {"actor": {"creation_date": "2026-05-25T16:32:46Z"}}
            }));
            assert!(metadata.created_at.is_none());
        }
    }

    /// Embedded credentials and executable schemes never become clickable links.
    #[test]
    fn unsafe_profile_urls_fall_back_only_to_a_safe_advertised_fid() {
        for profile in [
            "javascript:alert(1)",
            "file:///tmp/owner",
            "/@owner",
            "https://user:password@pod.example/@owner",
            "https://user@pod.example/@owner",
        ] {
            let metadata = parse_upload_metadata(&json!({"library": {"actor": {
                "preferred_username": "owner",
                "url": profile,
                "fid": "https://pod.example/federation/actors/owner"
            }}}));
            assert_eq!(metadata.uploader_name.as_deref(), Some("owner"));
            assert_eq!(
                metadata.uploader_url.unwrap().as_str(),
                "https://pod.example/federation/actors/owner"
            );
        }
        let metadata = parse_upload_metadata(&json!({"library": {"actor": {
            "name": "Uploader", "url": "javascript:alert(1)", "fid": "file:///tmp/owner"
        }}}));
        assert!(metadata.uploader_url.is_none());
    }

    /// Missing preferred fields may fall back, but invalid UI control text is ignored.
    #[test]
    fn empty_or_invalid_names_fall_back_to_the_next_advertised_owner_field() {
        let metadata = parse_upload_metadata(&json!({"library": {"actor": {
            "full_username": "  ", "name": "bad\nname", "preferred_username": " owner "
        }}}));
        assert_eq!(metadata.uploader_name.as_deref(), Some("owner"));
    }

    /// Account browsing uses only the advertised exact actor, not display fallbacks.
    #[test]
    fn uploader_navigation_identity_is_separate_from_display_name_and_profile() {
        let metadata = parse_upload_metadata(&json!({"library": {"actor": {
            "full_username": "emil@pod.example",
            "preferred_username": "emil",
            "domain": "pod.example",
            "name": "A different display name",
            "url": "https://pod.example/@emil",
            "fid": "https://pod.example/federation/actors/emil"
        }}}));
        let actor = metadata.uploader_actor.unwrap();
        assert_eq!(actor.full_username(), "emil@pod.example");
        assert_eq!(
            actor.actor_url().as_str(),
            "https://pod.example/federation/actors/emil"
        );
        assert_eq!(
            metadata.uploader_url.unwrap().as_str(),
            "https://pod.example/@emil"
        );
        for actor in [
            json!({"name": "emil@pod.example", "fid": "https://pod.example/federation/actors/emil"}),
            json!({"preferred_username": "emil", "domain": "pod.example", "fid": "https://pod.example/federation/actors/emil"}),
            json!({"full_username": "emil@pod.example", "url": "https://pod.example/@emil"}),
            json!({"full_username": "emil@pod.example", "fid": "https://other.example/federation/actors/emil"}),
            json!({"full_username": "emil@pod.example", "preferred_username": "other", "fid": "https://pod.example/federation/actors/emil"}),
            json!({"full_username": "emil@pod.example", "domain": "other.example", "fid": "https://pod.example/federation/actors/emil"}),
            json!({"full_username": "emil@pod.example,all", "fid": "https://pod.example/federation/actors/emil"}),
        ] {
            let metadata = parse_upload_metadata(&json!({"library": {"actor": actor}}));
            assert!(metadata.uploader_actor.is_none());
        }
    }
}
