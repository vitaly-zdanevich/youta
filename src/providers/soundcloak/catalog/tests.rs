//! Sanitized catalogue fixtures exercise production parsing without HTTP or media fetches.

use super::*;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::Mutex;

/// Records only bounded instance metadata requests.
#[derive(Default)]
struct Transport {
    requests: Mutex<Vec<Url>>,
    responses: Mutex<VecDeque<Value>>,
}

impl SoundcloakTransport for Transport {
    fn fetch(&self, url: &Url, limit: usize) -> Result<Vec<u8>, ProviderError> {
        assert_eq!(limit, DEFAULT_MAX_JSON_BYTES);
        self.requests.lock().unwrap().push(url.clone());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .map(|value| serde_json::to_vec(&value).unwrap())
            .ok_or(ProviderError::HttpStatus(503))
    }
}

/// Installs a finite, deterministic metadata fixture queue behind a prefixed instance URL.
fn client(values: Vec<Value>) -> (SoundcloakClient, Arc<Transport>) {
    let transport = Arc::new(Transport {
        responses: Mutex::new(values.into()),
        ..Transport::default()
    });
    let client = SoundcloakClient::with_transport(
        Url::parse("https://soundcloak.example/prefix/").unwrap(),
        transport.clone(),
    )
    .unwrap();
    (client, transport)
}

fn artist() -> SoundcloakArtist {
    SoundcloakArtist {
        id: "95982".into(),
        name: "Ninja Tune".into(),
        webpage_url: Url::parse("https://soundcloud.com/ninja-tune").unwrap(),
        track_count: Some(1_391),
    }
}

fn user() -> Value {
    json!({"kind":"user", "id":95982, "username":"Ninja Tune", "permalink":"ninja-tune",
        "permalink_url":"https://soundcloud.com/ninja-tune", "track_count":1391})
}

fn track(id: u64) -> Value {
    json!({"kind":"track", "id":id, "title":format!("Track {id}"),
        "permalink_url":format!("https://soundcloud.com/tycho/track-{id}"),
        "user":{"username":"Tycho", "permalink":"tycho", "permalink_url":"https://soundcloud.com/tycho"},
        "policy":"ALLOW", "streamable":true, "duration":120_000,
        "media":{"transcodings":[{"snipped":false,"format":{"protocol":"hls","mime_type":"audio/mpeg"}}]}})
}

fn album() -> Value {
    json!({"kind":"playlist", "id":779192838, "title":"Tycho — Weather", "is_album":true,
        "set_type":"album", "permalink_url":"https://soundcloud.com/ninja-tune/sets/weather",
        "user":user(), "track_count":3, "tracks":[track(1), {"id":2}, {"id":3}]})
}

fn album_url() -> Url {
    Url::parse("https://soundcloud.com/ninja-tune/sets/weather").unwrap()
}

fn next(kind: &str, offset: &str, limit: usize) -> String {
    let mut url = Url::parse(&format!("https://api-v2.soundcloud.com/users/95982/{kind}")).unwrap();
    url.query_pairs_mut()
        .append_pair("offset", offset)
        .append_pair("limit", &limit.to_string());
    url.to_string()
}

#[test]
fn artist_profile_identity_uses_advertised_permalink_never_display_name() {
    assert_eq!(user_profile_url(&user()), Some(artist().webpage_url));
    assert_eq!(
        user_profile_url(&json!({"username":"Wrong display", "permalink":"tycho"})),
        Some(Url::parse("https://soundcloud.com/tycho").unwrap())
    );
    for invalid in [
        json!({"username":"ninja-tune"}),
        json!({"permalink":"../../private"}),
        json!({"permalink_url":"https://foreign.example/tycho", "permalink":"tycho"}),
        json!({"permalink_url":"https://soundcloud.com/tycho", "permalink":"other"}),
        json!({"permalink_url":"https://secret@soundcloud.com/tycho"}),
    ] {
        assert!(user_profile_url(&invalid).is_none());
    }
    let (client, _) = client(vec![]);
    let parsed = client.normalize_track(&track(1)).unwrap();
    assert_eq!(
        parsed.artist_url.as_ref().unwrap().as_str(),
        "https://soundcloud.com/tycho"
    );
    let comment =
        normalize_comment(&json!({"kind":"comment", "user":user(), "body":"Hello"})).unwrap();
    assert_eq!(comment.author_url, Some(artist().webpage_url));
}

