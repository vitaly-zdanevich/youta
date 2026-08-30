//! Review-first Opus note creation through Evernote's EDAM API.
//!
//! The module owns ENML construction, attachment integrity metadata, and the
//! blocking Thrift transport. Authentication tokens remain in the Rust
//! process; front-ends receive only redacted editor state.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use evernote::note_store::{NoteStoreSyncClient, TNoteStoreSyncClient};
use evernote::types::{self, Data, NoteAttributes, Resource, ResourceAttributes};
use evernote::user_store::{TUserStoreSyncClient, UserStoreSyncClient};
use serde::{Deserialize, Serialize};
use thrift::protocol::{TBinaryInputProtocol, TBinaryOutputProtocol};
use thrift::transport::{ReadHalf, TIoChannel, WriteHalf};
use url::Url;

/// Production Evernote `UserStore` endpoint used to discover an account shard.
pub const EVERNOTE_USER_STORE_URL: &str = "https://www.evernote.com/edam/user";

/// Evernote's personal developer-token page.
pub const EVERNOTE_DEVELOPER_TOKEN_URL: &str = "https://www.evernote.com/api/DeveloperToken.action";

const EVERNOTE_CLIENT_NAME: &str = concat!("Youta/", env!("CARGO_PKG_VERSION"));
const MAXIMUM_NOTE_BODY_BYTES: usize = 512 * 1024;
const MAXIMUM_TAGS: usize = 100;
const MAXIMUM_TAG_BYTES: usize = 100;

type InputProtocol<C> = TBinaryInputProtocol<ReadHalf<ThriftHttpChannel<C>>>;
type OutputProtocol<C> = TBinaryOutputProtocol<WriteHalf<ThriftHttpChannel<C>>>;

/// Optional metadata reviewed before Youta stages any audio.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EvernoteNoteDraft {
    /// Optional note title; Evernote supplies its own default when omitted.
    pub title: String,
    /// Optional plain-text note body, including an inserted caption transcript.
    pub body: String,
    /// Optional comma-separated Evernote tag names.
    pub tags: String,
    /// Immutable canonical provider link, or empty for a local file.
    pub source_url: String,
}

impl EvernoteNoteDraft {
    /// Validates metadata and returns normalized, de-duplicated tag names.
    ///
    /// # Errors
    ///
    /// Returns an explanation when a field exceeds Evernote's bounded model or
    /// a present source is not a credential-free remote HTTP(S) link.
    pub fn validate(&self) -> Result<Vec<String>, String> {
        let title = self.title.trim();
        if title.len() > 255 || title.chars().any(char::is_control) {
            return Err(
                "The Evernote title exceeds 255 bytes or contains control characters".to_owned(),
            );
        }
        if self.body.len() > MAXIMUM_NOTE_BODY_BYTES {
            return Err("The Evernote note body exceeds 512 KiB".to_owned());
        }
        if !self.source_url.trim().is_empty() {
            let source = Url::parse(self.source_url.trim()).map_err(|_| {
                "The Evernote source URL must be an HTTP(S) video or audio link".to_owned()
            })?;
            if !matches!(source.scheme(), "http" | "https")
                || source.host_str().is_none()
                || !source.username().is_empty()
                || source.password().is_some()
            {
                return Err(
                    "The Evernote source URL must be an HTTP(S) video or audio link".to_owned(),
                );
            }
        }

        let mut tags = Vec::new();
        for tag in self
            .tags
            .split(',')
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
        {
            if tag.len() > MAXIMUM_TAG_BYTES || tag.chars().any(char::is_control) {
                return Err(
                    "An Evernote tag exceeds 100 bytes or contains control characters".to_owned(),
                );
            }
            if !tags
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(tag))
            {
                tags.push(tag.to_owned());
            }
            if tags.len() > MAXIMUM_TAGS {
                return Err("An Evernote note can contain at most 100 tags".to_owned());
            }
        }
        Ok(tags)
    }

    /// Renders a complete ENML document linked to one Opus resource hash.
    #[must_use]
    pub fn enml(&self, audio_hash: &str) -> String {
        let mut content = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE en-note SYSTEM \"http://xml.evernote.com/pub/enml2.dtd\">\n<en-note>\n",
        );
        for line in self.body.lines() {
            if line.is_empty() {
                content.push_str("<div><br/></div>\n");
            } else {
                content.push_str("<div>");
                content.push_str(&escape_xml(line));
                content.push_str("</div>\n");
            }
        }
        if !self.body.is_empty() {
            content.push_str("<div><br/></div>\n");
        }
        if !self.source_url.trim().is_empty() {
            let source = escape_xml(self.source_url.trim());
            let _ = writeln!(content, "<div><a href=\"{source}\">Source</a></div>");
        }
        let _ = writeln!(
            content,
            "<div><br/></div>\n<en-media type=\"audio/ogg\" hash=\"{audio_hash}\"/>"
        );
        content.push_str("</en-note>");
        content
    }
}

