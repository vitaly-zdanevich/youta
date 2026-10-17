//! Exact advertised upload-owner identities and their playable track catalogues.

use serde_json::Value;
use std::sync::Arc;
use url::Url;

use super::{FunkwhaleProvider, ProviderError, SearchPage, get_bounded_json};

/// A published account identity suitable for the API's exact actor scope filter.
///
/// Display names and profile URLs cannot construct this identity. Its federation
/// actor URL must agree with the account's domain, including any explicit port.
/// Immutable identity data is shared because queued jobs, metadata caches, and
/// navigation snapshots clone the same account without needing separate strings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunkwhaleUploader {
    identity: Arc<UploaderIdentity>,
}

/// Owns validated account data once while each source-scoped route retains a small handle.
#[derive(Debug, Eq, PartialEq)]
struct UploaderIdentity {
    full_username: String,
    actor_url: Url,
}

impl FunkwhaleUploader {
    /// Validates an advertised account and its matching federation actor URL.
    ///
    /// # Errors
    ///
    /// Rejects ambiguous scope syntax, unsafe actor URLs, and conflicting domains.
    pub fn new(full_username: String, actor_url: Url) -> Result<Self, ProviderError> {
        let invalid = || {
            ProviderError::InvalidRequest(
                "Funkwhale uploader requires an exact account and matching actor URL".to_owned(),
            )
        };
        if full_username.is_empty()
            || full_username.len() > 1024
            || full_username
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
            || actor_url.as_str().len() > 4096
            || !matches!(actor_url.scheme(), "http" | "https")
            || actor_url.host_str().is_none()
            || !actor_url.username().is_empty()
            || actor_url.password().is_some()
            || actor_url.query().is_some()
            || actor_url.fragment().is_some()
        {
            return Err(invalid());
        }
        let (username, domain) = full_username.split_once('@').ok_or_else(invalid)?;
        if username.is_empty()
            || username.chars().count() > 200
            || username.contains(',')
            || domain.is_empty()
            || domain.len() > 253
            || domain
                .chars()
                .any(|character| matches!(character, '@' | ',' | '/' | '\\' | '?' | '#' | '%'))
        {
            return Err(invalid());
        }
        let domain_url =
            Url::parse(&format!("{}://{domain}/", actor_url.scheme())).map_err(|_| invalid())?;
        if domain_url.host_str() != actor_url.host_str()
            || domain_url.port() != actor_url.port()
            || !domain_url.username().is_empty()
            || domain_url.password().is_some()
            || domain_url.path() != "/"
            || domain_url.query().is_some()
            || domain_url.fragment().is_some()
        {
            return Err(invalid());
        }
        Ok(Self {
            identity: Arc::new(UploaderIdentity {
                full_username,
                actor_url,
            }),
        })
    }

    /// Returns the exact published account, never a display-name fallback.
    #[must_use]
    pub fn full_username(&self) -> &str {
        &self.identity.full_username
    }

    /// Returns the credential-free federation actor identity, not a profile link.
    #[must_use]
    pub fn actor_url(&self) -> &Url {
        &self.identity.actor_url
    }

    /// Reads only authoritative identity fields from an advertised library actor.
    pub(super) fn from_actor(actor: &Value) -> Option<Self> {
        let full_username = actor.get("full_username")?.as_str()?;
        let fid = actor.get("fid")?.as_str()?;
        if fid.len() > 4096 || fid.chars().any(char::is_control) || fid.trim() != fid {
            return None;
        }
        let identity = Self::new(full_username.to_owned(), Url::parse(fid).ok()?).ok()?;
        let (username, domain) = identity.full_username().split_once('@')?;
        for (field, expected) in [("preferred_username", username), ("domain", domain)] {
            if let Some(value) = actor.get(field).filter(|value| !value.is_null())
                && !value.as_str()?.eq_ignore_ascii_case(expected)
            {
                return None;
            }
        }
        Some(identity)
    }
}

impl FunkwhaleProvider {
    /// Lists playable tracks from all of an advertised uploader's visible libraries.
    ///
    /// Requests stay on this provider's original pod even for a remote actor. The
    /// server's documented actor scope deduplicates tracks across libraries; each
    /// call reads one bounded page, not the actor's entire upload history.
    ///
    /// # Errors
    ///
    /// Rejects invalid pagination and propagates bounded pod API failures.
    pub fn uploader_tracks(
        &self,
        actor: &FunkwhaleUploader,
        page: u32,
    ) -> Result<SearchPage, ProviderError> {
        self.with_detected_api(|version| {
            let url = self.build_uploader_catalogue_url(actor, page, version)?;
            let raw = get_bounded_json(&self.agent, &url, self.max_json_bytes)?;
            self.convert_track_page(raw, page)
        })
    }

