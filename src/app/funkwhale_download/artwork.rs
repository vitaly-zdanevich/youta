//! Optional, byte-preserving artwork sidecars for explicit Funkwhale downloads.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MAX_BYTES: usize = 8 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(15);

/// A private, validated image removed after its optional no-replacement publication.
pub(super) struct PreparedArtwork {
    /// Private encoded bytes owned until sidecar publication completes.
    pub path: PathBuf,
    /// A fixed extension proven by the image signature, never URL text.
    pub extension: &'static str,
}

impl Drop for PreparedArtwork {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Fetches one advertised image with a shared deadline and at most five redirects.
///
/// Errors are deliberately URL-free because artwork locations can contain signed
/// query parameters. Failure is optional and must never fail the audio download.
pub(super) fn prepare(
    url: &url::Url,
    directory: &Path,
    cancellation: &AtomicBool,
) -> Result<PreparedArtwork, String> {
    let started = Instant::now();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(DEADLINE))
        .build()
        .into();
    let mut current = url.clone();
    for redirects in 0..=5 {
        check(cancellation, started)?;
        super::validate_audio_url(&current)
            .map_err(|_| "The pod advertised an unsafe artwork URL".to_owned())?;
        let mut response = agent
            .get(current.as_str())
            .header("Accept", "image/png, image/jpeg, image/webp, image/avif")
            .header("Accept-Encoding", "identity")
            .config()
            .timeout_global(Some(DEADLINE.saturating_sub(started.elapsed())))
            .build()
            .call()
            .map_err(|_| "Funkwhale artwork could not be downloaded".to_owned())?;
        check(cancellation, started)?;
        if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            if redirects == 5 {
                return Err("Funkwhale artwork redirected too many times".to_owned());
            }
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .filter(|value| value.len() <= 16_384)
                .ok_or("Funkwhale artwork has an invalid redirect")?;
            current = current
                .join(location)
                .map_err(|_| "Funkwhale artwork has an invalid redirect")?;
            continue;
        }
        if response.status().as_u16() != 200 {
            return Err(format!(
                "Funkwhale artwork returned HTTP status {}",
                response.status().as_u16()
            ));
        }
        let declared = response.body().content_length();
        let bytes = read_image(
            &mut response.body_mut().as_reader(),
            declared,
            cancellation,
            started,
        )?;
        let extension =
            image_extension(&bytes).ok_or("Funkwhale artwork is not a supported image")?;
        check(cancellation, started)?;
        let mut file = tempfile::Builder::new()
            .prefix("artwork-")
            .tempfile_in(directory)
            .map_err(|_| "Cannot create private artwork staging")?;
        crate::private_files::set_private_file_permissions(file.path())
            .map_err(|_| "Cannot protect artwork staging")?;
        file.write_all(&bytes)
            .and_then(|()| file.as_file().sync_all())
            .map_err(|_| "Cannot write downloaded artwork")?;
        check(cancellation, started)?;
        let (_, path) = file
            .keep()
            .map_err(|_| "Cannot retain downloaded artwork")?;
        return Ok(PreparedArtwork { path, extension });
    }
    Err("Funkwhale artwork redirected too many times".to_owned())
}

/// Links a complete image beside its audio file, never replacing existing artwork.
pub(super) fn publish(art: PreparedArtwork, audio_path: &Path) -> bool {
    if !matches!(art.extension, "png" | "jpg" | "webp" | "avif")
        || !std::fs::symlink_metadata(&art.path).is_ok_and(|metadata| {
            metadata.file_type().is_file()
                && metadata.len() > 0
                && metadata.len() <= MAX_BYTES as u64
        })
    {
        return false;
    }
    std::fs::hard_link(&art.path, audio_path.with_extension(art.extension)).is_ok()
}