/// Appends normalized `YouTube` captions as one distinct note-body edit.
#[must_use]
pub fn body_with_captions(body: &str, captions: &str) -> String {
    let body = body.trim_end();
    let captions = captions.trim();
    if body.is_empty() {
        format!("YouTube captions\n\n{captions}")
    } else {
        format!("{body}\n\nYouTube captions\n\n{captions}")
    }
}

/// Returns the latest saved body snapshot for Ctrl+Z handling.
#[must_use]
pub fn body_without_last_edit(history: &[String]) -> Option<String> {
    history.last().cloned()
}

/// Identity returned after Evernote accepts a note and its Opus resource.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvernoteNoteResult {
    /// Server-assigned note GUID.
    pub guid: String,
    /// Canonical Evernote web page for the created note.
    pub url: Url,
}

/// Mockable HTTP boundary used by the generated synchronous Thrift clients.
pub trait ThriftHttpClient: Clone + Send + Sync + 'static {
    /// Sends one serialized Thrift request and returns its complete response.
    ///
    /// # Errors
    ///
    /// Returns an opaque transport explanation without retaining credentials.
    fn post_thrift(&self, url: &str, body: Vec<u8>) -> Result<Vec<u8>, String>;
}

/// Production Thrift-over-HTTPS transport built on Youta's existing `ureq` stack.
#[derive(Clone)]
pub struct UreqThriftHttpClient {
    agent: ureq::Agent,
}

impl Default for UreqThriftHttpClient {
    fn default() -> Self {
        let agent = ureq::Agent::config_builder()
            .user_agent(EVERNOTE_CLIENT_NAME)
            .build()
            .into();
        Self { agent }
    }
}

impl ThriftHttpClient for UreqThriftHttpClient {
    fn post_thrift(&self, url: &str, body: Vec<u8>) -> Result<Vec<u8>, String> {
        let mut response = self
            .agent
            .post(url)
            .header("Content-Type", "application/x-thrift")
            .send(body.as_slice())
            .map_err(|error| format!("Evernote request failed: {error}"))?;
        response
            .body_mut()
            .read_to_vec()
            .map_err(|error| format!("Could not read the Evernote response: {error}"))
    }
}

/// Blocking Evernote EDAM client for one configured authentication token.
pub struct EvernoteClient<C = UreqThriftHttpClient>
where
    C: ThriftHttpClient,
{
    token: String,
    user_store_url: String,
    note_store_url: Arc<Mutex<Option<String>>>,
    http: C,
}

impl EvernoteClient<UreqThriftHttpClient> {
    /// Creates a production client without logging or serializing the token.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_http_client(token, UreqThriftHttpClient::default())
    }
}

