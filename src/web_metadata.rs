//! Bounded, credential-free metadata inspection for the selected Web media file.

use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use url::Url;

use crate::web_browser::validate_web_url;

const BLOCK_BYTES: usize = 64 * 1024;
const MAX_READ_BYTES: usize = 8 * 1024 * 1024;
const MAX_NON_RANGE_BYTES: usize = 1024 * 1024;
const MAX_PROBE_PREFIX_BYTES: usize = 256 * 1024;
const MAX_REQUESTS: usize = 32;
const MAX_REDIRECTS: usize = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Optional information obtained without downloading the complete remote file.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct WebMediaMetadata {
    /// Embedded title, or `None` when the filename should remain the fallback.
    pub(crate) title: Option<String>,
    /// Performing artist from the media tags.
    pub(crate) artist: Option<String>,
    /// Album or collection from the media tags.
    pub(crate) album: Option<String>,
    /// Genre from the media tags.
    pub(crate) genre: Option<String>,
    /// Embedded description or comment, bounded and terminal-safe.
    pub(crate) comment: Option<String>,
    /// Playable duration when the container exposes it within the read budget.
    pub(crate) duration: Option<Duration>,
    /// Complete resource length, never the size of a downloaded prefix.
    pub(crate) size_bytes: Option<u64>,
    /// Container identified from parsed bytes, not guessed from the URL.
    pub(crate) container: Option<String>,
    /// Audio codec when the parser can identify it unambiguously.
    pub(crate) codec: Option<String>,
    /// Audio bitrate in kilobits per second.
    pub(crate) bitrate_kbps: Option<u32>,
    /// Audio sample rate in hertz.
    pub(crate) sample_rate_hz: Option<u32>,
    /// Number of audio channels.
    pub(crate) channels: Option<u8>,
    /// Embedded cover bytes for the existing thumbnail validator/cache.
    pub(crate) artwork: Option<WebEmbeddedArtwork>,
    /// Bounded file prefix for an optional, local-only video metadata probe.
    ///
    /// Workers must remove this transient data before caching the result. A
    /// helper must inspect only a finite local pipe, never follow the remote URL.
    pub(crate) probe_prefix: Vec<u8>,
}

/// An embedded cover whose bytes still require normal thumbnail validation.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WebEmbeddedArtwork {
    /// Compressed image bytes, limited to 2 MiB.
    pub(crate) bytes: Vec<u8>,
    /// MIME type declared by the parsed picture.
    pub(crate) mime_type: String,
}

/// Performs best-effort inspection with one shared network and parsing deadline.
#[derive(Clone, Debug)]
pub(crate) struct WebMetadataClient {
    timeout: Duration,
}

impl Default for WebMetadataClient {
    fn default() -> Self {
        Self {
            timeout: REQUEST_TIMEOUT,
        }
    }
}

impl WebMetadataClient {
    /// Reads selected-file metadata; errors leave unavailable fields empty.
    #[cfg(test)]
    pub(crate) fn read(&self, url: &Url) -> WebMediaMetadata {
        self.read_cancellable(url, &AtomicBool::new(false))
    }

    /// Reads metadata while honoring cancellation before requests and reads.
    ///
    /// At most 8 MiB and 32 HTTP requests are consumed, including redirects.
    /// Servers ignoring Range supply at most a 1 MiB prefix; they are never
    /// retried for distant offsets or read beyond that prefix as a fallback.
    pub(crate) fn read_cancellable(&self, url: &Url, cancelled: &AtomicBool) -> WebMediaMetadata {
        let mut result = WebMediaMetadata::default();
        if cancelled.load(Ordering::Relaxed) || validate_web_url(url).is_err() {
            return result;
        }
        let mut reader = HttpRangeReader::new(url.clone(), self.timeout, cancelled);
        let _ = reader.fetch_block(0);
        result.size_bytes = reader.length;
        if reader.blocks.is_empty() {
            return result;
        }
        #[cfg(feature = "local-metadata")]
        if read_tags(&mut reader, &mut result)
            && result.codec.is_some()
            && result.duration.is_some()
        {
            return result;
        }
        // Do not pass an unbounded remote input to ffprobe. Only bytes already
        // obtained under the shared limits can be offered to a local helper.
        result.probe_prefix = reader.blocks.get(&0).map_or_else(Vec::new, |bytes| {
            bytes[..bytes.len().min(MAX_PROBE_PREFIX_BYTES)].to_vec()
        });
        result
    }
}

/// A cached, seekable view of bounded byte ranges from one HTTP resource.
struct HttpRangeReader<'a> {
    url: Url,
    deadline: Instant,
    cancelled: &'a AtomicBool,
    blocks: BTreeMap<u64, Vec<u8>>,
    position: u64,
    length: Option<u64>,
    accepts_ranges: bool,
    read_bytes: usize,
    requests: usize,
    validator: Option<String>,
}

impl<'a> HttpRangeReader<'a> {
    fn new(mut url: Url, timeout: Duration, cancelled: &'a AtomicBool) -> Self {
        url.set_fragment(None);
        Self {
            url,
            deadline: Instant::now() + timeout,
            cancelled,
            blocks: BTreeMap::new(),
            position: 0,
            length: None,
            accepts_ranges: true,
            read_bytes: 0,
            requests: 0,
            validator: None,
        }
    }