/// Reads encoded bytes without trusting Content-Length or allocating an unbounded body.
fn read_image(
    reader: &mut impl Read,
    declared: Option<u64>,
    cancellation: &AtomicBool,
    started: Instant,
) -> Result<Vec<u8>, String> {
    if declared.is_some_and(|length| length == 0 || length > MAX_BYTES as u64) {
        return Err("Funkwhale artwork is empty or exceeds the 8 MiB limit".to_owned());
    }
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        check(cancellation, started)?;
        let count = reader
            .read(&mut buffer)
            .map_err(|_| "Funkwhale artwork transfer was interrupted")?;
        check(cancellation, started)?;
        if count == 0 {
            break;
        }
        if count > MAX_BYTES.saturating_sub(bytes.len()) {
            return Err("Funkwhale artwork exceeds the 8 MiB limit".to_owned());
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    if bytes.is_empty() || declared.is_some_and(|length| length != bytes.len() as u64) {
        return Err("Funkwhale artwork is empty or truncated".to_owned());
    }
    Ok(bytes)
}

/// Reuses the shared sniffer when built, extending its formats with an AVIF ftyp brand.
fn image_extension(bytes: &[u8]) -> Option<&'static str> {
    #[cfg(any(feature = "remote-artwork", feature = "local-artwork"))]
    if let Some(format) = crate::artwork::ArtworkFormat::sniff(bytes) {
        return Some(match format {
            crate::artwork::ArtworkFormat::Png => "png",
            crate::artwork::ArtworkFormat::Jpeg => "jpg",
            crate::artwork::ArtworkFormat::WebP => "webp",
        });
    }
    #[cfg(not(any(feature = "remote-artwork", feature = "local-artwork")))]
    {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Some("png");
        }
        if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            return Some("jpg");
        }
        if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            return Some("webp");
        }
    }
    let size = u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
    if bytes.get(4..8) != Some(b"ftyp") || size < 16 || size > bytes.len() || size % 4 != 0 {
        return None;
    }
    let brand = |value: &[u8]| matches!(value, b"avif" | b"avis");
    (brand(bytes.get(8..12)?) || bytes[16..size].chunks_exact(4).any(brand)).then_some("avif")
}

