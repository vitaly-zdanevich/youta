//! Bounded extraction of the account identity from literal VK page configuration.

use std::collections::HashSet;

use html5gum::{DefaultEmitter, Token, Tokenizer};

const MAX_HTML_BYTES: usize = 2 * 1024 * 1024;
const MAX_HTML_TOKENS: usize = 65_536;
const MAX_SCRIPT_BYTES: usize = 256 * 1024;
const MAX_SCRIPT_TOKENS: usize = 32_768;
const MAX_NESTING: usize = 64;

/// Reads one positive root `id` from a complete inline `window.vk` assignment.
///
/// Scripts are never executed. HTML examples, external scripts and inert script
/// types cannot authenticate a session. Only the numeric identity is retained;
/// unsupported, truncated or ambiguous configuration fails closed.
pub(super) fn account(html: &str) -> Option<u64> {
    if html.len() > MAX_HTML_BYTES {
        return None;
    }
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    let mut templates = 0_usize;
    let mut noscript = 0_usize;
    let mut in_script = false;
    let mut script = Vec::new();
    let mut identity = None;
    for (index, token) in Tokenizer::new_with_emitter(html.as_bytes(), emitter).enumerate() {
        if index >= MAX_HTML_TOKENS {
            return None;
        }
        let Ok(token) = token;
        match token {
            Token::StartTag(tag) => {
                match tag.name.as_slice() {
                    b"template" => templates += 1,
                    b"noscript" => noscript += 1,
                    _ => {}
                }
                if tag.name.as_slice() != b"script" || templates > 0 || noscript > 0 {
                    continue;
                }
                let kind = tag.attributes.get(b"type".as_slice());
                let javascript = kind.is_none_or(|kind| {
                    std::str::from_utf8(kind.value.as_ref()).is_ok_and(|kind| {
                        matches!(
                            kind.trim().to_ascii_lowercase().as_str(),
                            "" | "module"
                                | "text/javascript"
                                | "application/javascript"
                                | "text/ecmascript"
                                | "application/ecmascript"
                        )
                    })
                });
                in_script = javascript && !tag.attributes.contains_key(b"src".as_slice());
                script.clear();
            }
            Token::EndTag(tag) => match tag.name.as_slice() {
                b"template" => templates = templates.saturating_sub(1),
                b"noscript" => noscript = noscript.saturating_sub(1),
                b"script" if in_script => {
                    if let Some(found) = script_account(&script).ok()? {
                        if identity.replace(found).is_some() {
                            return None;
                        }
                    }
                    in_script = false;
                    script.clear();
                }
                _ => {}
            },
            Token::String(value) if in_script => {
                let bytes = value.value.as_slice();
                if bytes.len() > MAX_SCRIPT_BYTES.saturating_sub(script.len()) {
                    return None;
                }
                script.extend_from_slice(bytes);
            }
            _ => {}
        }
    }
    if in_script { None } else { identity }
}

