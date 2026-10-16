//! Stable VK profile and audio identities, independent from web-session data.
//!
//! Ordinary search words and numbers never trigger a speculative profile lookup.
//! Explicit `@handle`, `profile:handle`, `id123` and profile URLs select that route.

use std::fmt;

use url::Url;

use crate::providers::ProviderError;

const MAX_QUERY_BYTES: usize = 2 * 1024;
const MAX_HANDLE_BYTES: usize = 64;

/// An explicit user-profile reference whose spelling carries no session data.
///
/// Construction must pass through [`Self::parse`] so invalid IDs and arbitrary
/// paths cannot enter the authenticated profile client.
///
/// ```compile_fail
/// use youta::providers::vk::ProfileReference;
/// let invalid = ProfileReference::UserId(0);
/// ```
///
/// ```compile_fail
/// use youta::providers::vk::ProfileReference;
/// let invalid = ProfileReference::Handle("../audio1_2".to_owned());
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileReference(ProfileKind);

/// Validated profile spelling, inaccessible to callers constructing references.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ProfileKind {
    UserId(u64),
    Handle(String),
}

/// A parsed VK query with explicit catalogue intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VkQuery {
    /// Text searched in the accessible music catalogue.
    Search(String),
    /// A user's account-accessible music catalogue.
    Profile(ProfileReference),
    /// One canonical audio item, with no transient access key or media URL.
    Track(VkAudioId),
}

/// Stable signed owner and positive track ID used for queue and replay identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct VkAudioId {
    owner_id: i64,
    audio_id: u64,
}

impl ProfileReference {
    /// Parses an ID, `id123`, bare handle or `@handle` in an explicit profile context.
    ///
    /// # Errors
    /// Rejects zero/out-of-range IDs, ambiguous website routes, delimiters and
    /// malformed handles without including the supplied text in the error.
    pub fn parse(value: &str) -> Result<Self, ProviderError> {
        let value = value.strip_prefix('@').unwrap_or(value);
        if value.bytes().all(|byte| byte.is_ascii_digit()) {
            return positive_id(value).map(|id| Self(ProfileKind::UserId(id)));
        }
        if let Some(id) = value.strip_prefix("id")
            && !id.is_empty()
            && id.bytes().all(|byte| byte.is_ascii_digit())
        {
            return positive_id(id).map(|id| Self(ProfileKind::UserId(id)));
        }
        if value.is_empty()
            || value.len() > MAX_HANDLE_BYTES
            || matches!(value, "." | "..")
            || value.starts_with('.')
            || value.ends_with('.')
            || value.contains("..")
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.'))
            || reserved_route(value)
        {
            return Err(invalid_profile());
        }
        Ok(Self(ProfileKind::Handle(value.to_owned())))
    }

    /// Returns a strictly positive user ID when this reference is numeric.
    #[must_use]
    pub const fn user_id(&self) -> Option<u64> {
        match self.0 {
            ProfileKind::UserId(id) => Some(id),
            ProfileKind::Handle(_) => None,
        }
    }

    /// Returns a safe profile name awaiting an authenticated exact-name lookup.
    #[must_use]
    pub fn handle(&self) -> Option<&str> {
        match &self.0 {
            ProfileKind::Handle(handle) => Some(handle),
            ProfileKind::UserId(_) => None,
        }
    }

    /// Returns a credential-free profile URL without resolving the handle.
    #[must_use]
    pub fn webpage_url(&self) -> Url {
        let path = match &self.0 {
            ProfileKind::UserId(id) => format!("id{id}"),
            ProfileKind::Handle(handle) => handle.clone(),
        };
        let mut url = webpage_origin();
        url.set_path(&path);
        url
    }
}