impl<C> EvernoteClient<C>
where
    C: ThriftHttpClient,
{
    /// Creates a client around a supplied transport, primarily for deterministic tests.
    #[must_use]
    pub fn with_http_client(token: impl Into<String>, http: C) -> Self {
        Self {
            token: token.into(),
            user_store_url: EVERNOTE_USER_STORE_URL.to_owned(),
            note_store_url: Arc::new(Mutex::new(None)),
            http,
        }
    }

    /// Creates one Evernote note and attaches the complete staged Opus file.
    ///
    /// This call blocks and retains the attachment bytes until Evernote returns.
    /// Callers should therefore run it on Youta's dedicated export worker.
    ///
    /// # Errors
    ///
    /// Returns an explanation when metadata, the staged file, shard discovery,
    /// transport, or Evernote's `createNote` operation fails.
    pub fn create_opus_note(
        &self,
        path: &Path,
        file_name: &str,
        draft: &EvernoteNoteDraft,
    ) -> Result<EvernoteNoteResult, String> {
        let tags = draft.validate()?;
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("Cannot inspect the staged Evernote audio: {error}"))?;
        if !metadata.file_type().is_file() || metadata.len() == 0 {
            return Err("The staged Evernote audio must be a non-empty regular file".to_owned());
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("opus") {
            return Err("Evernote audio from Youta must use Opus".to_owned());
        }
        let bytes = fs::read(path)
            .map_err(|error| format!("Cannot read the staged Evernote audio: {error}"))?;
        let digest = md5::compute(&bytes).0;
        let digest_hex = hex_md5(digest);
        let resource = opus_resource(file_name, &bytes)?;
        let note = types::Note {
            title: (!draft.title.trim().is_empty()).then(|| draft.title.trim().to_owned()),
            content: Some(draft.enml(&digest_hex)),
            attributes: Some(NoteAttributes {
                source: Some("youta".to_owned()),
                source_u_r_l: (!draft.source_url.trim().is_empty())
                    .then(|| draft.source_url.trim().to_owned()),
                source_application: Some(EVERNOTE_CLIENT_NAME.to_owned()),
                ..NoteAttributes::default()
            }),
            tag_names: (!tags.is_empty()).then_some(tags),
            resources: Some(vec![resource]),
            ..types::Note::default()
        };
        let mut client = self.note_store_client()?;
        let created = client
            .create_note(self.token.clone(), note)
            .map_err(|error| format!("Evernote could not create the note: {error}"))?;
        let guid = created
            .guid
            .filter(|guid| !guid.trim().is_empty())
            .ok_or_else(|| "Evernote created the note without returning its GUID".to_owned())?;
        let mut user_store = self.user_store_client()?;
        let user = user_store
            .get_user(self.token.clone())
            .map_err(|error| format!("Evernote could not identify the note owner: {error}"))?;
        let user_id = user
            .id
            .ok_or_else(|| "Evernote did not return the note owner's account ID".to_owned())?;
        let shard_id = user
            .shard_id
            .filter(|shard| !shard.trim().is_empty())
            .ok_or_else(|| "Evernote did not return the note owner's shard".to_owned())?;
        let url = Url::parse(&format!(
            "https://www.evernote.com/shard/{shard_id}/nl/{user_id}/{guid}"
        ))
        .map_err(|error| format!("Evernote returned an invalid note identity: {error}"))?;
        Ok(EvernoteNoteResult { guid, url })
    }

    fn note_store_client(
        &self,
    ) -> Result<NoteStoreSyncClient<InputProtocol<C>, OutputProtocol<C>>, String> {
        let channel = ThriftHttpChannel::new(self.note_store_url()?, self.http.clone());
        let (read, write) = channel
            .split()
            .map_err(|error| format!("Could not initialize Evernote NoteStore: {error}"))?;
        Ok(NoteStoreSyncClient::new(
            TBinaryInputProtocol::new(read, true),
            TBinaryOutputProtocol::new(write, true),
        ))
    }

    fn user_store_client(
        &self,
    ) -> Result<UserStoreSyncClient<InputProtocol<C>, OutputProtocol<C>>, String> {
        let channel = ThriftHttpChannel::new(self.user_store_url.clone(), self.http.clone());
        let (read, write) = channel
            .split()
            .map_err(|error| format!("Could not initialize Evernote UserStore: {error}"))?;
        Ok(UserStoreSyncClient::new(
            TBinaryInputProtocol::new(read, true),
            TBinaryOutputProtocol::new(write, true),
        ))
    }

    fn note_store_url(&self) -> Result<String, String> {
        if let Some(url) = self
            .note_store_url
            .lock()
            .map_err(|_| "Evernote NoteStore URL cache is poisoned".to_owned())?
            .clone()
        {
            return Ok(url);
        }
        let mut client = self.user_store_client()?;
        let urls = client
            .get_user_urls(self.token.clone())
            .map_err(|error| format!("Evernote could not discover the account shard: {error}"))?;
        let url = urls
            .note_store_url
            .filter(|url| !url.trim().is_empty())
            .ok_or_else(|| "Evernote did not return a NoteStore URL".to_owned())?;
        *self
            .note_store_url
            .lock()
            .map_err(|_| "Evernote NoteStore URL cache is poisoned".to_owned())? =
            Some(url.clone());
        Ok(url)
    }
}

