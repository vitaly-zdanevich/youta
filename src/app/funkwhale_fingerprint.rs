//! Explicit, pod-scoped identification of one selected Funkwhale audio upload.

use super::*;
use std::io::{Read, Write};

/// Identification never stages an unbounded remote object or retains its bytes.
const MAX_AUDIO_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CACHED_UPLOADS: usize = 32;
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);

/// An advertised immutable upload revision, separate from an expiring listen URL.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct UploadIdentity {
    source: String,
    upload_uuid: String,
    size: Option<u64>,
    duration: u64,
}

/// Fresh public resolution, retained only inside the explicit worker request.
struct AudioSource {
    identity: UploadIdentity,
    url: url::Url,
}

/// Results delivered on the existing serialized identifier's dedicated response lane.
pub(super) struct Completion {
    generation: u64,
    source: url::Url,
    result: Result<(UploadIdentity, Vec<MusicBrainzCandidate>), String>,
}

/// Work queued onto the same identifier and rate limiter as Local fingerprints.
pub(super) struct Request {
    generation: u64,
    source: url::Url,
    pub(super) cancellation: AudioIdentificationCancellation,
    responses: ResponseSender<Completion>,
}

/// A selected source owns cancellation and result publication until navigation.
struct Pending {
    generation: u64,
    source: url::Url,
    cancellation: AudioIdentificationCancellation,
}

/// RAM-only source-owned presentation; no audio bytes or account credentials persist.
pub(super) struct State {
    generation: u64,
    pending: Option<Pending>,
    results: HashMap<String, (UploadIdentity, Vec<MusicBrainzCandidate>)>,
    order: VecDeque<String>,
    owned_links: Vec<(String, String)>,
    sender: ResponseSender<Completion>,
    receiver: Receiver<Completion>,
}

impl State {
    /// Attaches the standard worker wake-up without starting network or helper work.
    pub(super) fn new(notifier: WorkerNotifier) -> Self {
        let (sender, receiver) = unbounded();
        Self {
            generation: 0,
            pending: None,
            results: HashMap::new(),
            order: VecDeque::new(),
            owned_links: Vec::new(),
            sender: ResponseSender::new(sender, notifier),
            receiver,
        }
    }

    /// Remembers a successful explicit lookup, including a confirmed absence.
    fn remember(&mut self, identity: UploadIdentity, candidates: Vec<MusicBrainzCandidate>) {
        let source = identity.source.clone();
        self.order.retain(|value| value != &source);
        while self.results.len() >= MAX_CACHED_UPLOADS && !self.results.contains_key(&source) {
            if let Some(oldest) = self.order.pop_front() {
                self.results.remove(&oldest);
            } else {
                break;
            }
        }
        self.order.push_back(source.clone());
        self.results.insert(source, (identity, candidates));
    }
}

/// A bounded upload-revision cache shared by successive jobs on the one worker.
#[derive(Default)]
pub(super) struct WorkerCache {
    results: HashMap<UploadIdentity, Vec<MusicBrainzCandidate>>,
    order: VecDeque<UploadIdentity>,
}

impl WorkerCache {
    /// An explicit fresh metadata resolution must precede every cache lookup.
    fn remember(&mut self, identity: UploadIdentity, candidates: Vec<MusicBrainzCandidate>) {
        self.order.retain(|value| value != &identity);
        while self.results.len() >= MAX_CACHED_UPLOADS && !self.results.contains_key(&identity) {
            if let Some(oldest) = self.order.pop_front() {
                self.results.remove(&oldest);
            } else {
                break;
            }
        }
        self.order.push_back(identity.clone());
        self.results.insert(identity, candidates);
    }
}

