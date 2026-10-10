//! Conservative analytics declarations from an already downloaded HTML prefix.
//!
//! Detection never fetches or executes scripts. Missing declarations do not prove
//! a site has no analytics: consent gates, runtime injection, self-hosted scripts,
//! obfuscated code, and content beyond the bounded prefix are intentionally missed.
//! Google tag examples: <https://developers.google.com/tag-platform/gtagjs>.
//! Yandex variants: <https://yandex.ru/support/metrica/en/general/alternative-domain>.

use html5gum::{DefaultEmitter, Token, Tokenizer};
use url::Url;

use super::{MAX_HTML_BYTES, MAX_HTML_TOKENS};

const LABELS: [&str; 4] = [
    "Google Analytics",
    "Google Tag",
    "Google Tag Manager",
    "Yandex Metrica",
];
const MAX_SCRIPT_BYTES: usize = 64 * 1024;
const MAX_SCRIPT_TOKENS: usize = 8_192;

/// Returns stable, deduplicated provider labels only when recognizable evidence exists.
/// HTML entities are decoded in attributes, not in JavaScript raw-text content.
pub(super) fn detect(html: &[u8]) -> Vec<&'static str> {
    let mut found = [false; LABELS.len()];
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    let mut templates = 0_usize;
    let mut in_script = false;
    let mut in_noscript = false;
    let mut script = Vec::new();
    let mut remaining = MAX_HTML_TOKENS;
    for token in Tokenizer::new_with_emitter(&html[..html.len().min(MAX_HTML_BYTES)], emitter) {
        if remaining == 0 {
            break;
        }
        remaining -= 1;
        let Ok(token) = token;
        match token {
            Token::StartTag(tag) => {
                let name = tag.name.as_slice();
                if name == b"template" {
                    templates += 1;
                }
                if templates > 0 {
                    continue;
                }
                if name == b"noscript" {
                    in_noscript = true;
                }
                if name != b"script" {
                    continue;
                }
                let attribute = |name: &[u8]| {
                    tag.attributes
                        .get(name)
                        .and_then(|value| std::str::from_utf8(value.value.as_ref()).ok())
                };
                let executable = attribute(b"type").is_none_or(|kind| {
                    matches!(
                        kind.trim().to_ascii_lowercase().as_str(),
                        "" | "module"
                            | "text/javascript"
                            | "application/javascript"
                            | "text/ecmascript"
                            | "application/ecmascript"
                    )
                });
                script.clear();
                in_script = executable && attribute(b"src").is_none();
                if executable && let Some(source) = attribute(b"src") {
                    record_source(source, &mut found);
                }
            }
            Token::EndTag(tag) => match tag.name.as_slice() {
                b"template" => templates = templates.saturating_sub(1),
                b"script" if in_script => {
                    inspect_script(&script, &mut found);
                    script.clear();
                    in_script = false;
                }
                b"noscript" => in_noscript = false,
                _ => {}
            },
            Token::String(value) if templates == 0 => {
                if in_script {
                    let bytes = value.value.as_slice();
                    let amount = bytes
                        .len()
                        .min(MAX_SCRIPT_BYTES.saturating_sub(script.len()));
                    script.extend_from_slice(&bytes[..amount]);
                } else if in_noscript {
                    inspect_noscript(value.value.as_slice(), &mut found, &mut remaining);
                }
            }
            _ => {}
        }
    }
    LABELS
        .into_iter()
        .zip(found)
        .filter_map(|(label, present)| present.then_some(label))
        .collect()
}

/// Recognizes only explicit HTTP(S) provider URLs, including protocol-relative sources.
fn provider_url(source: &str) -> Option<Url> {
    let source = source.trim();
    if source.len() > 4_096 || source.contains('\\') {
        return None;
    }
    let source = if source.starts_with("//") {
        format!("https:{source}")
    } else {
        source.to_owned()
    };
    let url = Url::parse(&source).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none())
    .then_some(url)
}

/// Distinguishes Google Tag from Tag Manager; loading either does not itself establish Analytics.
fn record_source(source: &str, found: &mut [bool; 4]) {
    let Some(url) = provider_url(source) else {
        return;
    };
    match (url.host_str().unwrap_or(""), url.path()) {
        (
            "google-analytics.com" | "www.google-analytics.com" | "ssl.google-analytics.com",
            "/analytics.js" | "/ga.js",
        ) => found[0] = true,
        ("googletagmanager.com" | "www.googletagmanager.com", "/gtag/js") => found[1] = true,
        ("googletagmanager.com" | "www.googletagmanager.com", "/gtm.js") => found[2] = true,
        ("mc.yandex.ru" | "mc.yandex.com", "/metrika/tag.js" | "/metrika/watch.js") => {
            found[3] = true;
        }
        _ => {}
    }
}