fn opus_resource(file_name: &str, body: &[u8]) -> Result<Resource, String> {
    let size = i32::try_from(body.len())
        .map_err(|_| "The staged Opus file exceeds Evernote's resource size field".to_owned())?;
    let file_name = sanitized_opus_file_name(file_name);
    Ok(Resource {
        data: Some(Data {
            body_hash: Some(md5::compute(body).0.to_vec()),
            size: Some(size),
            body: Some(body.to_vec()),
        }),
        mime: Some("audio/ogg".to_owned()),
        attributes: Some(ResourceAttributes {
            file_name: Some(file_name),
            attachment: Some(true),
            ..ResourceAttributes::default()
        }),
        ..Resource::default()
    })
}

fn sanitized_opus_file_name(value: &str) -> String {
    let mut stem = value
        .trim()
        .trim_end_matches(".opus")
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            character if character.is_control() => ' ',
            character => character,
        })
        .collect::<String>();
    stem = stem.split_whitespace().collect::<Vec<_>>().join(" ");
    if stem.is_empty() {
        stem.push_str("audio");
    }
    stem.truncate(stem.floor_char_boundary(180));
    format!("{stem}.opus")
}

fn escape_xml(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            character if !character.is_control() || matches!(character, '\n' | '\t' | '\r') => {
                escaped.push(character);
            }
            _ => {}
        }
    }
    escaped
}

fn hex_md5(digest: [u8; 16]) -> String {
    let mut hex = String::with_capacity(32);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[derive(Clone)]
struct ThriftHttpChannel<C>
where
    C: ThriftHttpClient,
{
    endpoint: String,
    http: C,
    state: Arc<Mutex<ThriftHttpState>>,
}

#[derive(Default)]
struct ThriftHttpState {
    read_bytes: Vec<u8>,
    read_position: usize,
    write_bytes: Vec<u8>,
}

impl<C> ThriftHttpChannel<C>
where
    C: ThriftHttpClient,
{
    fn new(endpoint: String, http: C) -> Self {
        Self {
            endpoint,
            http,
            state: Arc::new(Mutex::new(ThriftHttpState::default())),
        }
    }
}

impl<C> TIoChannel for ThriftHttpChannel<C>
where
    C: ThriftHttpClient,
{
    fn split(self) -> thrift::Result<(ReadHalf<Self>, WriteHalf<Self>)>
    where
        Self: Sized,
    {
        Ok((ReadHalf::new(self.clone()), WriteHalf::new(self)))
    }
}

impl<C> Read for ThriftHttpChannel<C>
where
    C: ThriftHttpClient,
{
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("Evernote transport state is poisoned"))?;
        let remaining = state.read_bytes.len().saturating_sub(state.read_position);
        let length = remaining.min(buffer.len());
        if length == 0 {
            return Ok(0);
        }
        let start = state.read_position;
        let end = start + length;
        buffer[..length].copy_from_slice(&state.read_bytes[start..end]);
        state.read_position = end;
        Ok(length)
    }
}