impl AppController {
    /// Accepts only the selected original-pod track, never a playing or stale item.
    fn selected_funkwhale_fingerprint_source(&self) -> Option<url::Url> {
        if self.view.screen != Screen::Funkwhale {
            return None;
        }
        let item = self.selected_funkwhale_queue_item().ok()?;
        let details = self.view.details.as_ref()?;
        if item.media.kind != MediaKind::Audio
            || item.media.id.source != SourceKind::Funkwhale
            || item.media.id.external_id != item.media.webpage_url.as_str()
            || self.view.rows.get(self.view.selected)?.media_id.as_ref() != Some(&item.media.id)
            || details.media_id.as_ref() != Some(&item.media.id)
            || details.webpage_url.as_ref() != Some(&item.media.webpage_url)
        {
            return None;
        }
        // Revealing an older playing item must retain its own pod after preferences change.
        source_pod(&item.media.webpage_url).ok()?;
        Some(item.media.webpage_url)
    }

    /// Reuses the fingerprint action without mutating provider-authored metadata links.
    pub(super) fn apply_funkwhale_fingerprint_details(&mut self) {
        if self.view.screen != Screen::Funkwhale {
            return;
        }
        let source = self.selected_funkwhale_fingerprint_source();
        if self
            .funkwhale_fingerprint
            .pending
            .as_ref()
            .is_some_and(|pending| source.as_ref() != Some(&pending.source))
        {
            self.cancel_funkwhale_fingerprint();
        }
        let state = &mut self.funkwhale_fingerprint;
        let Some(details) = self.view.details.as_mut() else {
            state.owned_links.clear();
            return;
        };
        details.links.retain(|link| {
            !state.owned_links.iter().any(|(label, url)| {
                link.label == *label && link.url == *url && link.wikidata_item_id.is_none()
            })
        });
        state.owned_links.clear();
        details.local_fingerprint_available = source.is_some();
        details.local_fingerprint_pending = state
            .pending
            .as_ref()
            .is_some_and(|pending| source.as_ref() == Some(&pending.source));
        let Some(source) = source else {
            return;
        };
        let Some((_, candidates)) = state.results.get(source.as_str()) else {
            return;
        };
        for (index, candidate) in candidates.iter().enumerate() {
            let label = format!(
                "MusicBrainz recording {} · {:.1}% match",
                index.saturating_add(1),
                candidate.score() * 100.0,
            );
            let url = candidate.url().to_string();
            if details.links.iter().any(|link| link.url == url) {
                continue;
            }
            state.owned_links.push((label.clone(), url.clone()));
            details.links.push(DetailLinkView {
                label,
                url,
                ..DetailLinkView::default()
            });
        }
    }

    /// Cancels only the remote owner; the shared queue is drained by the common policy.
    pub(super) fn cancel_funkwhale_fingerprint(&mut self) {
        if let Some(pending) = self.funkwhale_fingerprint.pending.take() {
            pending.cancellation.cancel();
        }
    }

    /// Indicates that the shared ASCII activity frame belongs to a remote lookup.
    pub(super) fn funkwhale_fingerprint_pending(&self) -> bool {
        self.funkwhale_fingerprint.pending.is_some()
    }