/// Reads only literal Yandex counter image declarations inside raw noscript markup.
fn inspect_noscript(html: &[u8], found: &mut [bool; 4], remaining: &mut usize) {
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    let mut templates = 0_usize;
    for token in Tokenizer::new_with_emitter(html, emitter) {
        if *remaining == 0 {
            break;
        }
        *remaining -= 1;
        let Ok(token) = token;
        match token {
            Token::StartTag(tag) if tag.name.as_slice() == b"template" => templates += 1,
            Token::EndTag(tag) if tag.name.as_slice() == b"template" => {
                templates = templates.saturating_sub(1);
            }
            Token::StartTag(tag) if templates == 0 && tag.name.as_slice() == b"img" => {
                let source = tag
                    .attributes
                    .get(b"src".as_slice())
                    .and_then(|value| std::str::from_utf8(value.value.as_ref()).ok())
                    .and_then(provider_url);
                if let Some(url) = source
                    && matches!(url.host_str(), Some("mc.yandex.ru" | "mc.yandex.com"))
                    && url.path().strip_prefix("/watch/").is_some_and(|id| {
                        !id.is_empty()
                            && id.len() <= 20
                            && id.bytes().all(|byte| byte.is_ascii_digit())
                    })
                {
                    found[3] = true;
                }
            }
            _ => {}
        }
    }
}

/// Minimal lexical categories keep examples inside strings, comments, and regexes inert.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScriptToken<'a> {
    Ident(&'a str),
    Literal(&'a str),
    Punct(u8),
}

/// Tokenizes bounded script text without evaluating expressions or interpreting escape sequences.
fn script_tokens(script: &[u8]) -> Vec<ScriptToken<'_>> {
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < script.len() && tokens.len() < MAX_SCRIPT_TOKENS {
        let byte = script[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if script[index..].starts_with(b"//") || script[index..].starts_with(b"<!--") {
            index += script[index..]
                .iter()
                .position(|byte| *byte == b'\n')
                .unwrap_or(script.len() - index);
            continue;
        }
        if script[index..].starts_with(b"/*") {
            index += 2;
            index += script[index..]
                .windows(2)
                .position(|pair| pair == b"*/")
                .map_or(script.len() - index, |length| length + 2);
            continue;
        }
        if matches!(byte, b'\'' | b'"' | b'`' | b'/') {
            let start = index + 1;
            index = start;
            let mut character_class = false;
            while index < script.len() {
                let current = script[index];
                if current == b'\\' {
                    index = (index + 2).min(script.len());
                    continue;
                }
                if byte == b'/' {
                    if current == b'[' {
                        character_class = true;
                    }
                    if current == b']' {
                        character_class = false;
                    }
                }
                if current == byte && !character_class {
                    break;
                }
                if current == b'\n' && byte != b'`' {
                    break;
                }
                index += 1;
            }
            if script.get(index) == Some(&byte) && matches!(byte, b'\'' | b'"') {
                if let Ok(value) = std::str::from_utf8(&script[start..index]) {
                    tokens.push(ScriptToken::Literal(value));
                }
            } else {
                tokens.push(ScriptToken::Punct(b'?'));
            }
            index = (index + 1).min(script.len());
            continue;
        }
        if byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$') {
            let start = index;
            while index < script.len()
                && (script[index].is_ascii_alphanumeric() || matches!(script[index], b'_' | b'$'))
            {
                index += 1;
            }
            if let Ok(name) = std::str::from_utf8(&script[start..index]) {
                tokens.push(ScriptToken::Ident(name));
            }
        } else {
            tokens.push(ScriptToken::Punct(byte));
            index += 1;
        }
    }
    tokens
}