    /// Adds one URL-encoded actor scope to ordinary recent-track pagination.
    fn build_uploader_catalogue_url(
        &self,
        actor: &FunkwhaleUploader,
        page: u32,
        version: u8,
    ) -> Result<Url, ProviderError> {
        let mut url = self.build_catalogue_url(None, page, version)?;
        url.query_pairs_mut()
            .append_pair("scope", &format!("actor:{}", actor.full_username()));
        Ok(url)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{API_V1, API_V2, SearchItem};
    use super::*;
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// A bounded public-pod fixture records requests without contacting real accounts.
    struct Pod {
        base: Url,
        requests: Arc<Mutex<Vec<Url>>>,
        stop: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl Pod {
        /// Serves a fixed response sequence and stops promptly when its guard drops.
        fn new(responses: Vec<(u16, serde_json::Value)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base = Url::parse(&format!(
                "http://{}/original/",
                listener.local_addr().unwrap()
            ))
            .unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = requests.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let stopping = stop.clone();
            let origin = base.clone();
            let worker = std::thread::spawn(move || {
                let started = Instant::now();
                while !stopping.load(Ordering::Relaxed)
                    && started.elapsed() < Duration::from_secs(10)
                {
                    let mut socket = match listener.accept() {
                        Ok((socket, _)) => socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(error) => panic!("mock uploader pod accept: {error}"),
                    };
                    crate::test_support::configure_http_fixture_stream(
                        &socket,
                        Duration::from_secs(1),
                    );
                    let mut reader = BufReader::new(&mut socket);
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let path = line.split_whitespace().nth(1).unwrap().to_owned();
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                            break;
                        }
                    }
                    drop(reader);
                    let index = {
                        let mut requests = recorded.lock().unwrap();
                        let index = requests.len();
                        requests.push(origin.join(&path).unwrap());
                        index
                    };
                    let (status, body) = responses
                        .get(index)
                        .expect("unexpected extra uploader request");
                    let reason = if *status == 200 { "OK" } else { "Not Found" };
                    let body = body.to_string();
                    write!(socket, "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
            });
            Self {
                base,
                requests,
                stop,
                worker: Some(worker),
            }
        }

        /// Returns a finite-timeout client owned by this fixture's original pod.
        fn provider(&self) -> FunkwhaleProvider {
            FunkwhaleProvider::with_options(self.base.clone(), Duration::from_secs(2), 32 * 1024)
                .unwrap()
                .with_page_size(2)
        }
    }

    impl Drop for Pod {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                let result = worker.join();
                if !std::thread::panicking() {
                    result.expect("uploader pod fixture worker completed");
                }
            }
        }
    }

    /// Matches one advertised remote actor without fetching its remote host.
    fn actor() -> FunkwhaleUploader {
        FunkwhaleUploader::new(
            "emil@remote.example".to_owned(),
            Url::parse("https://remote.example/federation/actors/emil").unwrap(),
        )
        .unwrap()
    }

    /// Uses API version fallback only for an actual missing route, retaining scope.
    #[test]
    fn uploader_tracks_keep_scope_across_version_fallback_and_pagination() {
        let pod = Pod::new(vec![
            (404, serde_json::json!({"detail": "Not found"})),
            (
                200,
                serde_json::json!({"count": 3, "results": [
                {"id": 42, "title": "Public", "is_playable": true, "artist_credit": [{"name": "Fixture artist"}]},
                    {"id": 43, "title": "Blocked", "is_playable": false}
                ]}),
            ),
            (
                200,
                serde_json::json!({"count": 3, "results": [
                {"id": 44, "title": "Another public track", "is_playable": true, "artist_credit": [{"name": "Fixture artist"}]}
                ]}),
            ),
        ]);
        let provider = pod.provider();
        let first = provider.uploader_tracks(&actor(), 1).unwrap();
        assert_eq!(first.items.len(), 1);
        assert_eq!(first.next_page, Some(2));
        assert!(matches!(&first.items[0], SearchItem::Video(track) if track.title == "Public"));
        let second = provider.uploader_tracks(&actor(), 2).unwrap();
        assert_eq!(second.items.len(), 1);
        assert_eq!(second.next_page, None);
        let requests = pod.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        for (index, request) in requests.iter().enumerate() {
            assert_eq!(
                request.path(),
                if index == 0 {
                    "/original/api/v1/tracks/"
                } else {
                    "/original/api/v2/tracks/"
                }
            );
            assert!(
                request
                    .query_pairs()
                    .any(|(key, value)| key == "scope" && value == "actor:emil@remote.example")
            );
            assert!(
                request.query_pairs().any(
                    |(key, value)| key == "page" && value == if index == 2 { "2" } else { "1" }
                )
            );
        }
    }

    /// A normal catalogue request never enumerates libraries or requests upload details.
    #[test]
    fn uploader_tracks_use_one_request_when_the_first_api_version_is_available() {
        let pod = Pod::new(vec![(200, serde_json::json!({"count": 0, "results": []}))]);
        let provider = pod.provider();
        let result = provider.uploader_tracks(&actor(), 1).unwrap();
        assert!(result.items.is_empty());
        assert_eq!(result.next_page, None);
        assert_eq!(provider.detected_api.load(Ordering::Relaxed), API_V1);
        assert_eq!(pod.requests.lock().unwrap().len(), 1);
    }

