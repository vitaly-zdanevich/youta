//! Station icons discovered from a station's own homepage.
//!
//! Wikidata covers only the broadcasters notable enough to have an item, which
//! is a small minority of Youta's catalogue: a hobby FLAC stream has no item to
//! link to and never will. Every preset does carry a homepage, and a station's
//! site already advertises its own logo to browsers and messaging apps — so
//! that is where the rest of the catalogue's artwork comes from.
//!
//! Only three declarations are read, in the order a station logo is most likely
//! to be found: `apple-touch-icon`, `og:image`, then `rel="icon"`. The request
//! goes to a compile-time homepage from Youta's own curated catalogue rather
//! than to anything a provider or a user supplied, and the address it yields is
//! validated exactly like any other remote artwork URL before it is used — the
//! page is untrusted even though the site is.
//!
//! Nothing here decodes or fetches an image. This resolves one URL; the artwork
//! pipeline applies its own transport, size, and format rules to it.

use url::Url;

use super::{ProviderError, map_ureq_error, provider_agent};
use crate::domain::remote_url_has_non_public_host;

use std::time::Duration;

/// Bounded wait for one homepage request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest homepage body read.
const MAX_PAGE_BYTES: usize = 512 * 1024;
/// Bytes of markup scanned for icon declarations.
///
/// Declarations live in `<head>`, so this never needs the whole document — and
/// a page that buries them past this bound simply has no icon Youta can see.
const MAX_SCANNED_BYTES: usize = 128 * 1024;
/// Tags inspected before the scan gives up.
const MAX_SCANNED_TAGS: usize = 512;
/// Longest attribute value considered.
const MAX_ATTRIBUTE_BYTES: usize = 2 * 1024;
/// Image extensions Youta's artwork pipeline can actually render.
///
/// `rel="icon"` is frequently an ICO or an SVG, and neither survives the
/// pipeline, so one is not worth a request.
const RENDERABLE_EXTENSIONS: [&str; 4] = ["png", "jpg", "jpeg", "webp"];
/// Extensions that identify an image the pipeline cannot decode.
///
/// A touch icon or an `og:image` regularly turns out to be one of these — a
/// vector logo, an animated banner, a format newer than the decoder. Naming an
/// address is free, so a candidate that is already known to be undecodable is
/// skipped in favour of the next one rather than becoming a blank panel.
const UNRENDERABLE_EXTENSIONS: [&str; 8] =
    ["svg", "ico", "gif", "bmp", "tif", "tiff", "avif", "heic"];

/// Bounded client resolving one station homepage to its advertised icon.
pub struct StationIconResolver {
    agent: ureq::Agent,
    max_page_bytes: usize,
}

impl Default for StationIconResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl StationIconResolver {
    /// Creates a resolver with the common provider timeout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            agent: provider_agent(REQUEST_TIMEOUT),
            max_page_bytes: MAX_PAGE_BYTES,
        }
    }

    /// Returns the icon a station's homepage advertises, if any.
    ///
    /// Relative addresses resolve against the page Youta actually landed on
    /// rather than the address it asked for, because a homepage commonly
    /// redirects to `www.` or to a country path and a relative icon belongs to
    /// wherever it was declared.
    ///
    /// # Errors
    ///
    /// Returns an error when the homepage cannot be fetched or exceeds the
    /// bounded body size. A page that simply advertises no usable icon is
    /// `Ok(None)`.
    pub fn fetch(&self, homepage: &Url) -> Result<Option<Url>, ProviderError> {
        if !is_public_web_url(homepage) {
            return Err(ProviderError::InvalidRequest(
                "station homepage must be a public HTTP or HTTPS URL".to_owned(),
            ));
        }
        let mut response = self
            .agent
            .get(homepage.as_str())
            .header("Accept", "text/html")
            .call()
            .map_err(map_ureq_error)?;
        if response
            .body()
            .content_length()
            .is_some_and(|length| length > self.max_page_bytes as u64)
        {
            return Err(ProviderError::ResponseTooLarge {
                limit: self.max_page_bytes,
            });
        }
        let landed = {
            use ureq::ResponseExt;

            Url::parse(&response.get_uri().to_string()).unwrap_or_else(|_| homepage.clone())
        };
        let page = response
            .body_mut()
            .with_config()
            .limit(u64::try_from(self.max_page_bytes.saturating_add(1)).unwrap_or(u64::MAX))
            .read_to_string()
            .map_err(|error| match error {
                ureq::Error::BodyExceedsLimit(_) => ProviderError::ResponseTooLarge {
                    limit: self.max_page_bytes,
                },
                other => ProviderError::Transport(other.to_string()),
            })?;
        Ok(icon_in_html(&page, &landed))
    }
}