/// Recognizes common literal loader forms and direct gtag configuration, not arbitrary JavaScript.
fn inspect_script(script: &[u8], found: &mut [bool; 4]) {
    use ScriptToken::{Ident, Literal, Punct};
    let tokens = script_tokens(script);
    let creates_script = tokens.contains(&Literal("script"))
        && tokens
            .windows(2)
            .any(|pair| pair == [Ident("createElement"), Punct(b'(')]);
    let assigns_source = tokens
        .windows(3)
        .any(|part| part == [Punct(b'.'), Ident("src"), Punct(b'=')]);
    for (index, token) in tokens.iter().enumerate() {
        if creates_script
            && assigns_source
            && let Literal(value) = token
        {
            let direct_source =
                index >= 3 && tokens[index - 3..index] == [Punct(b'.'), Ident("src"), Punct(b'=')];
            let loader_argument =
                index >= 2 && tokens[index - 2..index] == [Literal("script"), Punct(b',')];
            if direct_source || loader_argument {
                record_source(&value.replace("\\/", "/"), found);
            }
        }
        if *token == Ident("gtag")
            && (index == 0
                || tokens[index - 1] != Punct(b'.')
                || (index >= 2 && tokens[index - 2] == Ident("window")))
            && let Some(
                [
                    Punct(b'('),
                    Literal("config"),
                    Punct(b','),
                    Literal(id),
                    Punct(b')' | b','),
                    ..,
                ],
            ) = tokens.get(index + 1..)
            && analytics_id(id)
        {
            found[0] = true;
            found[1] = true;
        }
    }
}