impl VkAudioId {
    /// Creates an exact audio identity; negative owners represent community uploads.
    ///
    /// # Errors
    /// Rejects zero identities and track IDs outside the signed API integer range.
    pub fn new(owner_id: i64, audio_id: u64) -> Result<Self, ProviderError> {
        if owner_id == 0 || owner_id == i64::MIN || audio_id == 0 || audio_id > i64::MAX as u64 {
            return Err(invalid_audio());
        }
        Ok(Self { owner_id, audio_id })
    }

    /// Parses a canonical `owner_track` replay identity without optional secret suffixes.
    ///
    /// # Errors
    /// Rejects malformed, noncanonical, zero or out-of-range IDs.
    pub fn parse(value: &str) -> Result<Self, ProviderError> {
        let (owner, track) = value.split_once('_').ok_or_else(invalid_audio)?;
        let owner_id = owner.parse::<i64>().map_err(|_| invalid_audio())?;
        let audio_id = track.parse::<u64>().map_err(|_| invalid_audio())?;
        let id = Self::new(owner_id, audio_id)?;
        if id.to_string() != value {
            return Err(invalid_audio());
        }
        Ok(id)
    }

    /// Parses one exact HTTP(S) VK audio page with no credentials or query fields.
    ///
    /// # Errors
    /// Rejects other origins, non-audio routes, access keys and malformed identities.
    pub fn from_page_url(url: &Url) -> Result<Self, ProviderError> {
        validate_page_url(url).map_err(|_| invalid_audio())?;
        let path = url.path().trim_end_matches('/');
        let id = path.strip_prefix("/audio").ok_or_else(invalid_audio)?;
        Self::parse(id)
    }

    /// Returns the stable signed owner identifier used by VK's audio API.
    #[must_use]
    pub const fn owner_id(self) -> i64 {
        self.owner_id
    }

    /// Returns the positive track identifier within the owner's namespace.
    #[must_use]
    pub const fn audio_id(self) -> u64 {
        self.audio_id
    }

    /// Returns a canonical original page suitable for history, copying and links.
    #[must_use]
    pub fn webpage_url(self) -> Url {
        let mut url = webpage_origin();
        url.set_path(&format!("audio{self}"));
        url
    }
}

impl fmt::Display for VkAudioId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}_{}", self.owner_id, self.audio_id)
    }
}

/// Provides the fixed original-page origin without accepting external URL text.
fn webpage_origin() -> Url {
    Url::parse("https://vk.ru/").expect("fixed VK origin")
}

/// Distinguishes an explicit profile/track route from ordinary music-search text.
///
/// Full and bare-host profile URLs are accepted for both `vk.com` and `vk.ru`.
/// Bare words and numbers remain song searches; `@name`, `profile:name`, `id123`
/// or `profile:123` selects a user.
///
/// # Errors
/// Rejects oversized/empty/control-bearing queries and malformed explicit routes.
pub fn parse_query(input: &str) -> Result<VkQuery, ProviderError> {
    if input.len() > MAX_QUERY_BYTES || input.chars().any(char::is_control) {
        return Err(invalid_query());
    }
    let input = input.trim();
    if input.is_empty() {
        return Err(invalid_query());
    }
    if let Some(profile) = input.strip_prefix("profile:") {
        return ProfileReference::parse(profile.trim()).map(VkQuery::Profile);
    }
    if input.starts_with('@')
        || input
            .strip_prefix("id")
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return ProfileReference::parse(input).map(VkQuery::Profile);
    }
    let bare_host = [
        "vk.com/",
        "vk.ru/",
        "www.vk.com/",
        "www.vk.ru/",
        "m.vk.com/",
        "m.vk.ru/",
    ]
    .iter()
    .any(|prefix| input.starts_with(prefix));
    if input.contains("://") || bare_host {
        let address = if bare_host {
            format!("https://{input}")
        } else {
            input.to_owned()
        };
        let url = Url::parse(&address).map_err(|_| invalid_profile())?;
        validate_page_url(&url)?;
        let path = url.path().trim_end_matches('/');
        let value = path.strip_prefix('/').ok_or_else(invalid_profile)?;
        if let Some(id) = value.strip_prefix("audio")
            && id.split_once('_').is_some_and(|(owner, _)| {
                let owner = owner.strip_prefix('-').unwrap_or(owner);
                !owner.is_empty() && owner.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return VkAudioId::parse(id).map(VkQuery::Track);
        }
        if let Some(id) = value.strip_prefix("audios")
            && !id.is_empty()
            && id.bytes().all(|byte| byte.is_ascii_digit())
        {
            return ProfileReference::parse(id).map(VkQuery::Profile);
        }
        return ProfileReference::parse(value).map(VkQuery::Profile);
    }
    Ok(VkQuery::Search(input.to_owned()))
}