#[test]
fn artist_resolve_and_profile_links_preserve_instance_prefix_and_public_identity() {
    let (client, transport) = client(vec![user()]);
    assert_eq!(
        client.resolve_artist(&artist().webpage_url).unwrap(),
        artist()
    );
    assert_eq!(
        client
            .profile_page_url(&artist().webpage_url)
            .unwrap()
            .as_str(),
        "https://soundcloak.example/prefix/ninja-tune"
    );
    assert_eq!(
        transport.requests.lock().unwrap()[0].path(),
        "/prefix/_/api/v2/resolve"
    );
    for raw in [
        "http://soundcloud.com/ninja-tune",
        "https://soundcloud.com/search",
        "https://soundcloud.com/ninja-tune/track",
        "https://soundcloud.com/ninja-tune?secret_token=x",
        "https://soundcloud.com/ninja%2Ftune",
        "https://foreign.example/ninja-tune",
    ] {
        let url = Url::parse(raw).unwrap();
        assert!(client.resolve_artist(&url).is_err());
        assert!(client.profile_page_url(&url).is_err());
    }
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

#[test]
fn artist_tracks_preserve_opaque_cursors_without_following_upstream_urls() {
    let token = "2026-09-11T00:00:00.000Z,tracks,00000000002396371998";
    let (client, transport) = client(vec![
        json!({"collection":[track(1)], "next_href":next("tracks",token,2)}),
        json!({"collection":[track(2)], "next_href":null}),
    ]);
    let first = client.artist_tracks(&artist(), 2, None).unwrap();
    assert_eq!(first.items.len(), 1);
    let cursor = first.next_cursor.unwrap();
    let second = client.artist_tracks(&artist(), 2, Some(&cursor)).unwrap();
    assert_eq!(second.items[0].id, "2");
    assert!(second.next_cursor.is_none());
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|url| url.host_str() == Some("soundcloak.example"))
    );
    assert_eq!(requests[1].path(), "/prefix/_/api/v2/users/95982/tracks");
    assert!(
        requests[1]
            .query_pairs()
            .any(|(key, value)| key == "offset" && value == token)
    );
}

#[test]
fn artist_empty_album_pages_keep_continuation_and_classify_eps_explicitly() {
    let mut ep = album();
    ep["set_type"] = json!("ep");
    let mut ordinary = album();
    ordinary["is_album"] = json!(false);
    let (client, _) = client(vec![
        json!({"collection":[],"next_href":next("albums","2",2)}),
        json!({"collection":[ep,ordinary],"next_href":null}),
    ]);
    let first = client.artist_albums(&artist(), 2, None).unwrap();
    assert!(first.items.is_empty());
    let second = client
        .artist_albums(&artist(), 2, first.next_cursor.as_ref())
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.items[0].release_type.as_deref(), Some("ep"));
}