impl<C> Write for ThriftHttpChannel<C>
where
    C: ThriftHttpClient,
{
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.state
            .lock()
            .map_err(|_| io::Error::other("Evernote transport state is poisoned"))?
            .write_bytes
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let request = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| io::Error::other("Evernote transport state is poisoned"))?;
            std::mem::take(&mut state.write_bytes)
        };
        let response = self
            .http
            .post_thrift(&self.endpoint, request)
            .map_err(io::Error::other)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("Evernote transport state is poisoned"))?;
        state.read_bytes = response;
        state.read_position = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_accepts_empty_optional_fields_and_parses_tags() {
        let draft = EvernoteNoteDraft {
            title: String::new(),
            body: String::new(),
            tags: " music, archive ,music ".to_owned(),
            source_url: "https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_owned(),
        };

        assert_eq!(
            draft.validate().expect("valid optional metadata"),
            vec!["music", "archive"]
        );
    }

    #[test]
    fn draft_requires_a_safe_remote_source_url() {
        let mut draft = EvernoteNoteDraft::default();
        draft.source_url = "file:///tmp/audio.opus".to_owned();

        assert_eq!(
            draft.validate().expect_err("local source URL must fail"),
            "The Evernote source URL must be an HTTP(S) video or audio link"
        );
    }

    #[test]
    fn local_draft_accepts_an_absent_source_and_omits_the_source_link() {
        let draft = EvernoteNoteDraft {
            title: "Local recording".to_owned(),
            body: "Recorded locally".to_owned(),
            tags: "archive".to_owned(),
            source_url: String::new(),
        };

        assert_eq!(draft.validate().expect("local draft"), vec!["archive"]);
        let rendered = draft.enml("00112233445566778899aabbccddeeff");
        assert!(!rendered.contains("href="));
        assert!(!rendered.contains(">Source</a>"));
        assert!(rendered.contains("<en-media type=\"audio/ogg\""));
    }

    #[test]
    fn enml_escapes_body_and_links_source_and_opus() {
        let draft = EvernoteNoteDraft {
            title: "A title".to_owned(),
            body: "First < line\nSecond & line".to_owned(),
            tags: String::new(),
            source_url: "https://www.youtube.com/watch?v=a&list=b".to_owned(),
        };
        let rendered = draft.enml("00112233445566778899aabbccddeeff");

        assert!(rendered.contains("First &lt; line"));
        assert!(rendered.contains("Second &amp; line"));
        assert!(rendered.contains("href=\"https://www.youtube.com/watch?v=a&amp;list=b\""));
        assert!(
            rendered.contains(
                "<en-media type=\"audio/ogg\" hash=\"00112233445566778899aabbccddeeff\"/>"
            )
        );
    }

    #[test]
    fn captions_append_as_a_distinct_undoable_body_edit() {
        let original = "Creator description";
        let inserted = body_with_captions(original, "[00:01] Opening");

        assert_eq!(
            inserted,
            "Creator description\n\nYouTube captions\n\n[00:01] Opening"
        );
        assert_eq!(
            body_without_last_edit(&[original.to_owned()]),
            Some(original.to_owned())
        );
    }

    #[test]
    fn attachment_uses_opus_mime_filename_size_and_md5() {
        let resource = opus_resource("Example.opus", b"opus fixture").expect("resource");
        let data = resource.data.expect("resource data");
        let attributes = resource.attributes.expect("resource attributes");

        assert_eq!(resource.mime.as_deref(), Some("audio/ogg"));
        assert_eq!(data.size, Some(12));
        assert_eq!(data.body.as_deref(), Some(b"opus fixture".as_slice()));
        assert_eq!(
            data.body_hash,
            Some(md5::compute(b"opus fixture").0.to_vec())
        );
        assert_eq!(attributes.file_name.as_deref(), Some("Example.opus"));
        assert_eq!(attributes.attachment, Some(true));
    }
}