    /// Starts public audio staging only after the user explicitly requests identification.
    pub(super) fn fingerprint_selected_funkwhale_audio(&mut self) {
        let Some(source) = self.selected_funkwhale_fingerprint_source() else {
            return;
        };
        if self
            .funkwhale_fingerprint
            .pending
            .as_ref()
            .is_some_and(|pending| pending.source == source)
        {
            return;
        }
        if self.config.providers.acoustid_client_key.is_none() {
            self.show_actionable_message("AcoustID API key required", format!(
                "Create an AcoustID application key:\nhttps://acoustid.org/api-key\n\nAdd it to:\n{}\n\n[providers]\nacoustid_client_key = '...'\n\nThen restart Youta.",
                self.config.credentials_file().display(),
            ));
            return;
        }
        self.cancel_local_fingerprint();
        let Some(sender) = self.local_fingerprint_requests.clone() else {
            self.show_actionable_message(
                "Fingerprinting unavailable",
                "The fingerprint worker is unavailable. Restart Youta and try again.",
            );
            return;
        };
        let state = &mut self.funkwhale_fingerprint;
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        let cancellation = AudioIdentificationCancellation::new();
        let mut command = LocalFingerprintWorkerRequest::IdentifyFunkwhale(Request {
            generation,
            source: source.clone(),
            cancellation: cancellation.clone(),
            responses: state.sender.clone(),
        });
        loop {
            match sender.try_send(command) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) => {
                    if let Ok(stale) = self.local_fingerprint_request_drain.try_recv() {
                        stale.cancel();
                    }
                    command = returned;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.show_actionable_message(
                        "Fingerprinting unavailable",
                        "The fingerprint worker stopped. Restart Youta and try again.",
                    );
                    return;
                }
            }
        }
        // Old display data is not reused until this explicit job verifies the upload revision.
        state.results.remove(source.as_str());
        state.pending = Some(Pending {
            generation,
            source,
            cancellation,
        });
        self.view.local_fingerprint_animation_frame = 0;
        self.apply_funkwhale_fingerprint_details();
        self.view.status_line = "Identifying Funkwhale audio...".to_owned();
    }

    /// Discards completions whose selected track, pod, generation, or cancellation changed.
    pub(super) fn drain_funkwhale_fingerprint_responses(&mut self) {
        let selected = self.selected_funkwhale_fingerprint_source();
        if self
            .funkwhale_fingerprint
            .pending
            .as_ref()
            .is_some_and(|pending| selected.as_ref() != Some(&pending.source))
        {
            self.cancel_funkwhale_fingerprint();
        }
        while let Ok(response) = self.funkwhale_fingerprint.receiver.try_recv() {
            let Some(pending) = self.funkwhale_fingerprint.pending.as_ref() else {
                continue;
            };
            if pending.generation != response.generation || pending.source != response.source {
                continue;
            }
            let pending = self
                .funkwhale_fingerprint
                .pending
                .take()
                .expect("matching remote fingerprint");
            self.view.local_fingerprint_animation_frame = 0;
            if pending.cancellation.is_cancelled() || selected.as_ref() != Some(&pending.source) {
                continue;
            }
            match response.result {
                Ok((identity, candidates)) => {
                    self.view.status_line = if candidates.is_empty() {
                        "AcoustID found no linked MusicBrainz recording".to_owned()
                    } else {
                        format!(
                            "Found {} MusicBrainz recording candidates",
                            candidates.len()
                        )
                    };
                    self.funkwhale_fingerprint.remember(identity, candidates);
                }
                Err(error) => {
                    self.show_actionable_message("Funkwhale identification failed", error)
                }
            }
        }
        self.apply_funkwhale_fingerprint_details();
    }
}

/// Runs on the shared Local identifier so its AcoustID rate limit stays process-wide.
pub(super) fn identify(
    request: Request,
    identifier: &mut dyn AudioIdentifier,
    cache: &mut WorkerCache,
) {
    let result = identify_source(&request.source, &request.cancellation, identifier, cache);
    let _ = request.responses.send(Completion {
        generation: request.generation,
        source: request.source,
        result,
    });
}

/// Fresh metadata validation precedes cache reuse, and private staging outlives fpcalc.
fn identify_source(
    source: &url::Url,
    cancellation: &AudioIdentificationCancellation,
    identifier: &mut dyn AudioIdentifier,
    cache: &mut WorkerCache,
) -> Result<(UploadIdentity, Vec<MusicBrainzCandidate>), String> {
    check_cancelled(cancellation)?;
    let audio = resolve_audio(source, cancellation)?;
    if let Some(candidates) = cache.results.get(&audio.identity) {
        return Ok((audio.identity, candidates.clone()));
    }
    let directory = private_directory()?;
    let path = directory.path().join("audio");
    download_audio(&audio, &path, cancellation)?;
    check_cancelled(cancellation)?;
    let candidates = identifier
        .identify(&path, cancellation)
        .map_err(|error| error.to_string())?;
    check_cancelled(cancellation)?;
    cache.remember(audio.identity.clone(), candidates.clone());
    Ok((audio.identity, candidates))
}

