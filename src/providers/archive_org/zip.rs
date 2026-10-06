//! Lazy, bounded browsing of public Internet Archive ZIP members.

use super::*;

/// Rejects unusually large root ZIP menus independently of member-list bounds.
const MAX_ARCHIVES: usize = 1024;
/// A canonical directory may move once more between Archive storage nodes.
const MAX_LISTING_REDIRECTS: usize = 2;

impl ArchiveOrgClient {
    /// Lists playable members of one ZIP already present in a public root inventory.
    ///
    /// Archive.org performs member extraction remotely. No ZIP bytes are downloaded
    /// locally, and each member retains its exact relative path and individual size.
    ///
    /// # Errors
    ///
    /// Reports forged or nested archive selections, inaccessible listings, malformed
    /// HTML, and exceeded response, token, row, path, or playable-track bounds.
    pub fn zip_details(
        &self,
        parent: &ArchiveOrgItemDetails,
        archive: &ArchiveOrgZip,
    ) -> Result<ArchiveOrgItemDetails, ProviderError> {
        if parent.archive_filename.is_some()
            || !valid_identifier(&parent.item.identifier)
            || !valid_filename(&archive.filename)
            || !zip_filename(&archive.filename)
            || !parent.archives.contains(archive)
            || parent.item.webpage_url != archive_url(&["details", &parent.item.identifier])?
        {
            return Err(ProviderError::InvalidRequest(
                "ZIP must belong to the public root item inventory".into(),
            ));
        }
        let mut segments = vec!["download", &parent.item.identifier];
        segments.extend(archive.filename.split('/'));
        segments.push("");
        let url = archive_url(&segments)?;
        let bytes = self.transport.fetch_zip_listing(&url, MAX_HTML_BYTES)?;
        if bytes.len() > MAX_HTML_BYTES {
            return Err(ProviderError::ResponseTooLarge {
                limit: MAX_HTML_BYTES,
            });
        }
        let tracks = parse_listing(&url, &archive.filename, &bytes)?;
        Ok(ArchiveOrgItemDetails {
            item: parent.item.clone(),
            tracks,
            // Root inventory counts describe the ZIP container, not these members.
            file_counts: None,
            archives: Vec::new(),
            archive_filename: Some(archive.filename.clone()),
            comments: parent.comments.clone(),
        })
    }
}

/// Keeps only unambiguous, unrestricted ZIP records without opening their contents.
pub(super) fn normalize_archives(files: &[Value]) -> Result<Vec<ArchiveOrgZip>, ProviderError> {
    let mut by_name = BTreeMap::<&str, &Value>::new();
    let mut duplicate_names = HashSet::new();
    for file in files {
        let Some(filename) = file["name"]
            .as_str()
            .filter(|name| valid_filename(name) && zip_filename(name))
        else {
            continue;
        };
        if by_name.insert(filename, file).is_some() {
            duplicate_names.insert(filename);
        }
    }
    let mut archives = Vec::new();
    for (filename, file) in by_name {
        let format = file["format"].as_str().unwrap_or_default();
        if duplicate_names.contains(filename)
            || restricted(file)
            || !(format.is_empty() || format.eq_ignore_ascii_case("ZIP"))
        {
            continue;
        }
        if archives.len() >= MAX_ARCHIVES {
            return Err(invalid_response("too many item ZIP archives"));
        }
        archives.push(ArchiveOrgZip {
            filename: filename.into(),
            size_bytes: number(&file["size"]),
        });
    }
    archives.sort_by(|left, right| natural_cmp(&left.filename, &right.filename));
    Ok(archives)
}

/// Archive directory routing requires the uploaded file's ZIP extension.
fn zip_filename(filename: &str) -> bool {
    filename
        .rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("zip"))
}