/// Allows only ordinary VK page origins and drops no hidden URL credentials.
fn validate_page_url(url: &Url) -> Result<(), ProviderError> {
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some("vk.ru" | "vk.com" | "www.vk.ru" | "www.vk.com" | "m.vk.ru" | "m.vk.com")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port().is_some()
    {
        return Err(invalid_profile());
    }
    Ok(())
}

/// VK's positive numeric identities use the signed API integer range.
fn positive_id(value: &str) -> Result<u64, ProviderError> {
    let id = value.parse::<u64>().map_err(|_| invalid_profile())?;
    if id == 0 || id > i64::MAX as u64 || id.to_string() != value {
        return Err(invalid_profile());
    }
    Ok(id)
}

/// Website controls cannot accidentally become a username or catalogue owner.
fn reserved_route(value: &str) -> bool {
    [
        "audio", "audios", "feed", "friends", "groups", "im", "join", "login", "music", "search",
        "settings", "video",
    ]
    .iter()
    .any(|route| value.eq_ignore_ascii_case(route))
}

fn invalid_profile() -> ProviderError {
    ProviderError::InvalidRequest("invalid VK profile URL, username or numeric ID".to_owned())
}

fn invalid_audio() -> ProviderError {
    ProviderError::InvalidRequest("invalid VK audio identity".to_owned())
}