    /// Real account identity remains distinct from display names and profile links.
    #[test]
    fn uploader_identity_requires_a_safe_matching_federation_origin() {
        let actor = FunkwhaleUploader::new(
            "emil@audio.pepemoss.com".to_owned(),
            Url::parse("https://audio.pepemoss.com/federation/actors/emil").unwrap(),
        )
        .unwrap();
        assert_eq!(actor.full_username(), "emil@audio.pepemoss.com");
        assert_eq!(
            actor.actor_url().as_str(),
            "https://audio.pepemoss.com/federation/actors/emil"
        );
        for username in [
            "emil",
            "emil@",
            "@audio.pepemoss.com",
            "emil@@audio.pepemoss.com",
            "emil@another.example",
            "emil@audio.pepemoss.com,all",
            "emil,all@audio.pepemoss.com",
            "emil\n@audio.pepemoss.com",
            " emil@audio.pepemoss.com",
            "emil@audio.pepemoss.com/path",
        ] {
            assert!(
                FunkwhaleUploader::new(username.to_owned(), actor.actor_url().clone()).is_err(),
                "unsafe or unrelated account {username:?}"
            );
        }
        for url in [
            "javascript:alert(1)",
            "file:///tmp/owner",
            "https://user@audio.pepemoss.com/federation/actors/emil",
            "https://user:password@audio.pepemoss.com/federation/actors/emil",
            "https://another.example/federation/actors/emil",
            "https://audio.pepemoss.com/federation/actors/emil?account=other",
            "https://audio.pepemoss.com/federation/actors/emil#other",
        ] {
            assert!(
                FunkwhaleUploader::new(
                    "emil@audio.pepemoss.com".to_owned(),
                    Url::parse(url).unwrap()
                )
                .is_err(),
                "unsafe federation actor {url:?}"
            );
        }
    }

    /// Public account spelling survives URI encoding, including nondefault pod ports.
    #[test]
    fn uploader_scope_preserves_published_unicode_plus_and_port() {
        let actor = FunkwhaleUploader::new(
            "Музыка+mix@pod.example:8443".to_owned(),
            Url::parse("https://pod.example:8443/federation/actors/music").unwrap(),
        )
        .unwrap();
        let provider =
            FunkwhaleProvider::new(Url::parse("https://original.example/").unwrap()).unwrap();
        let url = provider
            .build_uploader_catalogue_url(&actor, 1, API_V1)
            .unwrap();
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "scope" && value == "actor:Музыка+mix@pod.example:8443")
        );
        assert!(
            FunkwhaleUploader::new(
                "Музыка+mix@pod.example:8443".to_owned(),
                Url::parse("https://pod.example/federation/actors/music").unwrap()
            )
            .is_err()
        );
    }

    /// Scope is an exact server-supported account filter, never a title search.
    #[test]
    fn uploader_catalogue_retains_original_pod_and_normal_pagination() {
        let provider = FunkwhaleProvider::new(Url::parse("https://pod.example/prefix/").unwrap())
            .unwrap()
            .with_page_size(37);
        let actor = FunkwhaleUploader::new(
            "emil@remote.example".to_owned(),
            Url::parse("https://remote.example/users/emil").unwrap(),
        )
        .unwrap();
        let url = provider
            .build_uploader_catalogue_url(&actor, 2, API_V2)
            .unwrap();
        assert_eq!(url.host_str(), Some("pod.example"));
        assert_eq!(url.path(), "/prefix/api/v2/tracks/");
        let query = url.query_pairs().collect::<BTreeMap<_, _>>();
        assert_eq!(query.get("q").map(|value| value.as_ref()), Some(""));
        assert_eq!(query.get("page").map(|value| value.as_ref()), Some("2"));
        assert_eq!(
            query.get("page_size").map(|value| value.as_ref()),
            Some("37")
        );
        assert_eq!(
            query.get("playable").map(|value| value.as_ref()),
            Some("true")
        );
        assert_eq!(
            query.get("include_channels").map(|value| value.as_ref()),
            Some("true")
        );
        assert_eq!(
            query.get("ordering").map(|value| value.as_ref()),
            Some("-creation_date")
        );
        assert_eq!(
            query.get("scope").map(|value| value.as_ref()),
            Some("actor:emil@remote.example")
        );
        assert!(!query.contains_key("artist"));
        assert!(!query.contains_key("album"));
        assert!(!query.contains_key("library"));
        assert!(
            provider
                .build_uploader_catalogue_url(&actor, 0, API_V2)
                .is_err()
        );
        assert!(
            provider
                .build_uploader_catalogue_url(&actor, 1, 99)
                .is_err()
        );
    }
}