/// Fetches a canonical listing while confining storage redirects to the same ZIP.
pub(super) fn fetch_listing(
    agent: &ureq::Agent,
    url: &Url,
    max_bytes: usize,
) -> Result<Vec<u8>, ProviderError> {
    if listing_identity(url).is_none() {
        return Err(ProviderError::InvalidRequest(
            "invalid canonical Archive.org ZIP listing URL".into(),
        ));
    }
    let mut current = url.clone();
    for redirects in 0..=MAX_LISTING_REDIRECTS {
        let response = agent
            .get(current.as_str())
            .header("Accept", "text/html")
            .call()
            .map_err(|error| match error {
                ureq::Error::StatusCode(code) => ProviderError::HttpStatus(code),
                other => ProviderError::Transport(other.to_string()),
            })?;
        if response.status().is_redirection() {
            if redirects == MAX_LISTING_REDIRECTS {
                return Err(invalid_response("too many ZIP listing redirects"));
            }
            current = response
                .headers()
                .get("Location")
                .and_then(|value| value.to_str().ok())
                .and_then(|target| listing_redirect(url, target))
                .ok_or_else(|| invalid_response("unsafe ZIP listing redirect"))?;
            continue;
        }
        return read_bounded_response(response, max_bytes);
    }
    Err(invalid_response("ZIP listing redirect failed"))
}

/// Returns the exact identifier and ZIP name only for reconstructed canonical URLs.
fn listing_identity(url: &Url) -> Option<(String, String)> {
    if url.scheme() != "https"
        || url.host_str() != Some("archive.org")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let path = url.path().strip_prefix("/download/")?.strip_suffix('/')?;
    let (identifier, path) = path.split_once('/')?;
    if !valid_identifier(identifier) {
        return None;
    }
    let filename = decode_path(path)?;
    if !valid_filename(&filename) || !zip_filename(&filename) {
        return None;
    }
    let mut segments = vec!["download", identifier];
    segments.extend(filename.split('/'));
    segments.push("");
    (archive_url(&segments).ok()?.as_str() == url.as_str()).then(|| (identifier.into(), filename))
}

/// Allows Archive's storage endpoint only for the exact requested ZIP, without files.
fn listing_redirect(source: &Url, location: &str) -> Option<Url> {
    if location.len() > 8192 || location.chars().any(char::is_control) {
        return None;
    }
    let (identifier, filename) = listing_identity(source)?;
    let target = Url::parse(location).ok()?;
    if target.scheme() != "https"
        || target.port().is_some()
        || !target.username().is_empty()
        || target.password().is_some()
        || target.fragment().is_some()
        || target.path() != "/view_archive.php"
        || !storage_host(target.host_str()?)
    {
        return None;
    }
    let mut pairs = target.query_pairs();
    let (key, archive) = pairs.next()?;
    if key != "archive" || pairs.next().is_some() {
        return None;
    }
    let path = archive.strip_prefix('/')?;
    let (disk, path) = path.split_once('/')?;
    if disk.is_empty()
        || disk.len() > 4
        || !disk.bytes().all(|byte| byte.is_ascii_digit())
        || path != format!("items/{identifier}/{filename}")
    {
        return None;
    }
    Some(target)
}

/// Archive's storage hostnames have an ia/dn numeric node and a country suffix.
fn storage_host(host: &str) -> bool {
    let Some(prefix) = host.strip_suffix(".archive.org") else {
        return false;
    };
    let Some((node, region)) = prefix.split_once('.') else {
        return false;
    };
    let Some(number) = node.strip_prefix("ia").or_else(|| node.strip_prefix("dn")) else {
        return false;
    };
    !number.is_empty()
        && number.len() <= 12
        && number.bytes().all(|byte| byte.is_ascii_digit())
        && region.len() == 2
        && region.bytes().all(|byte| byte.is_ascii_lowercase())
}

/// Strictly decodes one path layer while preserving literal plus and percent bytes.
fn decode_path(value: &str) -> Option<String> {
    if value.len() > MAX_FILENAME_BYTES * 3 {
        return None;
    }
    let mut result = Vec::with_capacity(value.len());
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = char::from(bytes.next()?).to_digit(16)?;
            let low = char::from(bytes.next()?).to_digit(16)?;
            result.push(u8::try_from(high * 16 + low).ok()?);
        } else {
            result.push(byte);
        }
    }
    String::from_utf8(result).ok()
}

/// An HTML row captures URL identity and its own uncompressed byte count only.
#[derive(Default)]
struct ListingRow {
    cell: usize,
    member: Option<String>,
    size_cell: bool,
    size: String,
}

/// Distinguishes a valid empty listing from missing, nested, or truncated tables.
#[derive(Default, Eq, PartialEq)]
enum TableState {
    #[default]
    Missing,
    Reading,
    Complete,
}

/// Bounds and captures only the archive member table within the full page.
#[derive(Default)]
struct ListingTable {
    state: TableState,
    rows: usize,
    row: Option<ListingRow>,
    members: BTreeMap<String, Option<u64>>,
}