#[test]
fn artist_cursors_reject_changed_owner_kind_limit_wrong_origins_and_cycles() {
    let (client, transport) = client(vec![
        json!({"collection":[],"next_href":next("tracks","opaque",2)}),
        json!({"collection":[],"next_href":next("tracks","opaque",2)}),
    ]);
    let first = client.artist_tracks(&artist(), 2, None).unwrap();
    let cursor = first.next_cursor.as_ref();
    let mut other = artist();
    other.id = "123".into();
    assert!(client.artist_tracks(&other, 2, cursor).is_err());
    assert!(client.artist_tracks(&artist(), 3, cursor).is_err());
    assert!(client.artist_albums(&artist(), 2, cursor).is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    assert!(
        client.artist_tracks(&artist(), 2, cursor).is_err(),
        "repeated cursor must stop"
    );
    for bad in [
        "https://foreign.example/users/95982/tracks?offset=x&limit=2",
        "https://api-v2.soundcloud.com/users/other/tracks?offset=x&limit=2",
        "https://api-v2.soundcloud.com/users/95982/albums?offset=x&limit=2",
        "https://api-v2.soundcloud.com/users/95982/tracks?offset=x&limit=3",
        "https://api-v2.soundcloud.com/users/95982/tracks?offset=x&offset=y&limit=2",
    ] {
        let (client, _) = self::client(vec![json!({"collection":[],"next_href":bad})]);
        assert!(client.artist_tracks(&artist(), 2, None).is_err(), "{bad}");
    }
}

#[test]
fn album_resolution_is_lazy_and_hydration_preserves_order_and_missing_slots() {
    let (client, transport) = client(vec![album(), json!([track(3)])]);
    let details = client.resolve_album(&album_url()).unwrap();
    assert_eq!(
        transport.requests.lock().unwrap().len(),
        1,
        "resolve must not hydrate"
    );
    assert!(details.tracks[0].track.is_some());
    assert!(details.tracks[1].track.is_none());
    let page = client.album_tracks(&details, 0, 3).unwrap();
    assert_eq!(
        page.items
            .iter()
            .map(|slot| slot.id.as_str())
            .collect::<Vec<_>>(),
        ["1", "2", "3"]
    );
    assert!(
        page.items[1].track.is_none(),
        "deleted/private track keeps its slot"
    );
    assert_eq!(page.items[2].track.as_ref().unwrap().artist, "Tycho");
    assert!(page.next_offset.is_none());
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests[1].path(), "/prefix/_/api/v2/tracks");
    assert_eq!(requests[1].query(), Some("ids=2%2C3"));
}

#[test]
fn album_hydration_is_bounded_to_two_fifty_id_batches_per_viewport() {
    let mut release = album();
    release["tracks"] = json!((1..=101).map(|id| json!({"id":id})).collect::<Vec<_>>());
    let (client, transport) = client(vec![
        release,
        json!((1..=50).rev().map(track).collect::<Vec<_>>()),
        json!((51..=100).rev().map(track).collect::<Vec<_>>()),
    ]);
    let details = client.resolve_album(&album_url()).unwrap();
    let page = client.album_tracks(&details, 0, 100).unwrap();
    assert_eq!(page.items.len(), 100);
    assert_eq!(page.next_offset, Some(100));
    assert!(
        page.items
            .iter()
            .enumerate()
            .all(|(i, slot)| slot.track.as_ref().unwrap().id == (i + 1).to_string())
    );
    assert_eq!(transport.requests.lock().unwrap().len(), 3);
    assert!(client.album_tracks(&details, 0, 101).is_err());
    assert!(client.album_tracks(&details, usize::MAX, 1).is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 3);
}

#[test]
fn album_and_artist_resolution_reject_mismatched_or_private_metadata() {
    let mut wrong_artist = user();
    wrong_artist["permalink_url"] = json!("https://soundcloud.com/other");
    let mut wrong_album = album();
    wrong_album["permalink_url"] = json!("https://soundcloud.com/other/sets/weather");
    let mut private_album = album();
    private_album["sharing"] = json!("private");
    let (client, _) = client(vec![wrong_artist, wrong_album, private_album]);
    assert!(client.resolve_artist(&artist().webpage_url).is_err());
    assert!(client.resolve_album(&album_url()).is_err());
    assert!(client.resolve_album(&album_url()).is_err());
}

/// Uses the public tag-search contract without adding fields to ordinary search requests.
fn tag_request() -> SoundcloakSearchRequest {
    SoundcloakSearchRequest {
        query: "*".into(),
        page: 1,
        limit: 2,
        query_urn: None,
    }
}