/// Cancellation and the absolute image deadline apply across all redirects and file writes.
fn check(cancellation: &AtomicBool, started: Instant) -> Result<(), String> {
    if cancellation.load(Ordering::Acquire) {
        return Err("Funkwhale artwork download cancelled".to_owned());
    }
    if started.elapsed() >= DEADLINE {
        return Err("Funkwhale artwork download timed out".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// URL suffixes and claimed MIME types cannot turn HTML or arbitrary bytes into images.
    #[test]
    fn signatures_accept_supported_images_and_reject_pages_and_other_iso_media() {
        for (bytes, extension) in [
            (b"\x89PNG\r\n\x1a\nfixture".as_slice(), "png"),
            (b"\xff\xd8\xfffixture", "jpg"),
            (b"RIFF\0\0\0\0WEBPfixture", "webp"),
            (b"\0\0\0\x14ftypavif\0\0\0\0mif1", "avif"),
            (b"\0\0\0\x14ftypmif1\0\0\0\0avif", "avif"),
        ] {
            assert_eq!(image_extension(bytes), Some(extension));
        }
        for bytes in [
            b"<html>challenge</html>".as_slice(),
            b"\0\0\0\x14ftypmp42\0\0\0\0isom",
            b"\0\0\0\xffftypavif\0\0\0\0mif1",
            b"RIFF",
            b"",
        ] {
            assert_eq!(image_extension(bytes), None);
        }
    }

    /// Full image bytes survive publication while an existing user sidecar remains unchanged.
    #[test]
    fn publication_preserves_bytes_never_overwrites_and_removes_private_staging() {
        let directory = tempfile::tempdir().unwrap();
        let audio = directory.path().join("Selected track [27].mp3");
        std::fs::write(&audio, b"audio").unwrap();
        let private = directory.path().join("private-image");
        let bytes = b"\x89PNG\r\n\x1a\ncomplete unchanged artwork";
        std::fs::write(&private, bytes).unwrap();
        assert!(publish(
            PreparedArtwork {
                path: private.clone(),
                extension: "png"
            },
            &audio,
        ));
        assert!(!private.exists());
        assert_eq!(std::fs::read(audio.with_extension("png")).unwrap(), bytes);
        std::fs::write(&private, b"do not replace existing image").unwrap();
        assert!(!publish(
            PreparedArtwork {
                path: private.clone(),
                extension: "png"
            },
            &audio,
        ));
        assert!(!private.exists());
        assert_eq!(std::fs::read(audio.with_extension("png")).unwrap(), bytes);
        assert_eq!(std::fs::read(audio).unwrap(), b"audio");
    }

    /// Both chunked and declared-length bodies are capped, and cancellation cannot publish bytes.
    #[test]
    fn body_limits_truncation_cancellation_and_deadline_are_enforced() {
        let cancelled = AtomicBool::new(false);
        for declared in [Some(0), Some(MAX_BYTES as u64 + 1), Some(4)] {
            assert!(read_image(&mut &b"abc"[..], declared, &cancelled, Instant::now()).is_err());
        }
        assert!(
            read_image(
                &mut std::io::repeat(0).take(MAX_BYTES as u64 + 1),
                None,
                &cancelled,
                Instant::now(),
            )
            .is_err()
        );
        assert!(
            read_image(
                &mut &b"abc"[..],
                None,
                &cancelled,
                Instant::now() - DEADLINE
            )
            .is_err()
        );
        cancelled.store(true, Ordering::Release);
        assert!(read_image(&mut &b"abc"[..], None, &cancelled, Instant::now()).is_err());
    }

    /// Invalid artwork locations fail before networking and never disclose signed locator text.
    #[test]
    fn unsafe_artwork_urls_are_rejected_without_disclosure_or_files() {
        let directory = tempfile::tempdir().unwrap();
        for source in [
            "file:///private/secret.png",
            "https://user:secret@pod.example/image?signature=private",
            "https://pod.example/image#private",
        ] {
            let error = prepare(
                &url::Url::parse(source).unwrap(),
                directory.path(),
                &AtomicBool::new(false),
            )
            .err()
            .expect("unsafe image location must fail");
            assert!(!error.contains(source));
            assert!(!error.contains("private"));
            assert!(!error.contains("secret"));
        }
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    /// A redirected, extensionless attachment is stored privately and keeps its encoded bytes.
    #[test]
    fn explicit_artwork_download_follows_a_bounded_redirect_and_sniffs_the_body() {
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let source =
            url::Url::parse(&format!("http://{}/cover", listener.local_addr().unwrap())).unwrap();
        let bytes = b"\x89PNG\r\n\x1a\noriginal image bytes";
        let server = thread::spawn(move || {
            let started = Instant::now();
            let mut paths = Vec::new();
            while paths.len() < 2 && started.elapsed() < Duration::from_secs(3) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut input = [0_u8; 4096];
                let count = stream.read(&mut input).unwrap();
                let request = String::from_utf8_lossy(&input[..count]);
                let path = request.split_whitespace().nth(1).unwrap().to_owned();
                if paths.is_empty() {
                    assert_eq!(path, "/cover");
                    stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: /attachment\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    assert_eq!(path, "/attachment");
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).unwrap();
                    stream.write_all(bytes).unwrap();
                }
                paths.push(path);
            }
            paths
        });
        let directory = tempfile::tempdir().unwrap();
        let result = prepare(&source, directory.path(), &AtomicBool::new(false));
        let paths = server.join().unwrap();
        let artwork = result.unwrap();
        assert_eq!(paths, ["/cover", "/attachment"]);
        assert_eq!(artwork.extension, "png");
        assert_eq!(std::fs::read(&artwork.path).unwrap(), bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&artwork.path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let path = artwork.path.clone();
        drop(artwork);
        assert!(!path.exists());
    }
}