impl ListingTable {
    /// Processes start tags while honoring the service's implicit cell/row endings.
    fn start_tag(
        &mut self,
        tag: &html5gum::StartTag<()>,
        base: &Url,
        archive_filename: &str,
    ) -> Result<(), ProviderError> {
        let attribute = |key: &[u8]| {
            tag.attributes
                .get(key)
                .and_then(|value| std::str::from_utf8(value.value.as_ref()).ok())
        };
        match tag.name.as_slice() {
            b"table"
                if attribute(b"class")
                    .unwrap_or_default()
                    .split_ascii_whitespace()
                    .any(|class| class == "archext") =>
            {
                if self.state != TableState::Missing {
                    return Err(invalid_response("ambiguous ZIP listing tables"));
                }
                self.state = TableState::Reading;
            }
            b"table" if self.state == TableState::Reading => {
                return Err(invalid_response("nested ZIP listing table"));
            }
            b"tr" if self.state == TableState::Reading => {
                finish_row(&mut self.row, &mut self.members)?;
                self.rows += 1;
                if self.rows > MAX_FILES {
                    return Err(invalid_response("too many ZIP listing rows"));
                }
                self.row = Some(ListingRow::default());
            }
            b"td" | b"th" if self.state == TableState::Reading => {
                if let Some(row) = &mut self.row {
                    row.cell += 1;
                    row.size_cell = attribute(b"id") == Some("size");
                }
            }
            b"a" if self.state == TableState::Reading => {
                if let Some(row) = &mut self.row
                    && row.cell == 1
                    && row.member.is_none()
                {
                    row.member = attribute(b"href")
                        .and_then(|href| member_path(base, archive_filename, href));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Parses Archive's archext table, including its intentionally unclosed td elements.
fn parse_listing(
    base: &Url,
    archive_filename: &str,
    bytes: &[u8],
) -> Result<Vec<ArchiveOrgTrack>, ProviderError> {
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    let mut table = ListingTable::default();
    let mut hidden = Vec::<Vec<u8>>::new();
    for (index, token) in Tokenizer::new_with_emitter(bytes, emitter).enumerate() {
        if index >= MAX_HTML_TOKENS {
            return Err(invalid_response("too many ZIP listing HTML tokens"));
        }
        let Ok(token) = token;
        match token {
            Token::StartTag(tag) if hidden_tag(tag.name.as_slice()) => {
                if hidden.len() >= 64 {
                    return Err(invalid_response("ZIP listing HTML is too deeply nested"));
                }
                hidden.push(tag.name.to_vec());
            }
            Token::EndTag(tag)
                if hidden
                    .last()
                    .is_some_and(|name| name.as_slice() == tag.name.as_slice()) =>
            {
                hidden.pop();
            }
            Token::StartTag(tag) if hidden.is_empty() => {
                table.start_tag(&tag, base, archive_filename)?;
            }
            Token::EndTag(tag) if hidden.is_empty() && table.state == TableState::Reading => {
                match tag.name.as_slice() {
                    b"td" | b"th" => {
                        if let Some(row) = &mut table.row {
                            row.size_cell = false;
                        }
                    }
                    b"tr" => finish_row(&mut table.row, &mut table.members)?,
                    b"table" => {
                        finish_row(&mut table.row, &mut table.members)?;
                        table.state = TableState::Complete;
                    }
                    _ => {}
                }
            }
            Token::String(text) if hidden.is_empty() && table.state == TableState::Reading => {
                if let Some(row) = &mut table.row
                    && row.size_cell
                {
                    if row.size.len().saturating_add(text.value.len()) > 32 {
                        return Err(invalid_response("oversized ZIP member size field"));
                    }
                    row.size
                        .push_str(&String::from_utf8_lossy(text.value.as_ref()));
                }
            }
            _ => {}
        }
    }
    if table.state != TableState::Complete {
        return Err(invalid_response("ZIP listing is unavailable or incomplete"));
    }
    member_tracks(base, archive_filename, table.members)
}

/// Creates one existing audio variant per member without guessing derivative families.
fn member_tracks(
    base: &Url,
    archive_filename: &str,
    members: BTreeMap<String, Option<u64>>,
) -> Result<Vec<ArchiveOrgTrack>, ProviderError> {
    let (identifier, _) =
        listing_identity(base).ok_or_else(|| invalid_response("invalid ZIP listing identity"))?;
    let mut tracks = Vec::with_capacity(members.len());
    for (member, size_bytes) in members {
        let filename = format!("{archive_filename}/{member}");
        let mut segments = vec!["download", &identifier];
        segments.extend(filename.split('/'));
        let download_url = archive_url(&segments)?;
        let variant = ArchiveOrgDownloadVariant {
            filename: filename.clone(),
            download_url: download_url.clone(),
            format: member
                .rsplit('.')
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase(),
            size_bytes,
            provenance: ArchiveOrgFileProvenance::Unknown,
            is_video: false,
        };
        tracks.push(ArchiveOrgTrack {
            filename,
            title: plain_text(&member, false),
            download_url,
            duration_seconds: None,
            size_bytes,
            waveform_url: None,
            download_variants: vec![variant],
        });
    }
    tracks.sort_by(|left, right| natural_cmp(&left.filename, &right.filename));
    Ok(tracks)
}

/// Commits each playable identity once and enforces the shared retained-track limit.
fn finish_row(
    row: &mut Option<ListingRow>,
    members: &mut BTreeMap<String, Option<u64>>,
) -> Result<(), ProviderError> {
    if let Some(row) = row.take()
        && let Some(member) = row.member
    {
        let size = row.size.trim().parse().ok();
        if members
            .get(&member)
            .is_some_and(|previous| *previous != size)
        {
            return Err(invalid_response("conflicting ZIP member sizes"));
        }
        members.insert(member, size);
        if members.len() > MAX_TRACKS {
            return Err(invalid_response("too many playable ZIP members"));
        }
    }
    Ok(())
}

/// Accepts only this archive's canonical member links and rejects extraction recursion.
fn member_path(base: &Url, archive_filename: &str, href: &str) -> Option<String> {
    if href.len() > MAX_FILENAME_BYTES * 3 + base.as_str().len()
        || href.chars().any(char::is_control)
    {
        return None;
    }
    // Validate the raw member before URL parsing can erase literal/encoded dot segments.
    let encoded = if let Some(path) = href
        .strip_prefix("https://archive.org")
        .or_else(|| href.strip_prefix("//archive.org"))
    {
        path.strip_prefix(base.path())?
    } else if href.starts_with('/') {
        href.strip_prefix(base.path())?
    } else {
        href
    };
    let member = decode_path(encoded)?;
    if !valid_filename(&member)
        || !valid_filename(&format!("{archive_filename}/{member}"))
        || audio_format_rank(&member, "").is_none()
    {
        return None;
    }
    let url = base.join(href).ok()?;
    if url.scheme() != "https"
        || url.host_str() != Some("archive.org")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().starts_with(base.path())
    {
        return None;
    }
    // Archive interprets another archive suffix before the leaf as nested extraction.
    if member.split('/').rev().skip(1).any(|part| {
        part.rsplit_once('.').is_some_and(|(_, extension)| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz"
            )
        })
    }) {
        return None;
    }
    Some(member)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use url::Url;

    use super::*;

    /// Routes known fixture endpoints and records accidental eager ZIP downloads.
    struct FixtureTransport {
        metadata: Vec<u8>,
        listing: Vec<u8>,
        requests: Mutex<Vec<Url>>,
    }

    impl ArchiveOrgTransport for FixtureTransport {
        fn fetch(&self, url: &Url, _max_bytes: usize) -> Result<Vec<u8>, ProviderError> {
            self.requests.lock().unwrap().push(url.clone());
            if url.path().starts_with("/metadata/") {
                Ok(self.metadata.clone())
            } else if url.path().starts_with("/details/") {
                Ok(Vec::new())
            } else if url.path().ends_with(".zip/") {
                Ok(self.listing.clone())
            } else {
                panic!("unexpected archive request: {url}");
            }
        }
    }

    /// The reported ZIP-only item must expose an unopened archive without fetching it.
    #[test]
    fn zip_only_item_exposes_a_lazy_archive() {
        let transport = Arc::new(FixtureTransport {
			metadata: serde_json::to_vec(&json!({
				"metadata": {"identifier": "merry-bitsmix-3", "title": "Merry Bitsmix 3.0", "mediatype": "audio"},
				"files": [{"name": "Merry Bitsmix 3.0.zip", "format": "ZIP", "source": "original", "size": "2170096165", "filecount": "452"}]
			})).unwrap(),
			listing: Vec::new(),
			requests: Mutex::new(Vec::new()),
		});
        let details = ArchiveOrgClient::with_transport(transport.clone())
            .item_details("merry-bitsmix-3")
            .unwrap();
        let value = serde_json::to_value(&details).unwrap();
        assert_eq!(value["archives"][0]["filename"], "Merry Bitsmix 3.0.zip");
        assert_eq!(value["archives"][0]["size_bytes"], 2_170_096_165_u64);
        assert!(details.tracks.is_empty());
        assert_eq!(transport.requests.lock().unwrap().len(), 2);
    }

    /// Builds a public root item and a lazy listing without network access.
    fn fixture(listing: impl Into<Vec<u8>>) -> (ArchiveOrgClient, ArchiveOrgItemDetails) {
        let transport = Arc::new(FixtureTransport {
			metadata: serde_json::to_vec(&json!({
				"metadata": {"identifier": "mock_audio", "title": "Mock album", "mediatype": "audio"},
				"files": [{"name": "album.zip", "format": "ZIP", "size": "999999"}]
			})).unwrap(),
			listing: listing.into(),
			requests: Mutex::new(Vec::new()),
		});
        let client = ArchiveOrgClient::with_transport(transport);
        let parent = client.item_details("mock_audio").unwrap();
        (client, parent)
    }

    /// Actual Archive markup omits closing cells and encodes nested member slashes.
    #[test]
    fn real_listing_shape_retains_exact_members_sizes_and_audio_variants() {
        let (client, parent) = fixture(
            r#"<html><table class="archext">
			<caption>listing of album.zip</caption><tr><th>file<th>as jpg<th>timestamp<th>size</tr>
			<tr><td><a href="//archive.org/download/mock_audio/album.zip/Disc%201%2F02%20B%26C.mp3">wrong display name</a><td><td>2022-12-11 17:12<td id="size">5675484</tr>
			<tr><td><a href="https://archive.org/download/mock_audio/album.zip/Disc%201%2F01%20%D0%A2%D1%80%D0%B5%D0%BA.flac">Unicode</a><td><td>2022-12-11 17:12<td id="size">1234</tr>
			<tr><td>Disc 1/<td><td>2022-12-11 17:12<td id="size"></tr>
			<tr><td><a href="//archive.org/download/mock_audio/album.zip/art.jpg">art.jpg</a><td><td><td id="size">888</tr>
			</table></html>"#,
        );
        let details = client.zip_details(&parent, &parent.archives[0]).unwrap();
        assert_eq!(details.tracks.len(), 2);
        assert_eq!(details.tracks[0].title, "Disc 1/01 Трек.flac");
        assert_eq!(details.tracks[1].filename, "album.zip/Disc 1/02 B&C.mp3");
        assert_eq!(details.tracks[1].title, "Disc 1/02 B&C.mp3");
        assert_eq!(details.tracks[1].size_bytes, Some(5_675_484));
        assert_eq!(
            details.tracks[1].download_url.as_str(),
            "https://archive.org/download/mock_audio/album.zip/Disc%201/02%20B&C.mp3"
        );
        assert_eq!(details.tracks[1].download_variants.len(), 1);
        assert_eq!(
            preferred_audio_variant(&details.tracks[1])
                .unwrap()
                .download_url,
            details.tracks[1].download_url
        );
        assert_eq!(
            details.tracks[1].download_variants[0].provenance,
            ArchiveOrgFileProvenance::Unknown
        );
        assert_eq!(details.archive_filename.as_deref(), Some("album.zip"));
        assert!(details.archives.is_empty());
        assert_eq!(details.item, parent.item);
        assert_eq!(details.file_counts, None);
    }

    /// ZIP inventory must not admit restricted, conflicting, or misleading metadata.
    #[test]
    fn zip_inventory_filters_restricted_unsafe_and_duplicate_archives() {
        let archives = normalize_archives(&[
            json!({"name": "yes.zip", "format": "ZIP", "size": "12"}),
            json!({"name": "nested/UPPER.ZIP"}),
            json!({"name": "../escape.zip", "format": "ZIP"}),
            json!({"name": "private.zip", "private": true}),
            json!({"name": "blocked.zip", "access-restricted": "unknown"}),
            json!({"name": "metadata.zip", "format": "Metadata"}),
            json!({"name": "ambiguous.zip", "format": "ZIP"}),
            json!({"name": "ambiguous.zip", "private": true}),
            json!({"name": "not-a-zip.rar", "format": "RAR"}),
        ])
        .unwrap();
        assert_eq!(
            archives
                .iter()
                .map(|archive| archive.filename.as_str())
                .collect::<Vec<_>>(),
            ["nested/UPPER.ZIP", "yes.zip"]
        );
    }

    /// Link text, URL bases, encoded traversal and foreign archives cannot select media.
    #[test]
    fn listing_rejects_unsafe_members_and_deduplicates_valid_paths() {
        let hrefs = [
            "https://evil.invalid/download/mock_audio/album.zip/bad.mp3",
            "https://archive.org.evil.invalid/download/mock_audio/album.zip/bad.mp3",
            "//archive.org/download/other/album.zip/bad.mp3",
            "//archive.org/download/mock_audio/other.zip/bad.mp3",
            "//archive.org/download/mock_audio/album.zip/%2E%2E%2Fbad.mp3",
            "//archive.org/download/mock_audio/album.zip/dir/../traversal.mp3",
            "//archive.org/download/mock_audio/album.zip/dir/%2e%2e/traversal.mp3",
            "//archive.org/download/mock_audio/album.zip/./dot.mp3",
            "//archive.org/download/mock_audio/album.zip/%2Fbad.mp3",
            "//archive.org/download/mock_audio/album.zip/a%5Cbad.mp3",
            "//archive.org/download/mock_audio/album.zip/bad%00.mp3",
            "//archive.org/download/mock_audio/album.zip/bad%ZZ.mp3",
            "//archive.org/download/mock_audio/album.zip/bad%FF.mp3",
            "//archive.org/download/mock_audio/album.zip/song.mp3?download=1",
            "//archive.org/download/mock_audio/album.zip/song.mp3#fragment",
            "//archive.org/download/mock_audio/album.zip/nested.zip/secret.mp3",
            "//archive.org/download/mock_audio/album.zip/folder/",
            "//archive.org/download/mock_audio/album.zip/unknown.xyz",
            "//archive.org/download/mock_audio/album.zip/good.mp3",
            "//archive.org/download/mock_audio/album.zip/good.mp3",
        ];
        let rows: String = hrefs
            .iter()
            .map(|href| {
                format!("<tr><td><a href=\"{href}\">Good.mp3</a><td><td><td id=\"size\">12</tr>")
            })
            .collect();
        let (client, parent) = fixture(format!(
            "<base href=\"https://evil.invalid\"><table class=\"archext\">{rows}</table>"
        ));
        let details = client.zip_details(&parent, &parent.archives[0]).unwrap();
        assert_eq!(details.tracks.len(), 1);
        assert_eq!(details.tracks[0].filename, "album.zip/good.mp3");
    }

    /// Error pages, truncation and oversize responses fail instead of becoming empty albums.
    #[test]
    fn listing_failures_and_hard_limits_are_explicit() {
        for listing in [
            "<html>Please log in</html>",
            "<table class=\"archext\"><tr><td>unfinished",
        ] {
            let (client, parent) = fixture(listing);
            assert!(client.zip_details(&parent, &parent.archives[0]).is_err());
        }
        let (client, parent) = fixture(vec![b' '; MAX_HTML_BYTES + 1]);
        assert!(matches!(
            client.zip_details(&parent, &parent.archives[0]),
            Err(ProviderError::ResponseTooLarge { .. })
        ));
        let (client, parent) = fixture(format!(
            "<table class=\"archext\">{}</table>",
            "<tr><td>directory/</tr>".repeat(MAX_FILES + 1)
        ));
        assert!(client.zip_details(&parent, &parent.archives[0]).is_err());
        let (client, parent) = fixture(format!(
            "<table class=\"archext\">{}</table>",
            "<!--x-->".repeat(MAX_HTML_TOKENS + 1)
        ));
        assert!(client.zip_details(&parent, &parent.archives[0]).is_err());
        let rows: String = (0..=MAX_TRACKS)
            .map(|index| format!("<tr><td><a href=\"song{index}.mp3\">Song</a></tr>"))
            .collect();
        let (client, parent) = fixture(format!("<table class=\"archext\">{rows}</table>"));
        assert!(client.zip_details(&parent, &parent.archives[0]).is_err());
    }

    /// Conflicting listings of one URL cannot supply an arbitrary selected file size.
    #[test]
    fn duplicate_members_with_conflicting_sizes_fail() {
        let (client, parent) = fixture(
            r#"<table class="archext">
			<tr><td><a href="song.mp3">Song</a><td id="size">12</tr>
			<tr><td><a href="song.mp3">Song</a><td id="size">13</tr>
			</table>"#,
        );
        assert!(client.zip_details(&parent, &parent.archives[0]).is_err());
    }

    /// Percent signs and literal plus signs are decoded once, not as form input.
    #[test]
    fn member_paths_preserve_literal_percent_plus_and_query_characters() {
        let (client, parent) = fixture(
            r#"<table class="archext"><tr><td><a href="100%25%20A+B%3F%23.mp3">Song</a></tr></table>"#,
        );
        let details = client.zip_details(&parent, &parent.archives[0]).unwrap();
        assert_eq!(details.tracks[0].filename, "album.zip/100% A+B?#.mp3");
        assert_eq!(
            details.tracks[0].download_url.as_str(),
            "https://archive.org/download/mock_audio/album.zip/100%25%20A+B%3F%23.mp3"
        );
    }

    /// Optional live regression fetches only metadata and listing HTML, never ZIP/media bytes.
    #[test]
    #[ignore = "requires public Archive.org network access"]
    fn live_merry_bitsmix_zip_lists_310_audio_members() {
        let client = ArchiveOrgClient::new();
        let parent = client.item_details("merry-bitsmix-3").unwrap();
        let archive = parent
            .archives
            .iter()
            .find(|archive| archive.filename == "Merry Bitsmix 3.0.zip")
            .unwrap();
        let details = client.zip_details(&parent, archive).unwrap();
        assert_eq!(details.tracks.len(), 310);
        assert_eq!(details.tracks[0].size_bytes, Some(5_675_484));
        assert_eq!(
            details.tracks[0].filename,
            "Merry Bitsmix 3.0.zip/3D Santa Quest - Level 1.mp3"
        );
    }

    /// Only root inventories may authorize one of their exact unopened archives.
    #[test]
    fn zip_navigation_rejects_forged_or_nested_archive_requests() {
        let (client, parent) = fixture("<table class=\"archext\"></table>");
        let mut other = parent.archives[0].clone();
        other.filename = "other.zip".into();
        assert!(client.zip_details(&parent, &other).is_err());
        let details = client.zip_details(&parent, &parent.archives[0]).unwrap();
        assert!(details.tracks.is_empty());
        assert!(client.zip_details(&details, &parent.archives[0]).is_err());
    }

    /// Redirects must remain on Archive storage and preserve the exact requested ZIP.
    #[test]
    fn listing_redirect_validation_is_confined_to_matching_archive() {
        let source =
            Url::parse("https://archive.org/download/mock_audio/Disc%201/album.zip/").unwrap();
        let allowed = "https://dn801303.us.archive.org/view_archive.php?archive=/0/items/mock_audio/Disc%201/album.zip";
        assert!(listing_redirect(&source, allowed).is_some());
        for target in [
            "http://dn801303.us.archive.org/view_archive.php?archive=/0/items/mock_audio/Disc%201/album.zip",
            "https://archive.org.evil.invalid/view_archive.php?archive=/0/items/mock_audio/Disc%201/album.zip",
            "https://localhost/view_archive.php?archive=/0/items/mock_audio/Disc%201/album.zip",
            "https://user:pass@dn801303.us.archive.org/view_archive.php?archive=/0/items/mock_audio/Disc%201/album.zip",
            "https://dn801303.us.archive.org:444/view_archive.php?archive=/0/items/mock_audio/Disc%201/album.zip",
            "https://dn801303.us.archive.org/view_archive.php?archive=/0/items/other/Disc%201/album.zip",
            "https://dn801303.us.archive.org/view_archive.php?archive=/0/items/mock_audio/Disc%201/album.zip&file=song.mp3",
            "https://dn801303.us.archive.org/view_archive.php?archive=/0/items/mock_audio/Disc%201/album.zip&archive=/0/items/other.zip",
            "https://dn801303.us.archive.org/view_archive.php?archive=/0/items/mock_audio/Disc%201/album.zip#fragment",
        ] {
            assert!(listing_redirect(&source, target).is_none(), "{target}");
        }
        assert!(
            listing_redirect(
                &Url::parse("https://archive.org/metadata/mock_audio").unwrap(),
                allowed
            )
            .is_none()
        );
    }
}
