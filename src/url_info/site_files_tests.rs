//! Mocked file fetching and bounded sitemap parsing, without any external requests.

use super::*;
use crate::url_info::{HttpResponse, LOOKUP_TIMEOUT};
use std::collections::VecDeque;
use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;

#[test]
fn target_uses_only_the_validated_origin() {
    let url = Url::parse("https://example.test/music/track?token=secret#part").unwrap();
    assert_eq!(
        target(&url, false).unwrap().as_str(),
        "https://example.test/robots.txt"
    );
    assert_eq!(
        target(&url, true).unwrap().as_str(),
        "https://example.test/sitemap.xml"
    );
}

#[test]
fn unsafe_origins_are_rejected_before_any_request() {
    for raw in [
        "https://user:password@example.test/x",
        "https://127.0.0.1/x",
        "http://[::1]/x",
        "https://localhost/x",
        "https://10.0.0.1/x",
        "https://example.test:8443/x",
        "file:///etc/passwd",
    ] {
        let url = Url::parse(raw).unwrap();
        let transport = MockTransport::default();
        assert!(target(&url, false).is_err(), "{raw}");
        assert!(lookup_with(&url, false, &AtomicBool::new(false), &transport).is_err());
        assert!(transport.requests.lock().unwrap().is_empty());
    }
}

#[test]
fn robots_retains_lines_but_never_follows_its_sitemap_directives() {
    let body = "User-agent: *\nDisallow: /private\n\nSitemap: https://other.test/hidden.xml\n";
    let transport = MockTransport::default();
    transport.push(
        200,
        "text/plain; charset=utf-8",
        body.as_bytes().to_vec(),
        None,
    );
    let url = target(
        &Url::parse("https://example.test/private?token=secret").unwrap(),
        false,
    )
    .unwrap();
    let file = lookup_with(&url, false, &AtomicBool::new(false), &transport).unwrap();
    assert_eq!(file.content, SiteFileContent::Robots(body.to_owned()));
    assert_eq!(
        *transport.requests.lock().unwrap(),
        vec!["https://example.test/robots.txt"]
    );
}

#[test]
fn sanitization_removes_ansi_c1_and_bidi_without_dropping_newlines() {
    assert_eq!(
        sanitize_text(
            "a\r\n\t\u{1b}[31mb\u{1b}[0m\u{1b}]8;;hidden\u{7}c\u{1b}]8;;\u{1b}\\\u{1b}Psecret\u{1b}\\\u{9b}31m\u{9d}secret\u{9c}\u{202e}\u{2066}\u{200b}\0\r\n"
        ),
        "a\n bc\n"
    );
}