fn invalid_query() -> ProviderError {
    ProviderError::InvalidRequest(
        "VK search must be nonempty text within the 2 KiB limit".to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_profile_names_urls_and_ids_share_one_typed_route() {
        for input in [
            "id123",
            "@123",
            "@id123",
            "profile:123",
            "https://vk.com/id123",
            "http://m.vk.ru/id123/",
            "vk.ru/audios123",
        ] {
            assert_eq!(
                parse_query(input).unwrap(),
                VkQuery::Profile(ProfileReference(ProfileKind::UserId(123))),
                "{input}"
            );
        }
        for input in [
            "@fixture.name",
            "profile:fixture.name",
            "https://vk.ru/fixture.name",
            "www.vk.com/fixture.name/",
        ] {
            assert_eq!(
                parse_query(input).unwrap(),
                VkQuery::Profile(ProfileReference(ProfileKind::Handle(
                    "fixture.name".to_owned()
                ))),
                "{input}"
            );
        }
    }

    #[test]
    fn audio_prefixed_handles_are_not_mistaken_for_numeric_routes() {
        for handle in [
            "audiosmith",
            "audiophile_name",
            "audios_crew",
            "audio9fan_artist",
        ] {
            let reference = ProfileReference::parse(handle).unwrap();
            assert_eq!(
                parse_query(&format!("https://vk.ru/{handle}")).unwrap(),
                VkQuery::Profile(reference),
                "{handle}"
            );
        }
    }

    #[test]
    fn profile_accessors_expose_only_the_validated_reference_kind() {
        let numeric = ProfileReference::parse("id123").unwrap();
        assert_eq!(numeric.user_id(), Some(123));
        assert_eq!(numeric.handle(), None);
        let named = ProfileReference::parse("@fixture.name").unwrap();
        assert_eq!(named.user_id(), None);
        assert_eq!(named.handle(), Some("fixture.name"));
    }

    #[test]
    fn music_searches_do_not_probe_possible_usernames() {
        for query in [
            "Minsk",
            "Ляпис Трубецкой",
            "Moby",
            "idMusic",
            "An artist @ another",
        ] {
            assert_eq!(
                parse_query(query).unwrap(),
                VkQuery::Search(query.to_owned())
            );
        }
        assert_eq!(
            parse_query("  calm music  ").unwrap(),
            VkQuery::Search("calm music".to_owned())
        );
    }

    /// Numbers can name music even when they are not valid canonical profile IDs.
    #[test]
    fn numeric_music_queries_do_not_select_a_profile() {
        for query in ["2032", "0", "00123", "9223372036854775808"] {
            assert_eq!(
                parse_query(query).unwrap(),
                VkQuery::Search(query.to_owned()),
                "{query}"
            );
        }
        assert_eq!(
            parse_query("  2032  ").unwrap(),
            VkQuery::Search("2032".to_owned())
        );
    }

    #[test]
    fn rejects_unsafe_malformed_or_ambiguous_explicit_profile_inputs() {
        for query in [
            "",
            "profile:0",
            "id0",
            "@0",
            "id00123",
            "profile:00123",
            "@00123",
            "@",
            "profile:",
            "@foo/bar",
            "@foo?secret=1",
            "@..",
            "@foo..bar",
            "@foo\nbar",
            "@-123",
            "profile:music",
            "https://vk.ru/feed",
            "https://vk.ru/id123?secret=1",
            "https://vk.ru/id123#secret",
            "https://user:secret@vk.ru/id123",
            "https://vk.ru.example.test/id123",
            "https://evilvk.ru/id123",
            "https://vk.ru:444/id123",
            "https://vk.ru/audios0",
            "https://vk.ru/a/b",
            "https://vk.ru/%40foo",
            "https://vk.ru/",
            "id9223372036854775808",
            "profile:9223372036854775808",
        ] {
            let error = parse_query(query).unwrap_err();
            assert!(!error.to_string().contains("secret"));
        }
        assert!(parse_query(&"x".repeat(MAX_QUERY_BYTES + 1)).is_err());
    }

    #[test]
    fn canonical_audio_identity_preserves_signed_owners_without_access_keys() {
        for text in ["123_456", "-123_456"] {
            let id = VkAudioId::parse(text).unwrap();
            assert_eq!(id.to_string(), text);
            assert_eq!(id.audio_id(), 456);
            assert_eq!(id.owner_id().unsigned_abs(), 123);
            assert_eq!(
                id.webpage_url().as_str(),
                format!("https://vk.ru/audio{text}")
            );
            assert_eq!(VkAudioId::from_page_url(&id.webpage_url()).unwrap(), id);
            assert_eq!(
                parse_query(&format!("https://vk.com/audio{text}")).unwrap(),
                VkQuery::Track(id)
            );
        }
        for text in [
            "0_1",
            "1_0",
            "01_2",
            "+1_2",
            "1_02",
            "1_-2",
            "1_2_secret",
            "1",
            "1_2/extra",
            "-9223372036854775808_1",
        ] {
            assert!(VkAudioId::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn stable_pages_never_retain_a_supplied_media_or_access_locator() {
        for address in [
            "https://vk.ru/audio1_2?access_key=secret",
            "https://vk.ru/audio1_2#secret",
            "https://vkuseraudio.net/audio1_2",
            "https://vk.ru/video1_2",
            "https://vk.ru/audio1_2/extra",
        ] {
            assert!(VkAudioId::from_page_url(&Url::parse(address).unwrap()).is_err());
        }
        assert_eq!(
            ProfileReference::parse("fixture.name")
                .unwrap()
                .webpage_url()
                .as_str(),
            "https://vk.ru/fixture.name"
        );
        assert_eq!(
            ProfileReference::parse("id123")
                .unwrap()
                .webpage_url()
                .as_str(),
            "https://vk.ru/id123"
        );
    }
}
