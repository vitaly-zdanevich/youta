//! Bounded imports of explicitly supplied Netscape cookies for VK requests.
//!
//! This module never discovers browser profiles or writes imported credentials.
//! Each request constructs its own domain/path-scoped header so credentials
//! cannot follow an audio CDN URL or a redirect to another site.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use url::Url;

use crate::providers::ProviderError;

/// Maximum size of an explicitly selected browser cookie export.
const MAX_COOKIE_FILE_BYTES: usize = 256 * 1024;
/// Maximum number of matching VK cookies retained in memory.
const MAX_COOKIES: usize = 512;
/// Maximum size of one cookie name, value or path.
const MAX_COOKIE_FIELD_BYTES: usize = 8 * 1024;
/// Prevents a bounded export from becoming an oversized HTTP header.
const MAX_HEADER_BYTES: usize = 32 * 1024;

/// Imported VK cookies without a printable representation of their contents.
#[derive(Clone)]
pub struct VkCookieJar {
    cookies: Vec<Cookie>,
}

#[derive(Clone)]
struct Cookie {
    domain: String,
    include_subdomains: bool,
    path: String,
    secure: bool,
    expires: u64,
    name: String,
    value: String,
}

/// A request-specific credential header whose debug output is always redacted.
pub struct VkCookieHeader(String);

impl fmt::Debug for VkCookieJar {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("VkCookieJar([REDACTED])")
    }
}

impl fmt::Debug for VkCookieHeader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("VkCookieHeader([REDACTED])")
    }
}