/// Validates both canonical spellings without allowing arbitrary URL paths or secrets.
fn source_pod(source: &url::Url) -> Result<url::Url, String> {
    let pod =
        crate::providers::funkwhale::FunkwhaleProvider::instance_url_from_track_id(source.as_str())
            .map_err(|_| "Select an original Funkwhale track page".to_owned())?;
    let prefix = pod
        .join("library/tracks/")
        .map_err(|_| "Invalid Funkwhale track page")?;
    let suffix = source
        .as_str()
        .strip_prefix(prefix.as_str())
        .ok_or("Invalid Funkwhale track page")?;
    let number = suffix.strip_suffix('/').unwrap_or(suffix);
    let id = number
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0 && id.to_string() == number)
        .ok_or("Select a canonical Funkwhale track page")?;
    let tail = if source.path().ends_with('/') {
        "/"
    } else {
        ""
    };
    if source
        != &prefix
            .join(&format!("{id}{tail}"))
            .map_err(|_| "Invalid Funkwhale track page")?
    {
        return Err("Select a canonical Funkwhale track page".to_owned());
    }
    Ok(pod)
}

/// Resolves only public track metadata, never cookies, account tokens, or upload-owner data.
fn resolve_audio(
    source: &url::Url,
    cancellation: &AudioIdentificationCancellation,
) -> Result<AudioSource, String> {
    let pod = source_pod(source)?;
    let id = source
        .path()
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .ok_or("Invalid track ID")?;
    let agent = crate::providers::provider_agent(Duration::from_secs(15));
    for version in [1, 2] {
        check_cancelled(cancellation)?;
        let endpoint = pod
            .join(&format!("api/v{version}/tracks/{id}/"))
            .map_err(|_| "Invalid pod API")?;
        let raw: serde_json::Value = match crate::providers::get_bounded_json(
            &agent,
            &endpoint,
            crate::providers::DEFAULT_MAX_JSON_BYTES,
        ) {
            Ok(raw) => raw,
            Err(crate::providers::ProviderError::HttpStatus(404)) if version == 1 => continue,
            Err(crate::providers::ProviderError::HttpStatus(status)) => {
                return Err(format!("Funkwhale metadata returned HTTP status {status}"));
            }
            Err(_) => return Err("Could not read bounded Funkwhale track metadata".to_owned()),
        };
        check_cancelled(cancellation)?;
        return audio_source(source, &pod, &raw);
    }
    Err("This pod does not expose the track API".to_owned())
}

/// Pins a specific advertised upload to avoid fingerprinting a different selected rendition.
fn audio_source(
    source: &url::Url,
    pod: &url::Url,
    raw: &serde_json::Value,
) -> Result<AudioSource, String> {
    let expected_id = source
        .path()
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .and_then(|id| id.parse::<u64>().ok());
    if raw.get("id").and_then(json_number) != expected_id
        || raw.get("is_playable").is_some_and(|value| {
            value == &serde_json::Value::Bool(false) || value.as_str() == Some("false")
        })
    {
        return Err("This selected Funkwhale track has no matching public audio".to_owned());
    }
    let uploads = raw
        .get("uploads")
        .and_then(serde_json::Value::as_array)
        .ok_or("This pod did not advertise any public audio upload")?;
    let eligible = |upload: &&serde_json::Value| {
        upload
            .get("uuid")
            .and_then(serde_json::Value::as_str)
            .is_some_and(valid_uuid)
            && upload
                .get("duration")
                .and_then(json_number)
                .is_some_and(|value| value > 0 && u32::try_from(value).is_ok())
    };
    let upload = uploads.iter().filter(eligible).find(|upload| {
        !upload
            .get("size")
            .and_then(json_number)
            .is_some_and(|size| size > MAX_AUDIO_BYTES)
    });
    if upload.is_none()
        && uploads.iter().filter(eligible).any(|upload| {
            upload
                .get("size")
                .and_then(json_number)
                .is_some_and(|size| size > MAX_AUDIO_BYTES)
        })
    {
        return Err("The audio uploads exceed the 128 MiB identification limit".to_owned());
    }
    let upload = upload.ok_or("This pod did not advertise an audio upload with a full duration")?;
    let uuid = upload["uuid"]
        .as_str()
        .expect("validated upload UUID")
        .to_owned();
    let duration = json_number(&upload["duration"]).expect("validated upload duration");
    let size = upload.get("size").and_then(json_number);
    let advertised = upload
        .get("listen_url")
        .and_then(serde_json::Value::as_str)
        .or_else(|| raw.get("listen_url").and_then(serde_json::Value::as_str))
        .ok_or("This pod did not advertise a public audio URL")?;
    let mut url = pod
        .join(advertised)
        .map_err(|_| "The advertised audio URL is invalid")?;
    validate_audio_url(&url)?;
    // The documented listen endpoint accepts an explicit upload UUID. Never add this to CDN URLs.
    if url.origin() == pod.origin()
        && ["api/v1/listen/", "api/v2/listen/"].iter().any(|path| {
            pod.join(path)
                .is_ok_and(|prefix| url.path().starts_with(prefix.path()))
        })
    {
        let pairs = url
            .query_pairs()
            .filter(|(key, _)| key != "upload")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        url.set_query(None);
        url.query_pairs_mut()
            .extend_pairs(pairs)
            .append_pair("upload", &uuid);
    }
    Ok(AudioSource {
        identity: UploadIdentity {
            source: source.to_string(),
            upload_uuid: uuid,
            size,
            duration,
        },
        url,
    })
}