    /// Applies the same deadline to cached reads and seeks, not just HTTP calls.
    fn remaining(&self) -> io::Result<Duration> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Web metadata inspection cancelled",
            ));
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Web metadata inspection timed out",
            ));
        }
        Ok(remaining)
    }

    /// Fetches one aligned block, validating redirects and Content-Range.
    #[allow(
        clippy::too_many_lines,
        reason = "Keep redirect validation, budget accounting, and response validation in one auditable request path"
    )]
    fn fetch_block(&mut self, start: u64) -> io::Result<()> {
        if !self.accepts_ranges {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Web server does not support seeking",
            ));
        }
        let end = start
            .checked_add(BLOCK_BYTES as u64 - 1)
            .ok_or_else(invalid_response)?;
        let mut current = self.url.clone();
        for redirects in 0..=MAX_REDIRECTS {
            let remaining = self.remaining()?;
            if self.requests >= MAX_REQUESTS || self.read_bytes >= MAX_READ_BYTES {
                return Err(io::Error::other("Web metadata inspection budget exhausted"));
            }
            validate_web_url(&current).map_err(|_| invalid_response())?;
            self.requests += 1;
            // A fresh agent per hop cannot replay Set-Cookie; automatic redirects
            // and inherited proxy credentials are deliberately disabled.
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(remaining))
                .max_redirects(0)
                .http_status_as_error(false)
                .proxy(None)
                .user_agent(concat!("youta/", env!("CARGO_PKG_VERSION")))
                .build()
                .into();
            let mut request = agent
                .get(current.as_str())
                .header("Range", format!("bytes={start}-{end}"))
                .header("Accept-Encoding", "identity");
            if let Some(validator) = &self.validator {
                request = request.header("If-Range", validator);
            }
            let mut response = request
                .call()
                .map_err(|_| io::Error::other("Web metadata request failed"))?;
            self.remaining()?;
            let status = response.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                if redirects == MAX_REDIRECTS {
                    return Err(invalid_response());
                }
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(invalid_response)?;
                current = current.join(location).map_err(|_| invalid_response())?;
                current.set_fragment(None);
                validate_web_url(&current).map_err(|_| invalid_response())?;
                continue;
            }
            if response
                .headers()
                .get("content-encoding")
                .is_some_and(|value| value.as_bytes() != b"identity")
            {
                return Err(invalid_response());
            }
            let validator = response
                .headers()
                .get("etag")
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.starts_with("W/"))
                .or_else(|| {
                    response
                        .headers()
                        .get("last-modified")
                        .and_then(|value| value.to_str().ok())
                })
                .map(str::to_owned);
            if self.validator.is_some() && self.validator != validator {
                return Err(io::Error::other(
                    "Web media changed during metadata inspection",
                ));
            }
            let (limit, expected, complete) = match status {
                206 => {
                    let header = response
                        .headers()
                        .get("content-range")
                        .and_then(|value| value.to_str().ok())
                        .ok_or_else(invalid_response)?;
                    let (actual_start, actual_end, length) =
                        parse_content_range(header).ok_or_else(invalid_response)?;
                    if actual_start != start
                        || actual_end > end
                        || self.length.is_some_and(|old| old != length)
                    {
                        return Err(invalid_response());
                    }
                    let expected = usize::try_from(actual_end - actual_start + 1)
                        .map_err(|_| invalid_response())?;
                    // ureq removes Content-Length after decompression. Requiring
                    // an exact length also rejects transformed byte ranges.
                    if response.body().content_length() != Some(expected as u64) {
                        return Err(invalid_response());
                    }
                    self.length = Some(length);
                    (expected.saturating_add(1), Some(expected), false)
                }
                200 if start == 0 => {
                    self.accepts_ranges = false;
                    self.length = response.body().content_length();
                    (MAX_NON_RANGE_BYTES, None, true)
                }
                _ => return Err(invalid_response()),
            };
            let limit = limit.min(MAX_READ_BYTES - self.read_bytes);
            if expected.is_some_and(|size| size > limit) {
                return Err(io::Error::other("Web metadata byte budget exhausted"));
            }
            let mut body = response.body_mut().as_reader().take(limit as u64);
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                self.remaining()?;
                let count = body.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                self.read_bytes += count;
                bytes.extend_from_slice(&buffer[..count]);
            }
            if expected.is_some_and(|size| bytes.len() != size) {
                return Err(invalid_response());
            }
            if complete && self.length.is_none() && bytes.len() < limit {
                self.length = Some(bytes.len() as u64);
            }
            if complete && self.length.is_some_and(|size| size < bytes.len() as u64) {
                return Err(invalid_response());
            }
            self.url = current;
            self.validator = validator;
            self.blocks.insert(start, bytes);
            return Ok(());
        }
        Err(invalid_response())
    }
}

impl Read for HttpRangeReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.remaining()?;
        if output.is_empty() || self.length.is_some_and(|length| self.position >= length) {
            return Ok(0);
        }
        let contains_position = self
            .blocks
            .range(..=self.position)
            .next_back()
            .is_some_and(|(start, bytes)| self.position - start < bytes.len() as u64);
        if !contains_position {
            let aligned = self.position / BLOCK_BYTES as u64 * BLOCK_BYTES as u64;
            // A compliant server may return less than the requested range.
            // Continue after its short block instead of requesting it again.
            let start = if self.blocks.contains_key(&aligned) {
                self.position
            } else {
                aligned
            };
            self.fetch_block(start)?;
        }
        let (&start, bytes) = self
            .blocks
            .range(..=self.position)
            .next_back()
            .ok_or_else(invalid_response)?;
        let offset = usize::try_from(self.position - start).map_err(|_| invalid_response())?;
        let available = bytes.get(offset..).ok_or_else(invalid_response)?;
        let count = output.len().min(available.len());
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Web metadata prefix exhausted",
            ));
        }
        output[..count].copy_from_slice(&available[..count]);
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for HttpRangeReader<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.remaining()?;
        let next = match position {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
            SeekFrom::End(offset) => self
                .length
                .and_then(|length| length.checked_add_signed(offset)),
        }
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Web metadata seek is outside known media",
            )
        })?;
        self.position = next;
        Ok(next)
    }
}