/// Picks the most logo-like icon one page advertises.
fn icon_in_html(page: &str, landed: &Url) -> Option<Url> {
    let scanned = &page[..page.len().min(MAX_SCANNED_BYTES)];
    let mut best: Option<(u8, Url)> = None;
    for (tag, attributes) in tags(scanned).take(MAX_SCANNED_TAGS) {
        let Some((priority, reference)) = icon_reference(tag, &attributes) else {
            continue;
        };
        if has_unrenderable_extension(reference) {
            continue;
        }
        if best
            .as_ref()
            .is_some_and(|(selected, _)| *selected <= priority)
        {
            continue;
        }
        let Some(url) = landed
            .join(&decode_entities(reference))
            .ok()
            .filter(is_public_web_url)
        else {
            continue;
        };
        best = Some((priority, url));
    }
    best.map(|(_, url)| url)
}

/// Classifies one `<link>` or `<meta>` tag, lower being more logo-like.
fn icon_reference<'a>(tag: &str, attributes: &[(String, &'a str)]) -> Option<(u8, &'a str)> {
    let value = |name: &str| {
        attributes
            .iter()
            .find(|(attribute, _)| attribute == name)
            .map(|(_, value)| *value)
    };
    match tag {
        "link" => {
            let relation = value("rel")?.to_ascii_lowercase();
            let reference = value("href")?;
            if relation
                .split_whitespace()
                .any(|word| word == "apple-touch-icon")
                || relation
                    .split_whitespace()
                    .any(|word| word == "apple-touch-icon-precomposed")
            {
                Some((0, reference))
            } else if relation.split_whitespace().any(|word| word == "icon")
                && has_renderable_extension(reference)
            {
                Some((2, reference))
            } else {
                None
            }
        }
        "meta" => {
            let kind = value("property").or_else(|| value("name"))?;
            (kind.eq_ignore_ascii_case("og:image") || kind.eq_ignore_ascii_case("og:image:url"))
                .then(|| value("content"))
                .flatten()
                .map(|reference| (1, reference))
        }
        _ => None,
    }
}

/// Whether an address ends in an extension the artwork pipeline can render.
fn has_renderable_extension(reference: &str) -> bool {
    extension_of(reference).is_some_and(|extension| {
        RENDERABLE_EXTENSIONS
            .iter()
            .any(|candidate| extension.eq_ignore_ascii_case(candidate))
    })
}

/// Whether an address names a format the artwork pipeline cannot decode.
///
/// An address with no extension at all is not rejected: plenty of sites serve
/// an icon from a path that names no format, and the pipeline decides from the
/// bytes anyway.
fn has_unrenderable_extension(reference: &str) -> bool {
    extension_of(reference).is_some_and(|extension| {
        UNRENDERABLE_EXTENSIONS
            .iter()
            .any(|candidate| extension.eq_ignore_ascii_case(candidate))
    })
}

/// Returns the extension of an address, ignoring its query and fragment.
fn extension_of(reference: &str) -> Option<&str> {
    let path = reference
        .split(['?', '#'])
        .next()
        .unwrap_or(reference)
        .trim_end_matches('/');
    let (_, extension) = path.rsplit_once('.')?;
    (!extension.is_empty() && !extension.contains('/')).then_some(extension)
}

/// Yields each `<link>` and `<meta>` tag's name and parsed attributes.
///
/// This is deliberately not an HTML parser: it looks for two void elements that
/// carry no content, so a scan is enough and a malformed page costs a missing
/// icon rather than a wrong one.
fn tags(page: &str) -> impl Iterator<Item = (&str, Vec<(String, &str)>)> {
    let mut rest = page;
    std::iter::from_fn(move || {
        loop {
            let start = rest.find('<')?;
            rest = &rest[start + 1..];
            let end = rest.find('>')?;
            let (inside, remainder) = rest.split_at(end);
            rest = &remainder[1..];
            let mut characters = inside.char_indices();
            let name_end = characters
                .find(|(_, character)| character.is_whitespace() || *character == '/')
                .map_or(inside.len(), |(index, _)| index);
            let name = &inside[..name_end];
            if name.eq_ignore_ascii_case("link") || name.eq_ignore_ascii_case("meta") {
                let lowercase = if name.eq_ignore_ascii_case("link") {
                    "link"
                } else {
                    "meta"
                };
                return Some((lowercase, attributes(&inside[name_end..])));
            }
        }
    })
}

/// Parses one tag's attributes, lowercasing names and unquoting values.
fn attributes(inside: &str) -> Vec<(String, &str)> {
    let mut parsed = Vec::new();
    let bytes = inside.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        while index < bytes.len() && (bytes[index].is_ascii_whitespace() || bytes[index] == b'/') {
            index += 1;
        }
        let name_start = index;
        while index < bytes.len()
            && !bytes[index].is_ascii_whitespace()
            && bytes[index] != b'='
            && bytes[index] != b'/'
        {
            index += 1;
        }
        if name_start == index {
            break;
        }
        let name = inside[name_start..index].to_ascii_lowercase();
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] != b'=' {
            parsed.push((name, ""));
            continue;
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let value = if index < bytes.len() && (bytes[index] == b'"' || bytes[index] == b'\'') {
            let quote = bytes[index];
            index += 1;
            let start = index;
            while index < bytes.len() && bytes[index] != quote {
                index += 1;
            }
            let value = &inside[start..index];
            index = (index + 1).min(bytes.len());
            value
        } else {
            let start = index;
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            &inside[start..index]
        };
        parsed.push((name, value.trim()));
    }
    parsed.retain(|(_, value)| value.len() <= MAX_ATTRIBUTE_BYTES);
    parsed
}