/// Lexical categories preserve boundaries without decoding or executing JavaScript.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Lexeme<'a> {
    Word(&'a [u8]),
    Number(&'a [u8]),
    Quoted(&'a [u8]),
    Punct(u8),
    Opaque,
}

/// Finds exact assignments while keeping strings, comments and regexes inert.
fn script_account(script: &[u8]) -> Result<Option<u64>, ()> {
    let mut lexer = Lexer::new(script, 0);
    let mut tokens = Vec::new();
    while let Some(token) = lexer.next()? {
        if tokens.len() >= MAX_SCRIPT_TOKENS {
            return Err(());
        }
        tokens.push(token);
    }
    let mut identity = None;
    for index in 0..tokens.len() {
        if tokens.get(index..index + 4)
            != Some(&[
                Lexeme::Word(b"window"),
                Lexeme::Punct(b'.'),
                Lexeme::Word(b"vk"),
                Lexeme::Punct(b'='),
            ])
            || index > 0
                && matches!(
                    tokens[index - 1],
                    Lexeme::Punct(b'.') | Lexeme::Word(_) | Lexeme::Number(_)
                )
            || tokens.get(index + 4) == Some(&Lexeme::Punct(b'='))
        {
            continue;
        }
        if identity.is_some() {
            return Err(());
        }
        let (found, end) = literal_identity(&tokens, index + 4)?;
        if tokens
            .get(end)
            .is_some_and(|token| *token != Lexeme::Punct(b';'))
        {
            return Err(());
        }
        identity = Some(found);
    }
    Ok(identity)
}

/// Inspects root keys and balanced values, retaining only a literal decimal ID.
fn literal_identity(tokens: &[Lexeme<'_>], mut index: usize) -> Result<(u64, usize), ()> {
    if tokens.get(index) != Some(&Lexeme::Punct(b'{')) {
        return Err(());
    }
    index += 1;
    let mut identity = None;
    let mut keys = HashSet::new();
    loop {
        if tokens.get(index) == Some(&Lexeme::Punct(b'}')) {
            return identity.map(|id| (id, index + 1)).ok_or(());
        }
        let key = match tokens.get(index) {
            Some(Lexeme::Word(key)) => *key,
            Some(Lexeme::Quoted(key)) if !key.contains(&b'\\') => *key,
            _ => return Err(()),
        };
        if !keys.insert(key) || tokens.get(index + 1) != Some(&Lexeme::Punct(b':')) {
            return Err(());
        }
        index += 2;
        let start = index;
        let mut nesting = Vec::new();
        loop {
            match tokens.get(index).ok_or(())? {
                Lexeme::Punct(b'}' | b',') if nesting.is_empty() => break,
                Lexeme::Punct(open @ (b'(' | b'[' | b'{')) => {
                    if nesting.len() >= MAX_NESTING {
                        return Err(());
                    }
                    nesting.push(*open);
                }
                Lexeme::Punct(close @ (b')' | b']' | b'}')) => {
                    if !matching(nesting.pop().ok_or(())?, *close) {
                        return Err(());
                    }
                }
                Lexeme::Opaque => return Err(()),
                _ => {}
            }
            index += 1;
        }
        if index == start {
            return Err(());
        }
        if key == b"id" {
            let [Lexeme::Number(digits)] = &tokens[start..index] else {
                return Err(());
            };
            if digits.is_empty() || digits[0] == b'0' || !digits.iter().all(u8::is_ascii_digit) {
                return Err(());
            }
            identity = std::str::from_utf8(digits)
                .ok()
                .and_then(|digits| digits.parse::<u64>().ok())
                .filter(|id| i64::try_from(*id).is_ok());
            if identity.is_none() {
                return Err(());
            }
        }
        if tokens[index] == Lexeme::Punct(b'}') {
            return identity.map(|id| (id, index + 1)).ok_or(());
        }
        index += 1;
    }
}

/// Advances over lexical literals only; no value or property is evaluated.
struct Lexer<'a> {
    bytes: &'a [u8],
    position: usize,
    depth: usize,
    regex_allowed: bool,
    after_closing_brace: bool,
    previous_word: Option<&'a [u8]>,
    control_parentheses: Vec<bool>,
}

impl<'a> Lexer<'a> {
    fn new(bytes: &'a [u8], depth: usize) -> Self {
        Self {
            bytes,
            position: 0,
            depth,
            regex_allowed: true,
            after_closing_brace: false,
            previous_word: None,
            control_parentheses: Vec::new(),
        }
    }

    fn next(&mut self) -> Result<Option<Lexeme<'a>>, ()> {
        self.trivia()?;
        let Some(&byte) = self.bytes.get(self.position) else {
            return Ok(None);
        };
        // Distinguishing a block ending from an object-expression ending requires
        // JavaScript grammar. Do not guess whether the following slash is division
        // or a regex and accidentally expose an assignment inside a regex literal.
        if byte == b'/' && self.after_closing_brace {
            return Err(());
        }
        let start = self.position;
        let token =
            match byte {
                b'\'' | b'"' => {
                    self.quoted(byte)?;
                    Lexeme::Quoted(&self.bytes[start + 1..self.position - 1])
                }
                b'`' => {
                    self.template()?;
                    Lexeme::Opaque
                }
                b'/' if self.regex_allowed => {
                    self.regex()?;
                    Lexeme::Opaque
                }
                byte if byte.is_ascii_digit() => {
                    self.position += 1;
                    while self.bytes.get(self.position).is_some_and(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_')
                    }) {
                        self.position += 1;
                    }
                    Lexeme::Number(&self.bytes[start..self.position])
                }
                byte if identifier(byte) => {
                    self.position += 1;
                    while self
                        .bytes
                        .get(self.position)
                        .is_some_and(|byte| identifier(*byte) || byte.is_ascii_digit())
                    {
                        self.position += 1;
                    }
                    Lexeme::Word(&self.bytes[start..self.position])
                }
                _ => {
                    self.position += 1;
                    Lexeme::Punct(byte)
                }
            };
        self.regex_allowed = match token {
            Lexeme::Word(word) => matches!(
                word,
                b"return"
                    | b"throw"
                    | b"case"
                    | b"delete"
                    | b"void"
                    | b"typeof"
                    | b"instanceof"
                    | b"in"
                    | b"of"
                    | b"new"
                    | b"yield"
                    | b"await"
                    | b"else"
                    | b"do"
            ),
            Lexeme::Punct(b'(') => {
                if self.control_parentheses.len() >= MAX_NESTING {
                    return Err(());
                }
                self.control_parentheses
                    .push(self.previous_word.is_some_and(|word| {
                        matches!(
                            word,
                            b"if" | b"while" | b"for" | b"with" | b"switch" | b"catch"
                        )
                    }));
                true
            }
            Lexeme::Punct(b')') => self.control_parentheses.pop().unwrap_or(false),
            Lexeme::Punct(b']' | b'.') | Lexeme::Number(_) | Lexeme::Quoted(_) | Lexeme::Opaque => {
                false
            }
            Lexeme::Punct(_) => true,
        };
        self.previous_word = if let Lexeme::Word(word) = token {
            Some(word)
        } else {
            None
        };
        self.after_closing_brace = token == Lexeme::Punct(b'}');
        Ok(Some(token))
    }

    fn trivia(&mut self) -> Result<(), ()> {
        loop {
            while self
                .bytes
                .get(self.position)
                .is_some_and(u8::is_ascii_whitespace)
            {
                self.position += 1;
            }
            let remaining = &self.bytes[self.position..];
            if remaining.starts_with(b"//")
                || remaining.starts_with(b"<!--")
                || remaining.starts_with(b"-->")
            {
                self.position += remaining
                    .iter()
                    .position(|byte| matches!(byte, b'\r' | b'\n'))
                    .unwrap_or(remaining.len());
            } else if remaining.starts_with(b"/*") {
                self.position += 2;
                self.position += self.bytes[self.position..]
                    .windows(2)
                    .position(|pair| pair == b"*/")
                    .ok_or(())?
                    + 2;
            } else {
                return Ok(());
            }
        }
    }

    fn quoted(&mut self, quote: u8) -> Result<(), ()> {
        self.position += 1;
        loop {
            match *self.bytes.get(self.position).ok_or(())? {
                byte if byte == quote => {
                    self.position += 1;
                    return Ok(());
                }
                b'\\' => {
                    self.bytes.get(self.position + 1).ok_or(())?;
                    self.position += 2;
                }
                b'\r' | b'\n' => return Err(()),
                _ => self.position += 1,
            }
        }
    }

    fn regex(&mut self) -> Result<(), ()> {
        self.position += 1;
        let mut class = false;
        loop {
            match *self.bytes.get(self.position).ok_or(())? {
                b'\\' => {
                    self.bytes.get(self.position + 1).ok_or(())?;
                    self.position += 2;
                    continue;
                }
                b'[' => class = true,
                b']' => class = false,
                b'/' if !class => {
                    self.position += 1;
                    while self
                        .bytes
                        .get(self.position)
                        .is_some_and(u8::is_ascii_alphabetic)
                    {
                        self.position += 1;
                    }
                    return Ok(());
                }
                b'\r' | b'\n' => return Err(()),
                _ => {}
            }
            self.position += 1;
        }
    }

    fn template(&mut self) -> Result<(), ()> {
        if self.depth >= MAX_NESTING {
            return Err(());
        }
        self.position += 1;
        loop {
            match *self.bytes.get(self.position).ok_or(())? {
                b'`' => {
                    self.position += 1;
                    return Ok(());
                }
                b'\\' => {
                    self.bytes.get(self.position + 1).ok_or(())?;
                    self.position += 2;
                }
                b'$' if self.bytes.get(self.position + 1) == Some(&b'{') => {
                    self.position += 2;
                    let mut inner = Self::new(&self.bytes[self.position..], self.depth + 1);
                    let mut nesting = Vec::new();
                    loop {
                        match inner.next()?.ok_or(())? {
                            Lexeme::Punct(b'}') if nesting.is_empty() => break,
                            Lexeme::Punct(open @ (b'(' | b'[' | b'{')) => {
                                if nesting.len() >= MAX_NESTING {
                                    return Err(());
                                }
                                nesting.push(open);
                            }
                            Lexeme::Punct(close @ (b')' | b']' | b'}')) => {
                                if !matching(nesting.pop().ok_or(())?, close) {
                                    return Err(());
                                }
                            }
                            _ => {}
                        }
                    }
                    self.position += inner.position;
                }
                _ => self.position += 1,
            }
        }
    }
}