/// Accepts only published Analytics identifier shapes, excluding Ads and empty identifiers.
fn analytics_id(id: &str) -> bool {
    if id.len() > 64 {
        return false;
    }
    if let Some(id) = id.strip_prefix("G-") {
        return !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit());
    }
    id.strip_prefix("UA-")
        .and_then(|id| id.split_once('-'))
        .is_some_and(|(account, property)| {
            !account.is_empty()
                && !property.is_empty()
                && account
                    .bytes()
                    .chain(property.bytes())
                    .all(|byte| byte.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::super::{MAX_HTML_BYTES, MAX_HTML_TOKENS};
    use super::*;

    /// Executable source URLs are classified by exact host and path, with entity decoding.
    #[test]
    fn script_sources_detect_distinct_providers_and_decode_attributes() {
        assert_eq!(detect(br"
            <SCRIPT async SRC='//www.googletagmanager&#46;com/gtag/js?id=G-TEST123&amp;l=dataLayer'></SCRIPT>
            <script src='https://www.google-analytics.com/analytics.js'></script>
            <script src='https://ssl.google-analytics.com/ga.js'></script>
            <script src='https://www.googletagmanager.com/gtm.js?id=GTM-TEST'></script>
            <script type='text/javascript' src='https://mc.yandex.ru/metrika/tag.js'></script>
            <script src='https://mc.yandex.com/metrika/watch.js'></script>
        "), ["Google Analytics", "Google Tag", "Google Tag Manager", "Yandex Metrica"]);
        assert_eq!(
            detect(br"<script src='https://www.googletagmanager.com/gtag/js?id=AW-123'></script>"),
            ["Google Tag"]
        );
    }

    /// Familiar inline loaders can reference a literal URL without an external script attribute.
    #[test]
    fn recognizable_inline_loaders_detect_literal_provider_urls() {
        for (snippet, expected) in [
            (
                r"(function(i,s,o,g,r,a,m){a=s.createElement(o);a.src=g;})(window,document,'script','https://www.google-analytics.com/analytics.js','ga');",
                "Google Analytics",
            ),
            (
                r"(function(w,d,s,l,i){var j=d.createElement(s);j.src='https://www.googletagmanager.com/gtm.js?id='+i;})(window,document,'script','dataLayer','GTM-TEST');",
                "Google Tag Manager",
            ),
            (
                r#"(function(m,e,t,r,i,k,a){k=e.createElement(t);k.src=r;})(window,document,"script","https://mc.yandex.ru/metrika/tag.js","ym");"#,
                "Yandex Metrica",
            ),
        ] {
            assert_eq!(
                detect(format!("<script>{snippet}</script>").as_bytes()),
                [expected]
            );
        }
    }

    /// Only actual configuration calls with Analytics-shaped identifiers imply Google Analytics.
    #[test]
    fn gtag_config_identifies_analytics_but_not_advertising_ids() {
        for call in [
            "gtag('config', 'G-ABC123');",
            "window.gtag( 'config' , 'UA-12345-1', {} );",
        ] {
            assert_eq!(
                detect(format!("<script>{call}</script>").as_bytes()),
                ["Google Analytics", "Google Tag"]
            );
        }
        for call in [
            "gtag('config', 'AW-12345');",
            "gtag('config', 'G-');",
            "gtag('event', 'G-ABC123');",
            "obj.gtag('config', 'G-ABC123');",
        ] {
            assert!(
                detect(format!("<script>{call}</script>").as_bytes()).is_empty(),
                "{call}"
            );
        }
    }

    /// Prose, links, comments, escaped examples, JSON data, and inert templates are not tags.
    #[test]
    fn ordinary_and_inert_content_does_not_report_analytics() {
        for html in [
            r"<p>Google Analytics https://www.google-analytics.com/analytics.js gtag('config','G-ABC123')</p>",
            r"<pre>&lt;script src='https://www.google-analytics.com/analytics.js'&gt;&lt;/script&gt;</pre>",
            r"<a href='https://mc.yandex.ru/metrika/tag.js'>Documentation</a>",
            r"<!-- <script src='https://www.google-analytics.com/ga.js'></script> -->",
            r"<template><template><script src='https://www.google-analytics.com/ga.js'></script></template></template>",
            r#"<script type='application/ld+json'>{"example":"gtag('config','G-ABC123')"}</script>"#,
            r"<script type='text/plain' src='https://www.google-analytics.com/ga.js'></script>",
            r"<script>// gtag('config','G-ABC123')
            /* gtag('config','G-ABC123') */</script>",
            r#"<script>const example = "gtag('config','G-ABC123')";</script>"#,
            r"<script>const example = /gtag('config','G-ABC123')/;</script>",
            r"<script>const example = `gtag('config','G-ABC123')`;</script>",
            r"<script>const example = 'https://mc.yandex.ru/metrika/tag.js';</script>",
        ] {
            assert!(detect(html.as_bytes()).is_empty(), "{html}");
        }
    }

    /// Near-matching domains, URL userinfo, and provider names buried in query values are ignored.
    #[test]
    fn lookalike_or_unrelated_source_urls_do_not_match() {
        for source in [
            "https://google-analytics.com.evil.example/analytics.js",
            "https://notgoogle-analytics.com/analytics.js",
            "https://www.google-analytics.com@evil.example/analytics.js",
            "https://evil.example/?url=https://www.google-analytics.com/analytics.js",
            "https://www.googletagmanager.com/gtm.js.extra",
            "https://mc.yandex.ru.evil.example/metrika/tag.js",
            "https://mc.yandex.ru/unrelated.js",
            "javascript:https://www.google-analytics.com/analytics.js",
        ] {
            let html = format!("<script src='{source}'></script>");
            assert!(detect(html.as_bytes()).is_empty(), "{source}");
        }
    }

    /// A recognizable noscript counter is observed as markup, without requesting the pixel.
    #[test]
    fn noscript_yandex_pixel_is_observed_without_executing_scripts() {
        assert_eq!(detect(br"<noscript><div><img src='https://mc.yandex.ru/watch/123456?x=1&amp;y=2'></div></noscript>"), ["Yandex Metrica"]);
        for html in [
            r"<noscript><script src='https://www.google-analytics.com/analytics.js'></script></noscript>",
            r"<noscript>&lt;img src='https://mc.yandex.ru/watch/123456'&gt;</noscript>",
            r"<template><noscript><img src='https://mc.yandex.ru/watch/123456'></noscript></template>",
            r"<noscript><img src='https://mc.yandex.ru/watch/not-an-id'></noscript>",
            r"<noscript><img src='https://mc.yandex.ru.evil.example/watch/123456'></noscript>",
        ] {
            assert!(detect(html.as_bytes()).is_empty(), "{html}");
        }
    }

    /// Detection respects both the fetched-byte prefix and the token-work ceiling.
    #[test]
    fn detection_is_bounded_and_keeps_early_evidence() {
        let tag = b"<script src='https://www.google-analytics.com/analytics.js'></script>";
        let mut late = vec![b' '; MAX_HTML_BYTES];
        late.extend_from_slice(tag);
        assert!(detect(&late).is_empty());
        let mut early = tag.to_vec();
        early.resize(MAX_HTML_BYTES + 100, b' ');
        assert_eq!(detect(&early), ["Google Analytics"]);
        let mut tokens = "<i></i>".repeat(MAX_HTML_TOKENS / 2).into_bytes();
        tokens.extend_from_slice(tag);
        assert!(detect(&tokens).is_empty());
    }
}