/// Expands the entities an address in markup can legitimately carry.
fn decode_entities(reference: &str) -> String {
    reference
        .replace("&amp;", "&")
        .replace("&#38;", "&")
        .replace("&#x26;", "&")
}

/// Whether a URL may be requested or handed to the artwork pipeline.
fn is_public_web_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && !remote_url_has_non_public_host(url)
}

#[cfg(test)]
mod tests {
    use super::{icon_in_html, is_public_web_url};

    use url::Url;

    fn page_url() -> Url {
        Url::parse("https://station.example/en/").expect("landed page URL")
    }

    /// A square touch icon is the station's logo; `og:image` is often a banner
    /// or a promo, and `rel="icon"` is often a favicon — so they rank below it.
    #[test]
    fn the_touch_icon_outranks_og_image_and_the_favicon() {
        let page = r#"
            <html><head>
            <link rel="shortcut icon" href="/favicon.png">
            <meta property="og:image" content="https://cdn.example/banner.jpg">
            <link rel="apple-touch-icon" sizes="180x180" href="../icons/touch.png">
            </head></html>
        "#;
        assert_eq!(
            icon_in_html(page, &page_url()).as_ref().map(Url::as_str),
            Some("https://station.example/icons/touch.png"),
            "a relative icon resolves against the page it was declared on"
        );
    }

    /// Attribute syntax in the wild is inconsistent, and an address in markup
    /// can carry entities.
    #[test]
    fn quoting_styles_attribute_order_and_entities_are_all_handled() {
        let page = "<META CONTENT='https://cdn.example/logo.png?a=1&amp;b=2' PROPERTY=og:image>";
        assert_eq!(
            icon_in_html(page, &page_url()).as_ref().map(Url::as_str),
            Some("https://cdn.example/logo.png?a=1&b=2")
        );
    }

    /// A touch icon that is a vector or an animation is no use to a pipeline
    /// that decodes JPEG, PNG, and WebP — so the next candidate wins instead of
    /// the panel staying blank.
    #[test]
    fn an_undecodable_touch_icon_yields_to_the_next_candidate() {
        let page = r#"
            <link rel="apple-touch-icon" href="/logo.svg">
            <meta property="og:image" content="/logo.png">
        "#;
        assert_eq!(
            icon_in_html(page, &page_url()).as_ref().map(Url::as_str),
            Some("https://station.example/logo.png")
        );

        // An address that names no format at all is still worth trying: the
        // pipeline decides from the bytes.
        assert_eq!(
            icon_in_html(
                r#"<link rel="apple-touch-icon" href="/icon?size=180">"#,
                &page_url()
            )
            .as_ref()
            .map(Url::as_str),
            Some("https://station.example/icon?size=180")
        );
        assert_eq!(
            icon_in_html(
                r#"<link rel="apple-touch-icon" href="/touch.gif">"#,
                &page_url()
            ),
            None
        );
    }

    /// A favicon is worth a request only when the pipeline could render it.
    #[test]
    fn an_ico_or_svg_favicon_is_not_worth_a_request() {
        for markup in [
            r#"<link rel="icon" href="/favicon.ico">"#,
            r#"<link rel="icon" type="image/svg+xml" href="/logo.svg">"#,
            r#"<link rel="stylesheet" href="/style.css">"#,
            r#"<meta name="description" content="a station">"#,
            "<html><head><title>no icon at all</title></head></html>",
        ] {
            assert_eq!(icon_in_html(markup, &page_url()), None, "{markup}");
        }
        assert!(
            icon_in_html(r#"<link rel="icon" href="/favicon.png">"#, &page_url()).is_some(),
            "a PNG favicon is still better than no artwork"
        );
    }

    /// The page is untrusted even though the station is: an address it declares
    /// must not reach the artwork pipeline unless it is public and credential
    /// free.
    #[test]
    fn a_page_cannot_point_artwork_at_a_private_or_odd_destination() {
        for reference in [
            "http://127.0.0.1/logo.png",
            "http://192.168.0.5/logo.png",
            "http://printer.local/logo.png",
            "file:///etc/passwd",
            "data:image/png;base64,AAAA",
            "javascript:alert(1)",
            "http://user:secret@station.example/logo.png",
        ] {
            let markup = format!(r#"<link rel="apple-touch-icon" href="{reference}">"#);
            assert_eq!(icon_in_html(&markup, &page_url()), None, "{reference}");
        }
    }

    /// The scan is bounded, so a page that never declares an icon cannot cost
    /// unbounded work.
    #[test]
    fn an_enormous_page_without_icons_stays_bounded() {
        let page = "<div class=\"row\"></div>".repeat(200_000);
        assert_eq!(icon_in_html(&page, &page_url()), None);
    }

    #[test]
    fn only_public_web_urls_are_accepted() {
        assert!(is_public_web_url(
            &Url::parse("https://station.example/").expect("public URL")
        ));
        assert!(!is_public_web_url(
            &Url::parse("http://localhost/").expect("loopback URL")
        ));
    }
}