/// Accepts bounded credential-free web locators; errors never disclose signed URLs.
fn validate_audio_url(url: &url::Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.as_str().len() > 16_384
        || url.fragment().is_some()
    {
        return Err("The pod advertised an unsafe audio URL".to_owned());
    }
    Ok(())
}

/// Upload UUIDs are exact revision identities, not free-form remote strings.
fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

/// Accommodates pod versions that encode upload numbers as decimal strings.
fn json_number(value: &serde_json::Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

/// Creates automatic-cleanup staging with explicit private Unix permissions.
fn private_directory() -> Result<tempfile::TempDir, String> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("youta-identify-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder
        .tempdir()
        .map_err(|_| "Cannot create private identification staging".to_owned())
}

/// Follows only a bounded chain of credential-free HTTP(S) redirects without cookies.
fn download_audio(
    audio: &AudioSource,
    path: &Path,
    cancellation: &AudioIdentificationCancellation,
) -> Result<(), String> {
    check_cancelled(cancellation)?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TRANSFER_TIMEOUT))
        .timeout_recv_body(Some(Duration::from_secs(2)))
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into();
    let mut url = audio.url.clone();
    let deadline = Instant::now() + TRANSFER_TIMEOUT;
    for _ in 0..=5 {
        check_cancelled(cancellation)?;
        let remaining = transfer_time_left(deadline)?;
        validate_audio_url(&url)?;
        let mut response = agent
            .get(url.as_str())
            .header("Accept", "audio/*, application/octet-stream")
            .header("Accept-Encoding", "identity")
            .config()
            .timeout_global(Some(remaining))
            .build()
            .call()
            .map_err(|_| "Could not download public Funkwhale audio".to_owned())?;
        check_cancelled(cancellation)?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or("Audio redirect has no valid destination")?;
            url = url.join(location).map_err(|_| "Invalid audio redirect")?;
            continue;
        }
        if response.status().as_u16() != 200 {
            return Err(format!(
                "Funkwhale audio returned HTTP status {}",
                response.status().as_u16()
            ));
        }
        if response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value.starts_with("text/")
                    || value.contains("json")
                    || value.contains("xml")
                    || value.contains("mpegurl")
            })
        {
            return Err(
                "The audio URL returned a page or playlist instead of an audio file".to_owned(),
            );
        }
        let declared = response.body().content_length();
        if declared.is_some_and(|length| length > MAX_AUDIO_BYTES) {
            return Err("The audio upload exceeds the 128 MiB identification limit".to_owned());
        }
        if let (Some(api), Some(http)) = (audio.identity.size, declared)
            && api != http
        {
            return Err("Funkwhale audio size differs from the selected upload".to_owned());
        }
        let expected = declared.or(audio.identity.size);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .map_err(|_| "Cannot create private identification audio".to_owned())?;
        let mut reader = response.body_mut().as_reader();
        copy_audio_until(
            &mut reader,
            &mut file,
            expected,
            MAX_AUDIO_BYTES,
            cancellation,
            deadline,
        )?;
        return Ok(());
    }
    Err("The audio URL redirected too many times".to_owned())
}