/// Parses only a complete, internally consistent single byte range.
fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let (range, length) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    let length = length.parse::<u64>().ok()?;
    (start <= end && end < length).then_some((start, end, length))
}

fn invalid_response() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "Web server returned an invalid metadata response",
    )
}

/// Uses the same existing audio-tag library as Local, without additional crates.
#[cfg(feature = "local-metadata")]
fn read_tags(reader: &mut HttpRangeReader<'_>, result: &mut WebMediaMetadata) -> bool {
    use lofty::config::{GlobalOptions, ParseOptions, apply_global_options};
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::probe::Probe;
    use lofty::tag::Accessor;

    struct OptionsReset;
    impl Drop for OptionsReset {
        fn drop(&mut self) {
            apply_global_options(GlobalOptions::default());
        }
    }
    let _reset = OptionsReset;
    apply_global_options(
        GlobalOptions::new()
            .allocation_limit(2 * 1024 * 1024)
            .use_custom_resolvers(false)
            .preserve_format_specific_items(false),
    );
    // Preserve leading tags before optional duration reads can spend the remaining
    // request/time budget seeking to distant container indexes or final packets.
    let mut parsed = false;
    for read_properties in [false, true] {
        if reader.seek(SeekFrom::Start(0)).is_err() {
            break;
        }
        let options = ParseOptions::new()
            .read_properties(read_properties)
            .read_tags(!read_properties)
            .read_cover_art(!read_properties && cfg!(feature = "local-artwork"));
        let file_type = lofty::file::FileType::from_path(std::path::Path::new(reader.url.path()));
        let Ok(mut probe) = Probe::new(&mut *reader).options(options).guess_file_type() else {
            continue;
        };
        if probe.file_type().is_none()
            && let Some(file_type) = file_type
        {
            probe = probe.set_file_type(file_type);
        }
        let Ok(tagged) = probe.read() else {
            continue;
        };
        let properties = tagged.properties();
        if read_properties {
            result.duration = (!properties.duration().is_zero()).then_some(properties.duration());
            result.bitrate_kbps = properties
                .audio_bitrate()
                .or(properties.overall_bitrate())
                .filter(|value| *value != 0);
            result.sample_rate_hz = properties.sample_rate().filter(|value| *value != 0);
            result.channels = properties.channels().filter(|value| *value != 0);
        }
        let (container, codec) = container_and_codec(tagged.file_type());
        result.container = Some(container.to_owned());
        result.codec = codec.map(str::to_owned);
        if !read_properties && let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            result.title = tag.title().as_deref().and_then(clean_text);
            result.artist = tag.artist().as_deref().and_then(clean_text);
            result.album = tag.album().as_deref().and_then(clean_text);
            result.genre = tag.genre().as_deref().and_then(clean_text);
            result.comment = tag.comment().as_deref().and_then(clean_text);
            if result.comment.is_none() {
                result.comment = tag
                    .get_string(lofty::tag::ItemKey::Description)
                    .and_then(clean_text);
            }
        }
        #[cfg(feature = "local-artwork")]
        if !read_properties {
            use lofty::picture::PictureType;
            let picture = tagged
                .tags()
                .iter()
                .flat_map(lofty::tag::Tag::pictures)
                .filter(|picture| {
                    !picture.data().is_empty() && picture.data().len() <= 2 * 1024 * 1024
                })
                .min_by_key(|picture| match picture.pic_type() {
                    PictureType::CoverFront => 0,
                    PictureType::Other => 1,
                    _ => 2,
                });
            result.artwork = picture.map(|picture| WebEmbeddedArtwork {
                bytes: picture.data().to_vec(),
                mime_type: picture.mime_type().map_or_else(
                    || "application/octet-stream".to_owned(),
                    ToString::to_string,
                ),
            });
        }
        parsed = true;
    }
    parsed
}