impl VkCookieHeader {
    /// Borrows the scoped header only when attaching it to a validated request.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl VkCookieJar {
    /// Reads one explicitly selected regular file without retaining its path.
    ///
    /// # Errors
    /// Rejects unreadable/non-regular files, oversized exports and malformed VK
    /// cookie rows. Errors contain neither the path nor file contents.
    pub fn from_file(path: &Path) -> Result<Self, ProviderError> {
        let metadata = path.metadata().map_err(|_| unreadable_file())?;
        if !metadata.is_file() {
            return Err(unreadable_file());
        }
        if metadata.len() > MAX_COOKIE_FILE_BYTES as u64 {
            return Err(oversized_file());
        }
        let file = File::open(path).map_err(|_| unreadable_file())?;
        if !file.metadata().map_err(|_| unreadable_file())?.is_file() {
            return Err(unreadable_file());
        }
        let mut bytes = Vec::new();
        file.take((MAX_COOKIE_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| unreadable_file())?;
        Self::parse(&bytes)
    }

    /// Imports Netscape rows while discarding all cookies for unrelated domains.
    ///
    /// A standard `#HttpOnly_` prefix is supported. Expiration zero or an empty
    /// expiration field denotes a session cookie, matching Mozilla cookie-jar
    /// exports. Expiration is evaluated again before each request.
    ///
    /// # Errors
    /// Rejects invalid UTF-8, invalid VK cookie fields and excessive input. No
    /// cookie names or values are included in an error.
    pub fn parse(bytes: &[u8]) -> Result<Self, ProviderError> {
        if bytes.len() > MAX_COOKIE_FILE_BYTES {
            return Err(oversized_file());
        }
        let text = std::str::from_utf8(bytes).map_err(|_| malformed_cookie())?;
        let mut cookies = Vec::new();
        for line in text.lines() {
            let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let domain = line.split('\t').next().unwrap_or_default();
            let domain = domain
                .strip_prefix('.')
                .unwrap_or(domain)
                .to_ascii_lowercase();
            if !first_party_host(&domain) {
                continue;
            }
            let fields = line.split('\t').collect::<Vec<_>>();
            if fields.len() != 7
                || fields.iter().any(|field| {
                    field.len() > MAX_COOKIE_FIELD_BYTES || field.chars().any(char::is_control)
                })
                || !valid_domain(&domain)
                || !fields[2].starts_with('/')
                || !fields[2].is_ascii()
                || !fields[5].bytes().all(cookie_name_byte)
                || fields[5].is_empty()
                || !fields[6].bytes().all(cookie_value_byte)
            {
                return Err(malformed_cookie());
            }
            let include_subdomains = parse_flag(fields[1])?;
            let secure = parse_flag(fields[3])?;
            let expires = if fields[4].is_empty() {
                0
            } else {
                fields[4].parse::<u64>().map_err(|_| malformed_cookie())?
            };
            if cookies.len() >= MAX_COOKIES {
                return Err(malformed_cookie());
            }
            cookies.push(Cookie {
                domain,
                include_subdomains,
                path: fields[2].to_owned(),
                secure,
                expires,
                name: fields[5].to_owned(),
                value: fields[6].to_owned(),
            });
        }
        if cookies.is_empty() {
            return Err(ProviderError::InvalidRequest(
                "VK cookie file contains no VK cookies".to_owned(),
            ));
        }
        Ok(Self { cookies })
    }

    /// Selects unexpired cookies for one HTTPS VK host and request path.
    ///
    /// Cookies with longer paths precede less-specific cookies, preserving file
    /// order for equal paths. No header is returned if no live cookie matches.
    /// Call this again for each permitted first-party redirect; never reuse a
    /// header when resolving or fetching a third-party audio resource.
    ///
    /// # Errors
    /// Rejects non-VK hosts, user information, fragments, non-HTTPS URLs,
    /// nonstandard ports and oversized matching headers.
    pub fn request_header(
        &self,
        url: &Url,
        now_unix_seconds: u64,
    ) -> Result<Option<VkCookieHeader>, ProviderError> {
        let host = url.host_str().unwrap_or_default();
        if url.scheme() != "https"
            || !first_party_host(host)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.port_or_known_default() != Some(443)
        {
            return Err(ProviderError::InvalidRequest(
                "VK cookies require a first-party HTTPS request".to_owned(),
            ));
        }
        let mut matching = self
            .cookies
            .iter()
            .filter(|cookie| {
                (cookie.expires == 0 || cookie.expires > now_unix_seconds)
                    && (!cookie.secure || url.scheme() == "https")
                    && (host == cookie.domain
                        || cookie.include_subdomains
                            && host
                                .strip_suffix(&cookie.domain)
                                .is_some_and(|prefix| prefix.ends_with('.')))
                    && path_matches(url.path(), &cookie.path)
            })
            .collect::<Vec<_>>();
        matching.sort_by_key(|cookie| std::cmp::Reverse(cookie.path.len()));
        let mut header = String::new();
        for cookie in matching {
            let separator = if header.is_empty() { "" } else { "; " };
            if header.len() + separator.len() + cookie.name.len() + 1 + cookie.value.len()
                > MAX_HEADER_BYTES
            {
                return Err(ProviderError::InvalidRequest(
                    "VK cookie header exceeds its size limit".to_owned(),
                ));
            }
            header.push_str(separator);
            header.push_str(&cookie.name);
            header.push('=');
            header.push_str(&cookie.value);
        }
        Ok((!header.is_empty()).then_some(VkCookieHeader(header)))
    }
}

/// Limits cookie use to VK's first-party DNS suffixes, excluding its media CDN.
fn first_party_host(host: &str) -> bool {
    ["vk.com", "vk.ru"].iter().any(|root| {
        host == *root
            || host
                .strip_suffix(root)
                .is_some_and(|prefix| prefix.ends_with('.'))
    })
}

/// Rejects invalid DNS labels even when their suffix resembles an allowed host.
fn valid_domain(domain: &str) -> bool {
    domain.len() <= 253
        && domain.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// Applies the slash-boundary path rule from HTTP cookie matching.
fn path_matches(request: &str, cookie: &str) -> bool {
    request == cookie
        || request
            .strip_prefix(cookie)
            .is_some_and(|suffix| cookie.ends_with('/') || suffix.starts_with('/'))
}

fn parse_flag(flag: &str) -> Result<bool, ProviderError> {
    match flag {
        "TRUE" => Ok(true),
        "FALSE" => Ok(false),
        _ => Err(malformed_cookie()),
    }
}

/// HTTP token bytes avoid separators and controls in cookie names.
fn cookie_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// RFC 6265 cookie-octets cannot introduce another header or cookie pair.
fn cookie_value_byte(byte: u8) -> bool {
    matches!(byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e)
}

fn unreadable_file() -> ProviderError {
    ProviderError::InvalidRequest("VK cookie file cannot be read as a regular file".to_owned())
}

fn oversized_file() -> ProviderError {
    ProviderError::InvalidRequest("VK cookie file exceeds the 256 KiB limit".to_owned())
}

fn malformed_cookie() -> ProviderError {
    ProviderError::InvalidRequest("VK cookie file contains an invalid Netscape cookie".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(jar: &VkCookieJar, url: &str, now: u64) -> Option<String> {
        jar.request_header(&Url::parse(url).unwrap(), now)
            .unwrap()
            .map(|header| header.as_str().to_owned())
    }

    #[test]
    fn imports_only_vk_cookies_and_honors_host_only_and_domain_scope() {
        let jar = VkCookieJar::parse(
            concat!(
                "# Netscape HTTP Cookie File\n",
                ".example.org\tTRUE\t/\tTRUE\t0\tother\tignored\n",
                "vk.ru\tFALSE\t/\tTRUE\t0\thost\troot\n",
                "#HttpOnly_.vk.ru\tTRUE\t/\tTRUE\t0\tdomain\tchildren\n",
                ".vk.com\tTRUE\t/\tTRUE\t0\tlegacy\tcom\n",
                "m.vk.ru\tFALSE\t/\tTRUE\t0\tmobile\tonly\n"
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            header(&jar, "https://vk.ru/audio", 1).as_deref(),
            Some("host=root; domain=children")
        );
        assert_eq!(
            header(&jar, "https://m.vk.ru/audio", 1).as_deref(),
            Some("domain=children; mobile=only")
        );
        assert_eq!(
            header(&jar, "https://vk.com/audio", 1).as_deref(),
            Some("legacy=com")
        );
    }

    #[test]
    fn matches_cookie_paths_and_orders_specific_values_before_root_values() {
        let jar = VkCookieJar::parse(
            concat!(
                ".vk.ru\tTRUE\t/\tTRUE\t0\tsid\troot\n",
                ".vk.ru\tTRUE\t/audio\tTRUE\t0\tsid\taudio\n",
                ".vk.ru\tTRUE\t/audio/\tTRUE\t0\tsid\tchild\n"
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            header(&jar, "https://vk.ru/audio", 1).as_deref(),
            Some("sid=audio; sid=root")
        );
        assert_eq!(
            header(&jar, "https://vk.ru/audio/track", 1).as_deref(),
            Some("sid=child; sid=audio; sid=root")
        );
        assert_eq!(
            header(&jar, "https://vk.ru/audios", 1).as_deref(),
            Some("sid=root")
        );
    }

    #[test]
    fn reevaluates_expiry_and_retains_session_cookies() {
        let jar = VkCookieJar::parse(
            concat!(
                "vk.ru\tFALSE\t/\tTRUE\t10\ttimed\tfixture\n",
                "vk.ru\tFALSE\t/\tFALSE\t0\tsession\tfixture\n"
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            header(&jar, "https://vk.ru/", 9).as_deref(),
            Some("timed=fixture; session=fixture")
        );
        assert_eq!(
            header(&jar, "https://vk.ru/", 10).as_deref(),
            Some("session=fixture")
        );
        assert!(header(&jar, "https://vk.com/", 10).is_none());
    }

    #[test]
    fn accepts_blank_session_expiry_without_reclassifying_negative_timestamps() {
        let jar = VkCookieJar::parse(b"vk.ru\tFALSE\t/\tTRUE\t\tsession\tfixture\n").unwrap();
        assert_eq!(
            header(&jar, "https://vk.ru/", u64::MAX).as_deref(),
            Some("session=fixture")
        );
        assert!(VkCookieJar::parse(b"vk.ru\tFALSE\t/\tTRUE\t-1\tsession\tfixture\n").is_err());
    }

    #[test]
    fn refuses_to_send_any_cookie_to_insecure_third_party_or_credentialed_targets() {
        let jar = VkCookieJar::parse(b".vk.ru\tTRUE\t/\tTRUE\t0\tsid\tfixture\n").unwrap();
        for target in [
            "http://vk.ru/",
            "https://vkuseraudio.net/",
            "https://evilvk.ru/",
            "https://vk.ru.example.test/",
            "https://user@vk.ru/",
            "https://vk.ru:444/",
            "https://vk.ru/#fragment",
            "https://127.0.0.1/",
            "https://vk.ru./",
        ] {
            assert!(
                jar.request_header(&Url::parse(target).unwrap(), 1).is_err(),
                "{target}"
            );
        }
    }

    #[test]
    fn rejects_header_injection_and_never_prints_credential_contents() {
        for value in [
            "secret\rInjected",
            "secret\0",
            "secret; other=1",
            "secret,value",
            "secret value",
            "secret\\value",
        ] {
            let row = format!("vk.ru\tFALSE\t/\tTRUE\t0\tsid\t{value}\n");
            let error = VkCookieJar::parse(row.as_bytes()).unwrap_err();
            assert!(!error.to_string().contains("secret"));
        }
        let jar = VkCookieJar::parse(b"vk.ru\tFALSE\t/\tTRUE\t0\tsid\tprivate-fixture\n").unwrap();
        let header = jar
            .request_header(&Url::parse("https://vk.ru/").unwrap(), 1)
            .unwrap()
            .unwrap();
        assert!(!format!("{jar:?} {header:?}").contains("private-fixture"));
    }

    #[test]
    fn rejects_malformed_vk_fields_and_accepts_crlf_cookie_exports() {
        for row in [
            "vk.ru\tFALSE\t/\tTRUE\t-1\tsid\tvalue",
            "vk.ru\tFALSE\t/\ttrue\t0\tsid\tvalue",
            "vk.ru\tFALSE\tnot-a-path\tTRUE\t0\tsid\tvalue",
            "vk.ru\tFALSE\t/\tTRUE\t0\tbad=name\tvalue",
            "vk.ru\tFALSE\t/\tTRUE\t0\tsid\tvalue\textra",
            "bad..vk.ru\tTRUE\t/\tTRUE\t0\tsid\tvalue",
        ] {
            assert!(VkCookieJar::parse(row.as_bytes()).is_err());
        }
        assert!(
            VkCookieJar::parse(
                b"# Netscape HTTP Cookie File\r\nvk.ru\tFALSE\t/\tTRUE\t0\tsid\tvalue\r\n"
            )
            .is_ok()
        );
        assert!(VkCookieJar::parse(b"example.org\tFALSE\t/\tTRUE\t0\tsid\tvalue\n").is_err());
    }

    #[test]
    fn bounds_cookie_file_cookie_count_and_request_header() {
        assert!(VkCookieJar::parse(&vec![b' '; MAX_COOKIE_FILE_BYTES + 1]).is_err());
        let rows = "vk.ru\tFALSE\t/\tTRUE\t0\tsid\tx\n".repeat(MAX_COOKIES + 1);
        assert!(VkCookieJar::parse(rows.as_bytes()).is_err());
        let rows = format!("vk.ru\tFALSE\t/\tTRUE\t0\tsid\t{}\n", "x".repeat(8 * 1024)).repeat(5);
        let jar = VkCookieJar::parse(rows.as_bytes()).unwrap();
        assert!(
            jar.request_header(&Url::parse("https://vk.ru/").unwrap(), 1)
                .is_err()
        );
    }

    #[test]
    fn reads_only_an_explicit_regular_file_and_sanitizes_filesystem_errors() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private-session-name");
        let error = VkCookieJar::from_file(&path).unwrap_err();
        assert!(!error.to_string().contains("private-session-name"));
        assert!(VkCookieJar::from_file(directory.path()).is_err());
        std::fs::write(&path, b"vk.ru\tFALSE\t/\tTRUE\t0\tsid\tfixture\n").unwrap();
        assert!(VkCookieJar::from_file(&path).is_ok());
    }
}
