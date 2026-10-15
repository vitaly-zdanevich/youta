//! Plain-text projection of pod descriptions, retaining ordinary clickable URLs.

use html5gum::{DefaultEmitter, Token, Tokenizer};

/// Decodes declared or recognizable HTML without executing markup or showing hidden text.
pub(super) fn plain_description(value: &str, declared_html: bool) -> String {
    let looks_html = [
        "<p", "<a ", "<br", "<div", "<ul", "<ol", "<li", "<span", "<strong", "<em>",
    ]
    .iter()
    .any(|tag| value.contains(tag));
    let plain = if declared_html || looks_html {
        strip_markup(value)
    } else {
        value.to_owned()
    };
    super::display_text(&plain)
}

/// Tokenizes bounded visible content, decodes entities, and preserves anchor destinations.
fn strip_markup(value: &str) -> String {
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    let mut output = String::new();
    let mut hidden = Vec::<Vec<u8>>::new();
    let mut links = Vec::<(String, usize)>::new();
    for token in Tokenizer::new_with_emitter(value, emitter).take(65_536) {
        let Ok(token) = token;
        match token {
            Token::StartTag(tag)
                if matches!(
                    tag.name.as_slice(),
                    b"script" | b"style" | b"template" | b"noscript" | b"svg" | b"math"
                ) =>
            {
                if hidden.len() >= 64 {
                    break;
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
            Token::String(text) if hidden.is_empty() => {
                output.push_str(&String::from_utf8_lossy(text.value.as_ref()));
            }
            Token::StartTag(tag) if hidden.is_empty() && tag.name.as_slice() == b"a" => {
                if links.len() < 32 {
                    if let Some(raw) = tag.attributes.get(b"href".as_slice()) {
                        let destination = String::from_utf8_lossy(raw.as_ref());
                        if let Ok(url) = url::Url::parse(destination.trim()) {
                            if matches!(url.scheme(), "http" | "https" | "mailto")
                                && url.username().is_empty()
                                && url.password().is_none()
                            {
                                links.push((destination.into_owned(), output.len()));
                            }
                        }
                    }
                }
            }
            Token::EndTag(tag) if hidden.is_empty() && tag.name.as_slice() == b"a" => {
                if let Some((url, start)) = links.pop() {
                    if output
                        .get(start..)
                        .is_some_and(|label| label.trim() != url.trim())
                    {
                        output.push_str(" (");
                        output.push_str(url.trim());
                        output.push(')');
                    }
                }
            }
            Token::StartTag(tag)
                if hidden.is_empty()
                    && matches!(tag.name.as_slice(), b"br" | b"p" | b"div" | b"li") =>
            {
                output.push('\n')
            }
            Token::EndTag(tag)
                if hidden.is_empty() && matches!(tag.name.as_slice(), b"p" | b"div" | b"li") =>
            {
                output.push('\n')
            }
            _ => {}
        }
        if output.len() > 128 * 1024 {
            break;
        }
    }
    output
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}