fn identifier(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$') || !byte.is_ascii()
}

fn matching(open: u8, close: u8) -> bool {
    matches!((open, close), (b'(', b')') | (b'[', b']') | (b'{', b'}'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(script: &str) -> String {
        format!("<html><head><script>{script}</script></head></html>")
    }

    #[test]
    fn accepts_mobile_json_and_desktop_literal_without_executing_code() {
        for script in [
            r#"window.vk = {"id":123,"ignored":{"id":0}}; doNotExecute();"#,
            "window.vk = {first: false, id: 123, nested: {id: 456}, last: true,};",
            "window /* comment */ . vk = { 'id' /* key */ : /* value */ 123, other: [1, {id: 0}] };",
            "window.vk && window.vk.id; window.vk={id:123};",
        ] {
            assert_eq!(account(&page(script)), Some(123), "{script}");
        }
    }

    #[test]
    fn ignores_inert_html_and_javascript_examples() {
        for html in [
            "window.vk={id:123};",
            "<p>window.vk={id:123};</p>",
            "<!-- <script>window.vk={id:123};</script> -->",
            "<div data-example='window.vk={id:123};'></div>",
            "<template><script>window.vk={id:123};</script></template>",
            "<noscript><script>window.vk={id:123};</script></noscript>",
            "<script src='/config.js'>window.vk={id:123};</script>",
            "<script type='application/json'>window.vk={id:123};</script>",
            "<script>/* window.vk={id:123}; */</script>",
            "<script>// window.vk={id:123};\n</script>",
            "<script>const example = 'window.vk={id:123};';</script>",
            "<script>const example = `window.vk={id:123};`;</script>",
            "<script>const example = /window.vk={id:123};/;</script>",
            "<script>otherwindow.vk={id:123};</script>",
            "<script>other.window.vk={id:123};</script>",
            "<script>window.vkExtra={id:123};</script>",
            "<script>window.vk=={id:123};</script>",
        ] {
            assert_eq!(account(html), None, "{html}");
        }
        assert_eq!(
            account(&format!(
                "<script>const example = 'window.vk={{id:999}};';</script>{}",
                page("window.vk={id:123};")
            )),
            Some(123)
        );
    }

    #[test]
    fn rejects_ambiguous_regex_division_after_a_closing_brace() {
        // The first slash is division, but the next pair delimits an inert regex.
        // Treating the first pair as a regex would expose its example assignment.
        let script = "const ratio = {} / 1; /a;window.vk={id:123};/; const tail = 1 / 2;";
        assert_eq!(account(&page(script)), None);
    }

    #[test]
    fn requires_one_positive_literal_root_identity() {
        for script in [
            "window.vk={id:0};",
            "window.vk={id:-1};",
            "window.vk={id:'123'};",
            "window.vk={id:0123};",
            "window.vk={id:123.0};",
            "window.vk={id:123e0};",
            "window.vk={id:0x7b};",
            "window.vk={id:123n};",
            "window.vk={id:123+1};",
            "window.vk={id:accountId};",
            "window.vk={id:9223372036854775808};",
            "window.vk={nested:{id:123},note:'id:123'};",
            "window.vk={id:123,id:123};",
            "window.vk={id:123,'id':456};",
            "window.vk={id:123, ['id']:456};",
            "window.vk={id:123, get id(){return 456;}};",
            "window.vk={id:123,...other};",
            r#"window.vk={id:123,"\u0069d":456};"#,
            "window.vk={id:123};window.vk={id:456};",
            "window.vk={id:123};window.vk=other;",
            "window.vk={id:123} || {id:456};",
            "window.vk=readConfig();",
        ] {
            assert_eq!(account(&page(script)), None, "{script}");
        }
        assert_eq!(
            account(&format!(
                "{}{}",
                page("window.vk={id:123};"),
                page("window.vk={id:123};")
            )),
            None
        );
    }

    #[test]
    fn rejects_truncated_malformed_and_oversized_configuration() {
        for html in [
            "<script>window.vk={id:123};",
            "<script>window.vk={id:123, value:'unterminated};</script>",
            "<script>window.vk={id:123, value:/* unterminated};</script>",
            "<script>window.vk={id:123, value:[1,2)};</script>",
            "<script>window.vk={id:123, value:};</script>",
            "<script>window.vk={id:123</script>",
        ] {
            assert_eq!(account(html), None, "{html}");
        }
        assert_eq!(
            account(&page(&format!(
                "window.vk={{id:123,value:'{}'}};",
                "x".repeat(MAX_SCRIPT_BYTES)
            ))),
            None
        );
        assert_eq!(
            account(&format!(
                "{}{}",
                page("window.vk={id:123};"),
                " ".repeat(MAX_HTML_BYTES)
            )),
            None
        );
        assert_eq!(
            account(&page(&format!(
                "window.vk={{id:123,value:{}0{}}};",
                "[".repeat(MAX_NESTING + 1),
                "]".repeat(MAX_NESTING + 1)
            ))),
            None
        );
    }
}