#[test]
fn sitemap_index_lists_children_without_fetching_them() {
    let transport = MockTransport::default();
    transport.push(200, "application/xml", br#"<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"><sitemap><loc>child.xml</loc><lastmod>2026-10-10</lastmod></sitemap><sitemap><loc>https://other.test/archive.xml.gz</loc></sitemap></sitemapindex>"#.to_vec(), None);
    let file = lookup_with(
        &Url::parse("https://example.test/catalog/index.xml").unwrap(),
        true,
        &AtomicBool::new(false),
        &transport,
    )
    .unwrap();
    let SiteFileContent::Sitemap { index, entries } = file.content else {
        panic!("expected sitemap")
    };
    assert!(index);
    assert_eq!(
        entries[0].url.as_str(),
        "https://example.test/catalog/child.xml"
    );
    assert_eq!(
        entries[0].metadata,
        vec![("Last modified".to_owned(), "2026-10-10".to_owned())]
    );
    assert_eq!(entries[1].url.as_str(), "https://other.test/archive.xml.gz");
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

#[test]
fn urlset_preserves_supplied_metadata_and_decodes_standard_entities() {
    let text = r#"<urlset xmlns:image="http://www.google.com/schemas/sitemap-image/1.1"><url><loc>https://example.test/page?a=1&amp;b=2</loc><lastmod>2026-10-09T12:00:00Z</lastmod><changefreq>weekly</changefreq><priority>0.8</priority><image:image><image:caption>Title &#38; details</image:caption></image:image></url></urlset>"#;
    let (index, entries) = parsed(text);
    assert!(!index);
    assert_eq!(entries[0].url.query(), Some("a=1&b=2"));
    assert_eq!(
        entries[0].metadata,
        vec![
            ("Last modified".into(), "2026-10-09T12:00:00Z".into()),
            ("Change frequency".into(), "weekly".into()),
            ("Priority".into(), "0.8".into()),
            (
                "image:image / image:caption".into(),
                "Title & details".into()
            ),
        ]
    );
}

#[test]
fn dtd_entities_unsafe_locations_and_malformed_xml_are_rejected() {
    for text in [
        "<!DOCTYPE urlset [<!ENTITY private SYSTEM 'file:///etc/passwd'>]><urlset/>",
        "<urlset><url><loc>https://example.test/&custom;</loc></url></urlset>",
        "<urlset><url><loc>https://127.0.0.1/private</loc></url></urlset>",
        "<urlset><url><loc>file:///etc/passwd</loc></url></urlset>",
        "<urlset><url><loc>https://user:secret@example.test/</loc></url></urlset>",
        "<urlset><url><loc>https://example.test/&#10;hidden</loc></url></urlset>",
        "<urlset><url><loc>https://example.test/</loc><loc>https://other.test/</loc></url></urlset>",
        "<urlset><url><loc><part>https://example.test/</part></loc></url></urlset>",
        "<urlset><url></url></urlset>",
        "<urlset><url><loc>https://example.test/</loc></wrong></urlset>",
        "<urlset/><urlset/>",
        "<html>not a sitemap</html>",
    ] {
        assert!(
            parse_sitemap(
                text,
                &Url::parse("https://example.test/sitemap.xml").unwrap(),
                &Budget::new(&AtomicBool::new(false))
            )
            .is_err(),
            "{text}"
        );
    }
}

#[test]
fn parser_limits_reject_complete_files_instead_of_silently_dropping_entries() {
    for text in [
        format!(
            "<urlset><url><loc>https://example.test/</loc><title>{}</title></url></urlset>",
            "a".repeat(MAX_FIELD_BYTES + 1)
        ),
        format!(
            "<urlset><url><loc>https://example.test/</loc>{}</url></urlset>",
            "<field>x</field>".repeat(MAX_METADATA_FIELDS + 1)
        ),
        format!(
            "<urlset>{}</urlset>",
            "<url><loc>https://example.test/</loc></url>".repeat(MAX_ENTRIES + 1)
        ),
        format!(
            "<urlset><url><loc>https://example.test/</loc>{}{}</url></urlset>",
            "<field>".repeat(MAX_XML_DEPTH),
            "</field>".repeat(MAX_XML_DEPTH)
        ),
    ] {
        let error = parse_sitemap(
            &text,
            &Url::parse("https://example.test/sitemap.xml").unwrap(),
            &Budget::new(&AtomicBool::new(false)),
        )
        .unwrap_err();
        assert!(error.contains("limit"), "{error}");
    }
}

#[test]
fn cdata_namespaces_and_whitespace_keep_metadata_readable() {
    let (index, entries) = parsed(
        "<?xml version='1.0' encoding='UTF-8'?><sm:urlset xmlns:sm='urn:sitemap'><sm:url><sm:loc><![CDATA[https://example.test/page?a=1&b=2]]></sm:loc><title><![CDATA[Hello <world>\n  again]]></title></sm:url></sm:urlset>",
    );
    assert!(!index);
    assert_eq!(entries[0].url.query(), Some("a=1&b=2"));
    assert_eq!(
        entries[0].metadata,
        vec![("title".into(), "Hello <world> again".into())]
    );
    assert_eq!(parsed("<urlset/>").1, Vec::new());
}

#[test]
fn exact_file_limit_is_accepted_and_larger_files_are_not_truncated() {
    for (length, succeeds) in [(MAX_FILE_BYTES, true), (MAX_FILE_BYTES + 1, false)] {
        let transport = MockTransport::default();
        transport.push(200, "text/plain", vec![b'a'; length], None);
        let result = lookup_with(
            &Url::parse("https://example.test/robots.txt").unwrap(),
            false,
            &AtomicBool::new(false),
            &transport,
        );
        assert_eq!(result.is_ok(), succeeds);
        if let Err(error) = result {
            assert!(error.contains("1 MiB"));
        }
    }
}

#[test]
fn gzip_sitemaps_are_parsed_and_decompression_has_its_own_limit() {
    let mut exact_limit = b"<urlset/>".to_vec();
    exact_limit.resize(MAX_FILE_BYTES, b' ');
    for (content, succeeds) in [
        (b"<urlset/>".to_vec(), true),
        (exact_limit, true),
        (vec![b' '; MAX_FILE_BYTES + 1], false),
    ] {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&content).unwrap();
        let transport = MockTransport::default();
        transport.push(200, "application/gzip", encoder.finish().unwrap(), None);
        let result = lookup_with(
            &Url::parse("https://example.test/child.xml.gz").unwrap(),
            true,
            &AtomicBool::new(false),
            &transport,
        );
        assert_eq!(result.is_ok(), succeeds);
        if let Err(error) = result {
            assert!(error.contains("1 MiB"));
        }
    }
}

#[test]
fn broken_compressed_sitemaps_return_a_clear_error() {
    let transport = MockTransport::default();
    transport.push(200, "application/gzip", vec![0x1f, 0x8b, 0], None);
    let error = lookup_with(
        &Url::parse("https://example.test/child.xml.gz").unwrap(),
        true,
        &AtomicBool::new(false),
        &transport,
    )
    .unwrap_err();
    assert!(error.contains("compressed sitemap is malformed"), "{error}");
}

#[test]
fn missing_files_wrong_mime_and_bad_encoding_have_clear_errors() {
    for (status, mime, body, expected) in [
        (404, "text/html", Vec::new(), "not found"),
        (200, "application/xml", Vec::new(), "plain text"),
        (200, "text/plain", vec![0xff], "UTF-8"),
    ] {
        let transport = MockTransport::default();
        transport.push(status, mime, body, None);
        let error = lookup_with(
            &Url::parse("https://example.test/robots.txt").unwrap(),
            false,
            &AtomicBool::new(false),
            &transport,
        )
        .unwrap_err();
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn redirects_retain_the_final_base_but_private_or_insecure_hops_are_blocked() {
    for destination in [
        "http://example.test/file",
        "https://127.0.0.1/file",
        "https://user:secret@other.test/file",
    ] {
        let transport = MockTransport::default();
        transport.push(302, "text/plain", Vec::new(), Some(destination));
        assert!(
            lookup_with(
                &Url::parse("https://example.test/robots.txt").unwrap(),
                false,
                &AtomicBool::new(false),
                &transport
            )
            .is_err()
        );
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }
    let transport = MockTransport::default();
    transport.push(
        302,
        "text/plain",
        Vec::new(),
        Some("https://other.test/maps/index.xml"),
    );
    transport.push(
        200,
        "text/xml",
        b"<sitemapindex><sitemap><loc>child.xml</loc></sitemap></sitemapindex>".to_vec(),
        None,
    );
    let file = lookup_with(
        &Url::parse("https://example.test/sitemap.xml").unwrap(),
        true,
        &AtomicBool::new(false),
        &transport,
    )
    .unwrap();
    assert_eq!(file.url.as_str(), "https://other.test/maps/index.xml");
    let SiteFileContent::Sitemap { entries, .. } = file.content else {
        panic!("expected sitemap")
    };
    assert_eq!(entries[0].url.as_str(), "https://other.test/maps/child.xml");
    assert_eq!(transport.requests.lock().unwrap().len(), 2);
}

#[test]
fn cancellation_prevents_requests_and_empty_robots_are_allowed() {
    let transport = MockTransport::default();
    assert!(
        lookup_with(
            &Url::parse("https://example.test/robots.txt").unwrap(),
            false,
            &AtomicBool::new(true),
            &transport
        )
        .is_err()
    );
    assert!(transport.requests.lock().unwrap().is_empty());
    transport.push(200, "text/plain", Vec::new(), None);
    assert_eq!(
        lookup_with(
            &Url::parse("https://example.test/robots.txt").unwrap(),
            false,
            &AtomicBool::new(false),
            &transport
        )
        .unwrap()
        .content,
        SiteFileContent::Robots(String::new())
    );
}

fn parsed(text: &str) -> (bool, Vec<SitemapEntry>) {
    let SiteFileContent::Sitemap { index, entries } = parse_sitemap(
        text,
        &Url::parse("https://example.test/sitemap.xml").unwrap(),
        &Budget::new(&AtomicBool::new(false)),
    )
    .unwrap() else {
        panic!("expected sitemap")
    };
    (index, entries)
}

/// One queued response per expected request; no DNS or external network is used.
#[derive(Default)]
struct MockTransport {
    responses: Mutex<VecDeque<HttpResponse>>,
    requests: Mutex<Vec<String>>,
}

impl MockTransport {
    fn push(&self, status: u16, content_type: &str, body: Vec<u8>, location: Option<&str>) {
        self.responses.lock().unwrap().push_back(HttpResponse {
            status,
            location: location.map(str::to_owned),
            content_type: content_type.to_owned(),
            body,
            ..Default::default()
        });
    }
}

impl HttpTransport for MockTransport {
    fn fetch(
        &self,
        url: &Url,
        _: DocumentKind,
        timeout: Duration,
        _: &AtomicBool,
    ) -> Result<HttpResponse, Failure> {
        assert!(!timeout.is_zero() && timeout <= LOOKUP_TIMEOUT);
        self.requests.lock().unwrap().push(url.to_string());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(Failure::Transport)
    }
}