#[test]
fn tag_search_encodes_exact_filter_and_keeps_numeric_continuation() {
    let tag = "Ambient & Field";
    let mut continuation = Url::parse("https://api-v2.soundcloud.com/search/tracks").unwrap();
    continuation
        .query_pairs_mut()
        .append_pair("q", "*")
        .append_pair("filter.genre_or_tag", tag)
        .append_pair("sort", "popular")
        .append_pair("limit", "2")
        .append_pair("offset", "2")
        .append_pair("query_urn", "urn:fixture");
    let (client, transport) = client(vec![
        json!({"collection":[track(1)],"next_href":continuation,
        "query_urn":"urn:fixture"}),
        json!({"collection":[track(2)],"next_href":null}),
    ]);
    let first = client.search_tag(&tag_request(), tag).unwrap();
    assert_eq!(first.next_page, Some(2));
    let mut next = tag_request();
    next.page = 2;
    next.query_urn = first.query_urn;
    assert_eq!(client.search_tag(&next, tag).unwrap().items.len(), 1);
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for (index, url) in requests.iter().enumerate() {
        assert_eq!(url.path(), "/prefix/_/api/v2/search/tracks");
        let query = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(query["q"], "*");
        assert_eq!(query["filter.genre_or_tag"], tag);
        assert_eq!(query["sort"], "popular");
        assert_eq!(query["offset"], (index * 2).to_string());
    }
}

#[test]
fn tag_search_rejects_changed_or_duplicated_filters_and_does_not_change_text_search() {
    for suffix in [
        "filter.genre_or_tag=other&sort=popular",
        "filter.genre_or_tag=ambient&sort=recent",
        "filter.genre_or_tag=ambient&filter.genre_or_tag=other&sort=popular",
        "sort=popular",
    ] {
        let next =
            format!("https://api-v2.soundcloud.com/search/tracks?q=*&limit=2&offset=2&{suffix}");
        let (client, _) = client(vec![json!({"collection":[],"next_href":next})]);
        assert!(client.search_tag(&tag_request(), "ambient").is_err());
    }
    let (client, transport) = client(vec![json!({"collection":[],"next_href":null})]);
    let mut request = tag_request();
    request.query = "ordinary title".into();
    assert!(client.search_tag(&request, "ambient").is_err());
    for tag in ["", "bad\ncontrol", &"a".repeat(MAX_TAG_BYTES + 1)] {
        assert!(client.search_tag(&tag_request(), tag).is_err());
    }
    assert!(transport.requests.lock().unwrap().is_empty());
    client.search(&request).unwrap();
    let requests = transport.requests.lock().unwrap();
    assert!(
        !requests[0]
            .query_pairs()
            .any(|(key, _)| key == "filter.genre_or_tag" || key == "sort")
    );
}

#[test]
fn album_pages_do_not_fetch_known_tracks_and_reject_unrequested_batch_ids() {
    let (client, transport) = client(vec![album(), json!([track(99)])]);
    let details = client.resolve_album(&album_url()).unwrap();
    let first = client.album_tracks(&details, 0, 1).unwrap();
    assert_eq!(first.next_offset, Some(1));
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    assert!(client.album_tracks(&details, 1, 2).is_err());
}

#[test]
fn artist_limits_and_oversized_albums_fail_without_unbounded_work() {
    let (client, transport) = client(vec![]);
    for limit in [0, 101, usize::MAX] {
        assert!(client.artist_tracks(&artist(), limit, None).is_err());
        assert!(client.artist_albums(&artist(), limit, None).is_err());
    }
    assert!(transport.requests.lock().unwrap().is_empty());
    let mut release = album();
    release["tracks"] = json!((1..=1001).map(|id| json!({"id":id})).collect::<Vec<_>>());
    let (client, transport) = self::client(vec![release]);
    assert!(client.resolve_album(&album_url()).is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}