/// Copies an entire bounded audio object; full-file duration remains correct for AcoustID.
#[cfg(test)]
fn copy_audio(
    reader: &mut impl Read,
    writer: &mut impl Write,
    expected: Option<u64>,
    limit: u64,
    cancellation: &AudioIdentificationCancellation,
) -> Result<u64, String> {
    copy_audio_until(
        reader,
        writer,
        expected,
        limit,
        cancellation,
        Instant::now() + TRANSFER_TIMEOUT,
    )
}

/// Shares one absolute transfer deadline across every redirect and body chunk.
fn copy_audio_until(
    reader: &mut impl Read,
    writer: &mut impl Write,
    expected: Option<u64>,
    limit: u64,
    cancellation: &AudioIdentificationCancellation,
    deadline: Instant,
) -> Result<u64, String> {
    let mut buffer = [0_u8; 64 * 1024];
    let mut count = 0_u64;
    let mut prefix = Vec::with_capacity(16);
    loop {
        check_cancelled(cancellation)?;
        transfer_time_left(deadline)?;
        let read = match reader.read(&mut buffer) {
            // Keep the same body and byte count, but recheck cancellation and
            // the original transfer deadline before retrying an interrupted read.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("Audio staging was interrupted or truncated".to_owned()),
            Ok(read) => read,
        };
        check_cancelled(cancellation)?;
        transfer_time_left(deadline)?;
        if read == 0 {
            break;
        }
        count = count
            .checked_add(read as u64)
            .ok_or("Audio size exceeded its safety limit")?;
        if count > limit || expected.is_some_and(|expected| count > expected) {
            return Err(
                "The audio upload exceeded its declared size or the 128 MiB identification limit"
                    .to_owned(),
            );
        }
        if prefix.len() < 16 {
            prefix.extend_from_slice(&buffer[..read.min(16 - prefix.len())]);
            if prefix.len() == 16 && !audio_prefix(&prefix) {
                return Err("The audio URL did not return a supported audio file".to_owned());
            }
        }
        writer
            .write_all(&buffer[..read])
            .map_err(|_| "Cannot write private identification audio".to_owned())?;
    }
    if count == 0 || expected.is_some_and(|expected| count != expected) {
        return Err("The public audio download was empty or truncated".to_owned());
    }
    if !audio_prefix(&prefix) {
        return Err("The audio URL did not return a supported audio file".to_owned());
    }
    Ok(count)
}

/// Request-local timeout budgets can only decrease during the complete transfer.
fn transfer_time_left(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| "Audio staging timed out".to_owned())
}

/// A remote manifest must never reach fpcalc as a local file containing nested URLs.
fn audio_prefix(prefix: &[u8]) -> bool {
    [
        b"fLaC".as_slice(),
        b"OggS",
        b"ID3",
        b"RIFF",
        b"RIFX",
        b"FORM",
        b".snd",
        b"wvpk",
        b"MAC ",
        b"MPCK",
        b"DSD ",
    ]
    .iter()
    .any(|magic| prefix.starts_with(magic))
        || prefix.get(4..8) == Some(b"ftyp")
        || prefix.starts_with(&[0x30, 0x26, 0xb2, 0x75, 0x8e, 0x66, 0xcf, 0x11])
        || prefix
            .get(..2)
            .is_some_and(|bytes| bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0)
        || prefix.starts_with(&[0x0b, 0x77])
}

/// Cancellation is checked at every network and staging boundary.
fn check_cancelled(cancellation: &AudioIdentificationCancellation) -> Result<(), String> {
    if cancellation.is_cancelled() {
        Err("Audio identification cancelled".to_owned())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