/// Keeps network-provided tags bounded and unable to emit terminal controls.
#[cfg(feature = "local-metadata")]
fn clean_text(value: &str) -> Option<String> {
    let mut text = String::new();
    for character in value.chars().filter(|character| {
        (!character.is_control() || matches!(*character, '\n' | '\t'))
            && !matches!(*character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }) {
        if text.len() + character.len_utf8() > 16 * 1024 {
            break;
        }
        text.push(character);
    }
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Avoids assigning an audio codec to containers which can hold several codecs.
#[cfg(feature = "local-metadata")]
fn container_and_codec(file_type: lofty::file::FileType) -> (&'static str, Option<&'static str>) {
    use lofty::file::FileType;
    match file_type {
        FileType::Aac => ("ADTS", Some("AAC")),
        FileType::Aiff => ("AIFF", None),
        FileType::Ape => ("APE", Some("Monkey's Audio")),
        FileType::Flac => ("FLAC", Some("FLAC")),
        FileType::Mpeg => ("MPEG", Some("MP3")),
        FileType::Mp4 => ("MP4", None),
        FileType::Mpc => ("Musepack", Some("Musepack")),
        FileType::Opus => ("Ogg", Some("Opus")),
        FileType::Vorbis => ("Ogg", Some("Vorbis")),
        FileType::Speex => ("Ogg", Some("Speex")),
        FileType::Wav => ("WAV", None),
        FileType::WavPack => ("WavPack", Some("WavPack")),
        _ => ("Audio", None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    /// A stoppable mock server records bounded requests without hanging on failures.
    struct RangeServer {
        url: Url,
        stopped: Arc<AtomicBool>,
        requests: Arc<Mutex<Vec<String>>>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl RangeServer {
        /// Uses blocking accepted streams and dispatches only complete HTTP headers.
        ///
        /// Windows inherits the listener's nonblocking mode; restoring blocking I/O
        /// lets the existing read timeout bound a fragmented request on all platforms.
        fn new(handler: impl Fn(&str) -> Vec<u8> + Send + 'static) -> Self {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = Url::parse(&format!(
                "http://{}/track.opus",
                listener.local_addr().unwrap()
            ))
            .unwrap();
            let stopped = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let worker_stopped = stopped.clone();
            let worker_requests = requests.clone();
            let worker = std::thread::spawn(move || {
                while !worker_stopped.load(Ordering::Relaxed) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(stream) => stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(error) => panic!("mock accept: {error}"),
                    };
                    stream
                        .set_nonblocking(false)
                        .expect("mock request stream should be blocking");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut request = Vec::new();
                    let mut byte = [0_u8];
                    while !request.ends_with(b"\r\n\r\n") {
                        if stream.read_exact(&mut byte).is_err() {
                            break;
                        }
                        request.push(byte[0]);
                        assert!(request.len() <= 64 * 1024);
                    }
                    // EOF, reset, or timeout cannot turn partial headers into a request.
                    if !request.ends_with(b"\r\n\r\n") {
                        continue;
                    }
                    let request = String::from_utf8(request).unwrap();
                    worker_requests.lock().unwrap().push(request.clone());
                    let _ = stream.write_all(&handler(&request));
                }
            });
            Self {
                url,
                stopped,
                requests,
                worker: Some(worker),
            }
        }

        fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    impl Drop for RangeServer {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::Relaxed);
            self.worker.take().unwrap().join().unwrap();
        }
    }

    fn requested_range(request: &str) -> (u64, u64) {
        let range = request
            .lines()
            .find_map(|line| line.strip_prefix("range: bytes="))
            .unwrap();
        let (start, end) = range.split_once('-').unwrap();
        (start.parse().unwrap(), end.parse().unwrap())
    }

    /// A cancelled connection must not reach handlers that require complete Range headers.
    #[test]
    fn range_server_discards_cancelled_headers_and_serves_the_next_request() {
        let server = RangeServer::new(|_| response("200 OK", "Content-Length: 4\r\n", b"null"));
        let address = server.url.socket_addrs(|| None).unwrap()[0];
        for prefix in [
            b"".as_slice(),
            b"GET /track.opus HTTP/1.1\r\nHost: localhost\r\n",
        ] {
            let mut client = TcpStream::connect(address).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            client.write_all(prefix).unwrap();
            client.shutdown(Shutdown::Write).unwrap();
            let mut reply = Vec::new();
            client.read_to_end(&mut reply).unwrap();
            assert!(
                reply.is_empty(),
                "incomplete headers must not invoke the fixture handler"
            );
        }

        let metadata = WebMetadataClient::default().read(&server.url);
        assert_eq!(metadata.size_bytes, Some(4));
        assert_eq!(server.request_count(), 1);
        assert_eq!(
            requested_range(&server.requests.lock().unwrap()[0]),
            (0, 65_535)
        );
    }

    /// Accepted sockets must wait across header fragments even on nonblocking listeners.
    #[test]
    fn range_server_waits_for_header_fragments_before_responding() {
        let server = RangeServer::new(|_| response("200 OK", "Content-Length: 4\r\n", b"null"));
        let address = server.url.socket_addrs(|| None).unwrap()[0];
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        client
            .write_all(b"GET /track.opus HTTP/1.1\r\nHost: localhost\r\n")
            .unwrap();
        let mut byte = [0];
        // Signals may interrupt a Unix read without delivering a response or timing out.
        let result = loop {
            match client.read(&mut byte) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result,
            }
        };
        let error = result.expect_err("a partial request has no response");
        assert!(matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ));
        assert_eq!(server.request_count(), 0);

        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        client.write_all(b"range: bytes=0-3\r\n\r\n").unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        assert!(reply.ends_with("\r\n\r\nnull"));
        assert_eq!(server.request_count(), 1);
        assert_eq!(requested_range(&server.requests.lock().unwrap()[0]), (0, 3));
    }

    /// Encodes real ID3 frames followed by a small valid MPEG frame sequence.
    fn tagged_mp3() -> Vec<u8> {
        use lofty::config::WriteOptions;
        use lofty::picture::{MimeType, Picture, PictureType};
        use lofty::tag::{Accessor, Tag, TagExt, TagType};
        let mut tag = Tag::new(TagType::Id3v2);
        tag.set_title("An MP3 title".to_owned());
        tag.set_artist("An MP3 artist".to_owned());
        tag.set_genre("Audiobook".to_owned());
        tag.push_picture(
            Picture::unchecked(b"\x89PNG\r\n\x1a\ncover".to_vec())
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Png)
                .build(),
        );
        let mut bytes = Vec::new();
        tag.dump_to(&mut bytes, WriteOptions::default()).unwrap();
        for _ in 0..8 {
            bytes.extend_from_slice(&[0xff, 0xfb, 0x90, 0]);
            bytes.extend_from_slice(&[0; 413]);
        }
        bytes
    }

    fn vorbis_comments() -> Vec<u8> {
        let mut bytes = 0_u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&3_u32.to_le_bytes());
        for comment in [
            "TITLE=A tagged title",
            "ARTIST=A tagged artist",
            "ALBUM=A tagged album",
        ] {
            bytes.extend_from_slice(&u32::try_from(comment.len()).unwrap().to_le_bytes());
            bytes.extend_from_slice(comment.as_bytes());
        }
        bytes
    }

    /// Contains FLAC STREAMINFO and `VORBIS_COMMENT` blocks with one second of samples.
    fn tagged_flac() -> Vec<u8> {
        let mut bytes = b"fLaC\0\0\0\x22".to_vec();
        let mut info = [0_u8; 34];
        info[..4].copy_from_slice(&[0x10, 0, 0x10, 0]);
        let properties = (8_000_u64 << 44) | (15_u64 << 36) | 0x1f40;
        info[10..18].copy_from_slice(&properties.to_be_bytes());
        bytes.extend_from_slice(&info);
        let comments = vorbis_comments();
        bytes.push(0x84);
        bytes.extend_from_slice(&u32::try_from(comments.len()).unwrap().to_be_bytes()[1..]);
        bytes.extend_from_slice(&comments);
        bytes.extend_from_slice(&[0; 256]);
        bytes
    }

    /// Builds Ogg pages; the metadata parser does not decode their audio packet.
    fn tagged_opus() -> Vec<u8> {
        fn page(packet: &[u8], sequence: u32, flags: u8, granule: u64) -> Vec<u8> {
            assert!(packet.len() < 255);
            let mut bytes = b"OggS\0".to_vec();
            bytes.push(flags);
            bytes.extend_from_slice(&granule.to_le_bytes());
            bytes.extend_from_slice(&1_u32.to_le_bytes());
            bytes.extend_from_slice(&sequence.to_le_bytes());
            bytes.extend_from_slice(&0_u32.to_le_bytes());
            bytes.extend_from_slice(&[1, u8::try_from(packet.len()).unwrap()]);
            bytes.extend_from_slice(packet);
            bytes
        }
        let mut head = b"OpusHead\x01\x02\0\0".to_vec();
        head.extend_from_slice(&48_000_u32.to_le_bytes());
        head.extend_from_slice(&[0, 0, 0]);
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&vorbis_comments());
        let mut bytes = page(&head, 0, 2, 0);
        bytes.extend(page(&tags, 1, 0, 0));
        bytes.extend(page(&[0xf8, 0xff, 0xfe], 2, 4, 48_000));
        bytes
    }

    #[test]
    fn mp3_flac_and_opus_expose_their_embedded_tags_and_audio_properties() {
        for (extension, bytes, title, codec) in [
            ("mp3", tagged_mp3(), "An MP3 title", "MP3"),
            ("flac", tagged_flac(), "A tagged title", "FLAC"),
            ("opus", tagged_opus(), "A tagged title", "Opus"),
        ] {
            let (mut url, worker) = one_response(response(
                "200 OK",
                &format!("Content-Length: {}\r\n", bytes.len()),
                &bytes,
            ));
            url.set_path(&format!("/track.{extension}"));
            let metadata = WebMetadataClient::default().read(&url);
            assert_eq!(metadata.title.as_deref(), Some(title), "{extension}");
            assert_eq!(metadata.codec.as_deref(), Some(codec), "{extension}");
            assert!(metadata.duration.is_some(), "{extension}: {metadata:?}");
            assert!(metadata.sample_rate_hz.is_some(), "{extension}");
            assert!(metadata.channels.is_some(), "{extension}");
            worker.join().unwrap();
        }
    }

    #[test]
    #[cfg(feature = "local-artwork")]
    fn embedded_cover_is_returned_for_the_existing_thumbnail_validator() {
        let bytes = tagged_mp3();
        let (mut url, worker) = one_response(response(
            "200 OK",
            &format!("Content-Length: {}\r\n", bytes.len()),
            &bytes,
        ));
        url.set_path("/cover.mp3");
        let metadata = WebMetadataClient::default().read(&url);
        let artwork = metadata.artwork.expect("embedded artwork enabled");
        assert_eq!(artwork.mime_type, "image/png");
        assert_eq!(artwork.bytes, b"\x89PNG\r\n\x1a\ncover");
        worker.join().unwrap();
    }

    #[test]
    fn leading_tags_survive_an_unavailable_distant_duration_read() {
        let mut opus = tagged_opus();
        opus.resize(MAX_NON_RANGE_BYTES, 0);
        let (mut url, worker) =
            one_response(response("200 OK", "Content-Length: 536870912\r\n", &opus));
        url.set_path("/large.opus");
        let metadata = WebMetadataClient::default().read(&url);
        assert_eq!(metadata.title.as_deref(), Some("A tagged title"));
        assert_eq!(metadata.size_bytes, Some(512 * 1024 * 1024));
        assert!(
            metadata.duration.is_none(),
            "prefix duration must not pretend to describe the full resource"
        );
        worker.join().unwrap();
    }

    #[test]
    fn seeking_to_the_tail_of_a_large_file_reads_only_two_blocks_and_reuses_cache() {
        const LENGTH: u64 = 512 * 1024 * 1024;
        let server = RangeServer::new(|request| {
            let (start, end) = requested_range(request);
            let end = end.min(LENGTH - 1);
            let body = vec![42; usize::try_from(end - start + 1).unwrap()];
            response(
                "206 Partial Content",
                &format!(
                    "Content-Range: bytes {start}-{end}/{LENGTH}\r\nContent-Length: {}\r\n",
                    body.len()
                ),
                &body,
            )
        });
        let cancelled = AtomicBool::new(false);
        let mut reader = HttpRangeReader::new(server.url.clone(), REQUEST_TIMEOUT, &cancelled);
        let mut byte = [0_u8];
        reader.read_exact(&mut byte).unwrap();
        assert_eq!(reader.length, Some(LENGTH));
        reader.seek(SeekFrom::End(-1)).unwrap();
        reader.read_exact(&mut byte).unwrap();
        reader.seek(SeekFrom::Start(0)).unwrap();
        reader.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [42]);
        assert_eq!(reader.read_bytes, 2 * BLOCK_BYTES);
        assert_eq!(server.request_count(), 2);
    }

    #[test]
    fn servers_ignoring_range_never_trigger_a_complete_large_download() {
        let server = RangeServer::new(|_| {
            response(
                "200 OK",
                "Content-Length: 536870912\r\n",
                &vec![0_u8; MAX_NON_RANGE_BYTES],
            )
        });
        let cancelled = AtomicBool::new(false);
        let mut reader = HttpRangeReader::new(server.url.clone(), REQUEST_TIMEOUT, &cancelled);
        reader.fetch_block(0).unwrap();
        assert_eq!(reader.length, Some(512 * 1024 * 1024));
        assert_eq!(reader.read_bytes, MAX_NON_RANGE_BYTES);
        reader.seek(SeekFrom::End(-1)).unwrap();
        assert!(reader.read(&mut [0_u8]).is_err());
        assert_eq!(server.request_count(), 1);
    }

    #[test]
    fn shorter_valid_range_responses_continue_from_their_actual_end() {
        let server = RangeServer::new(|request| {
            let (start, _) = requested_range(request);
            let end = (start + 15).min(99);
            let body = vec![42; usize::try_from(end - start + 1).unwrap()];
            response(
                "206 Partial Content",
                &format!(
                    "Content-Range: bytes {start}-{end}/100\r\nContent-Length: {}\r\n",
                    body.len()
                ),
                &body,
            )
        });
        let cancelled = AtomicBool::new(false);
        let mut reader = HttpRangeReader::new(server.url.clone(), REQUEST_TIMEOUT, &cancelled);
        let mut bytes = [0_u8; 100];
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, [42; 100]);
        assert_eq!(reader.read_bytes, 100);
        assert_eq!(server.request_count(), 7);
    }

    #[test]
    fn request_byte_and_cancellation_budgets_stop_before_network_access() {
        let server = RangeServer::new(|_| panic!("exhausted budget must not send a request"));
        let cancelled = AtomicBool::new(false);
        let mut reader = HttpRangeReader::new(server.url.clone(), REQUEST_TIMEOUT, &cancelled);
        reader.requests = MAX_REQUESTS;
        assert!(reader.fetch_block(0).is_err());
        reader.requests = 0;
        reader.read_bytes = MAX_READ_BYTES;
        assert!(reader.fetch_block(0).is_err());
        reader.read_bytes = 0;
        cancelled.store(true, Ordering::Relaxed);
        assert!(reader.fetch_block(0).is_err());
        assert_eq!(server.request_count(), 0);
    }

    #[test]
    fn invalid_ranges_and_partial_body_sizes_are_rejected() {
        for (range, length, body) in [
            ("bytes 1-4/5", "4", b"null".as_slice()),
            ("bytes 0-3/*", "4", b"null".as_slice()),
            ("bytes 0-4/4", "4", b"null".as_slice()),
            ("bytes 0-3/4", "3", b"nul".as_slice()),
            ("bytes 0-3/4", "4", b"nul".as_slice()),
        ] {
            let (url, worker) = one_response(response(
                "206 Partial Content",
                &format!("Content-Range: {range}\r\nContent-Length: {length}\r\n"),
                body,
            ));
            let cancelled = AtomicBool::new(false);
            let mut reader = HttpRangeReader::new(url, REQUEST_TIMEOUT, &cancelled);
            assert!(reader.fetch_block(0).is_err(), "must reject {range}");
            assert!(reader.blocks.is_empty());
            worker.join().unwrap();
        }
    }

    #[test]
    fn redirects_are_validated_and_never_forward_cookies_or_authorization() {
        let server = RangeServer::new(|request| {
            if request.starts_with("GET /track.opus ") {
                response(
                    "302 Found",
                    "Location: /other.opus\r\nSet-Cookie: session=private\r\nContent-Length: 0\r\n",
                    b"",
                )
            } else {
                response("200 OK", "Content-Length: 4\r\n", b"null")
            }
        });
        let metadata = WebMetadataClient::default().read(&server.url);
        assert_eq!(metadata.size_bytes, Some(4));
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("cookie:") && !request.contains("authorization:"))
        );
        drop(requests);
        for location in [
            "file:///etc/passwd",
            "http://user:password@127.0.0.1/track.wav",
        ] {
            let (url, worker) = one_response(response(
                "302 Found",
                &format!("Location: {location}\r\nContent-Length: 0\r\n"),
                b"",
            ));
            assert!(WebMetadataClient::default().read(&url).size_bytes.is_none());
            worker.join().unwrap();
        }
    }

    #[test]
    fn changing_resource_validators_do_not_mix_byte_ranges() {
        let server = RangeServer::new(|request| {
            let (start, end) = requested_range(request);
            let validator = if start == 0 { "one" } else { "two" };
            response(
                "206 Partial Content",
                &format!(
                    "ETag: \"{validator}\"\r\nContent-Range: bytes {start}-{end}/524288\r\nContent-Length: {BLOCK_BYTES}\r\n"
                ),
                &vec![0; BLOCK_BYTES],
            )
        });
        let cancelled = AtomicBool::new(false);
        let mut reader = HttpRangeReader::new(server.url.clone(), REQUEST_TIMEOUT, &cancelled);
        reader.fetch_block(0).unwrap();
        assert!(reader.fetch_block(BLOCK_BYTES as u64).is_err());
        assert_eq!(reader.blocks.len(), 1);
        assert!(server.requests.lock().unwrap()[1].contains("if-range: \"one\""));
    }

    #[test]
    fn known_validator_must_not_disappear_between_ranges() {
        let server = RangeServer::new(|request| {
            let (start, end) = requested_range(request);
            let validator = if start == 0 { "ETag: \"one\"\r\n" } else { "" };
            response(
                "206 Partial Content",
                &format!(
                    "{validator}Content-Range: bytes {start}-{end}/524288\r\nContent-Length: {BLOCK_BYTES}\r\n"
                ),
                &vec![0; BLOCK_BYTES],
            )
        });
        let cancelled = AtomicBool::new(false);
        let mut reader = HttpRangeReader::new(server.url.clone(), REQUEST_TIMEOUT, &cancelled);
        reader.fetch_block(0).unwrap();
        assert!(reader.fetch_block(BLOCK_BYTES as u64).is_err());
        assert_eq!(reader.validator.as_deref(), Some("\"one\""));
    }

    #[test]
    fn weak_etags_are_not_sent_as_if_range_validators() {
        let server = RangeServer::new(|request| {
            let (start, end) = requested_range(request);
            response(
                "206 Partial Content",
                &format!(
                    "ETag: W/\"weak\"\r\nContent-Range: bytes {start}-{end}/524288\r\nContent-Length: {BLOCK_BYTES}\r\n"
                ),
                &vec![0; BLOCK_BYTES],
            )
        });
        let cancelled = AtomicBool::new(false);
        let mut reader = HttpRangeReader::new(server.url.clone(), REQUEST_TIMEOUT, &cancelled);
        reader.fetch_block(0).unwrap();
        reader.fetch_block(BLOCK_BYTES as u64).unwrap();
        assert!(reader.validator.is_none());
        assert!(!server.requests.lock().unwrap()[1].contains("if-range:"));
    }

    #[test]
    fn unsupported_encodings_and_changed_lengths_are_rejected() {
        let (url, worker) = one_response(response(
            "200 OK",
            "Content-Encoding: unsupported\r\nContent-Length: 4\r\n",
            b"null",
        ));
        assert!(WebMetadataClient::default().read(&url).size_bytes.is_none());
        worker.join().unwrap();

        let server = RangeServer::new(|request| {
            let (start, end) = requested_range(request);
            let length = if start == 0 { 524_288 } else { 524_289 };
            response(
                "206 Partial Content",
                &format!(
                    "Content-Range: bytes {start}-{end}/{length}\r\nContent-Length: {BLOCK_BYTES}\r\n"
                ),
                &vec![0; BLOCK_BYTES],
            )
        });
        let cancelled = AtomicBool::new(false);
        let mut reader = HttpRangeReader::new(server.url.clone(), REQUEST_TIMEOUT, &cancelled);
        reader.fetch_block(0).unwrap();
        assert!(reader.fetch_block(BLOCK_BYTES as u64).is_err());
        assert_eq!(reader.length, Some(524_288));
    }

    #[test]
    fn cached_reads_and_seeks_also_honor_cancellation_and_deadline() {
        let cancelled = AtomicBool::new(false);
        let mut reader = HttpRangeReader::new(
            Url::parse("http://127.0.0.1:1/a.mp3").unwrap(),
            Duration::ZERO,
            &cancelled,
        );
        reader.blocks.insert(0, vec![1, 2, 3]);
        reader.length = Some(3);
        assert!(reader.read(&mut [0]).is_err());
        assert!(reader.seek(SeekFrom::Start(0)).is_err());
        reader.deadline = Instant::now() + REQUEST_TIMEOUT;
        cancelled.store(true, Ordering::Relaxed);
        assert!(reader.read(&mut [0]).is_err());
        assert!(reader.seek(SeekFrom::Start(0)).is_err());
    }

    #[test]
    fn slow_metadata_response_obeys_the_shared_deadline() {
        let server = RangeServer::new(|_| {
            std::thread::sleep(Duration::from_millis(100));
            response("200 OK", "Content-Length: 4\r\n", b"null")
        });
        let start = Instant::now();
        let result = WebMetadataClient {
            timeout: Duration::from_millis(20),
        }
        .read(&server.url);
        assert!(result.title.is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn unknown_length_prefix_is_not_mistaken_for_complete_file_size() {
        let (url, worker) = one_response(response("200 OK", "", &vec![0; MAX_NON_RANGE_BYTES]));
        let metadata = WebMetadataClient::default().read(&url);
        assert!(metadata.size_bytes.is_none());
        assert_eq!(metadata.probe_prefix.len(), MAX_PROBE_PREFIX_BYTES);
        worker.join().unwrap();
    }

    #[test]
    #[cfg(feature = "local-metadata")]
    fn empty_control_only_and_oversized_tags_are_safe_fallbacks() {
        assert_eq!(clean_text("\0\u{1b}\u{202e} \t\n"), None);
        assert_eq!(clean_text("A\u{1b} title"), Some("A title".to_owned()));
        assert!(clean_text(&"é".repeat(32_000)).unwrap().len() <= 16 * 1024);
    }

    /// Builds a complete PCM WAV with conventional RIFF INFO tags.
    fn tagged_wav() -> Vec<u8> {
        fn chunk(kind: [u8; 4], bytes: &[u8]) -> Vec<u8> {
            let mut result = kind.to_vec();
            result.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_le_bytes());
            result.extend_from_slice(bytes);
            if !bytes.len().is_multiple_of(2) {
                result.push(0);
            }
            result
        }
        let mut contents = b"WAVE".to_vec();
        let mut format = Vec::new();
        format.extend_from_slice(&1_u16.to_le_bytes());
        format.extend_from_slice(&1_u16.to_le_bytes());
        format.extend_from_slice(&8_000_u32.to_le_bytes());
        format.extend_from_slice(&16_000_u32.to_le_bytes());
        format.extend_from_slice(&2_u16.to_le_bytes());
        format.extend_from_slice(&16_u16.to_le_bytes());
        contents.extend(chunk(*b"fmt ", &format));
        let mut info = b"INFO".to_vec();
        info.extend(chunk(*b"INAM", b"A Web title\0"));
        info.extend(chunk(*b"IART", b"An artist\0"));
        info.extend(chunk(*b"IPRD", b"An album\0"));
        info.extend(chunk(*b"ICMT", b"A description\0"));
        contents.extend(chunk(*b"LIST", &info));
        contents.extend(chunk(*b"data", &[0; 16_000]));
        chunk(*b"RIFF", &contents)
    }

    /// Serves one local mock response without contacting any public service.
    fn one_response(response: Vec<u8>) -> (Url, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let url = Url::parse(&format!(
            "http://{}/track.wav",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0_u8];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let _ = stream.write_all(&response);
            String::from_utf8(request).unwrap()
        });
        (url, worker)
    }

    fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut response =
            format!("HTTP/1.1 {status}\r\nConnection: close\r\n{headers}\r\n").into_bytes();
        response.extend_from_slice(body);
        response
    }

    #[test]
    #[cfg(feature = "local-metadata")]
    fn selected_web_audio_reads_tags_and_properties_from_a_range_response() {
        let wav = tagged_wav();
        let (url, worker) = one_response(response(
            "206 Partial Content",
            &format!(
                "Content-Range: bytes 0-{}/{}\r\nContent-Length: {}\r\n",
                wav.len() - 1,
                wav.len(),
                wav.len()
            ),
            &wav,
        ));
        let metadata = WebMetadataClient::default().read(&url);
        assert_eq!(metadata.title.as_deref(), Some("A Web title"));
        assert_eq!(metadata.artist.as_deref(), Some("An artist"));
        assert_eq!(metadata.album.as_deref(), Some("An album"));
        assert_eq!(metadata.comment.as_deref(), Some("A description"));
        assert_eq!(metadata.duration, Some(Duration::from_secs(1)));
        assert_eq!(metadata.sample_rate_hz, Some(8_000));
        assert_eq!(metadata.channels, Some(1));
        assert_eq!(metadata.size_bytes, Some(wav.len() as u64));
        assert!(worker.join().unwrap().contains("range: bytes=0-65535"));
    }

    #[test]
    #[cfg(feature = "local-metadata")]
    fn small_servers_ignoring_range_still_supply_metadata() {
        let wav = tagged_wav();
        let (url, worker) = one_response(response(
            "200 OK",
            &format!("Content-Length: {}\r\n", wav.len()),
            &wav,
        ));
        let metadata = WebMetadataClient::default().read(&url);
        assert_eq!(metadata.title.as_deref(), Some("A Web title"));
        assert_eq!(metadata.duration, Some(Duration::from_secs(1)));
        worker.join().unwrap();
    }

    #[test]
    fn malformed_metadata_preserves_known_size_without_inventing_tags() {
        let (url, worker) = one_response(response("200 OK", "Content-Length: 4\r\n", b"null"));
        let metadata = WebMetadataClient::default().read(&url);
        assert_eq!(metadata.size_bytes, Some(4));
        assert!(metadata.title.is_none());
        assert!(metadata.duration.is_none());
        worker.join().unwrap();
    }

    #[test]
    fn cancelled_or_credential_bearing_urls_do_not_make_requests() {
        let cancelled = AtomicBool::new(true);
        let metadata = WebMetadataClient::default()
            .read_cancellable(&Url::parse("http://127.0.0.1:1/a.wav").unwrap(), &cancelled);
        assert!(metadata.size_bytes.is_none());
        assert!(
            WebMetadataClient::default()
                .read(&Url::parse("https://user:secret@example.org/a.wav").unwrap())
                .size_bytes
                .is_none()
        );
    }
}
