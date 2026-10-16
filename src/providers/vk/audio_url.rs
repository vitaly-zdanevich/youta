//! Resolves the account-scoped URL encoding returned by authenticated VK audio.
//!
//! The validated `i` operation is the same URL permutation implemented by
//! yt-dlp's `VKWallPostIE._unmask_url`:
//! <https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/vk.py>.
//! Unknown operations are rejected rather than interpreted. This is not a media
//! decryption layer and does not invent alternate MP3 paths for HLS playlists.

use std::fmt;
use std::sync::LazyLock;

use base64::Engine;
use base64::alphabet::Alphabet;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use url::Url;

use crate::providers::ProviderError;

const MAX_MASKED_BYTES: usize = 16 * 1024;
const MAX_MEDIA_URL_BYTES: usize = 8 * 1024;
const MAX_OPERATION_BYTES: usize = 64;

/// VK uses a shuffled alphabet, including a swapped capital O and digit zero.
static VK_BASE64: LazyLock<GeneralPurpose> = LazyLock::new(|| {
    let alphabet =
        Alphabet::new("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN0PQRSTUVWXYZO123456789+/")
            .expect("fixed VK alphabet");
    GeneralPurpose::new(
        &alphabet,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    )
});

/// A transient signed audio locator that must not enter persisted metadata.
///
/// Debug output is deliberately redacted. Cookie headers are never attached to
/// this CDN URL: the authenticated first-party lookup has already resolved it.
#[derive(Clone, Eq, PartialEq)]
pub struct VkPlaybackUrl(Url);

impl fmt::Debug for VkPlaybackUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("VkPlaybackUrl([REDACTED])")
    }
}

impl VkPlaybackUrl {
    /// Borrows the signed locator only for a bounded media request or playback.
    #[must_use]
    pub fn as_url(&self) -> &Url {
        &self.0
    }

    /// Transfers the transient locator to a playback request, never to history.
    #[must_use]
    pub fn into_url(self) -> Url {
        self.0
    }
}

/// Decodes one validated account-scoped VK audio URL or accepts a direct CDN URL.
///
/// Only the observed single `i` permutation is supported. Output is limited to
/// credential-free HTTPS URLs on `vkuseraudio.ru`, `.net` and `.com`; signed query
/// fields are retained exactly as parsed, without converting HLS into guessed MP3.
/// The account ID is used only in memory and never included in errors.
///
/// # Errors
/// Rejects oversized/malformed locators, invalid account IDs, unknown encoding
/// operations, and decoded URLs outside the permitted audio CDN origins.
pub fn decode_audio_url(raw: &str, account_id: u64) -> Result<VkPlaybackUrl, ProviderError> {
    if raw.len() > MAX_MASKED_BYTES
        || !printable_ascii(raw.as_bytes())
        || account_id == 0
        || account_id > i64::MAX as u64
    {
        return Err(invalid_locator());
    }
    let outer = Url::parse(raw).map_err(|_| invalid_locator())?;
    if outer.path() != "/mp3/audio_api_unavailable.mp3" {
        return validate_media_url(outer);
    }
    if outer.scheme() != "https"
        || !matches!(
            outer.host_str(),
            Some("vk.ru" | "vk.com" | "m.vk.ru" | "m.vk.com" | "www.vk.ru" | "www.vk.com")
        )
        || !outer.username().is_empty()
        || outer.password().is_some()
        || outer.port_or_known_default() != Some(443)
    {
        return Err(invalid_locator());
    }
    // These are custom Base64 strings, not form values: `+` must remain `+`.
    let encoded = outer
        .query()
        .and_then(|query| query.strip_prefix("extra="))
        .ok_or_else(invalid_locator)?;
    let operation = decode_base64(
        outer.fragment().ok_or_else(invalid_locator)?,
        MAX_OPERATION_BYTES,
    )?;
    let operation = std::str::from_utf8(&operation).map_err(|_| unsupported_operation())?;
    let (mode, parameter) = operation
        .split_once('\u{b}')
        .ok_or_else(unsupported_operation)?;
    if mode != "i" || parameter.is_empty() || !parameter.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(unsupported_operation());
    }
    let parameter = parameter
        .parse::<u32>()
        .map_err(|_| unsupported_operation())?;
    let mut bytes = decode_base64(encoded, MAX_MEDIA_URL_BYTES)?;
    if bytes.is_empty() || !printable_ascii(&bytes) {
        return Err(invalid_locator());
    }
    unshuffle(&mut bytes, u64::from(parameter) ^ account_id);
    let decoded = std::str::from_utf8(&bytes).map_err(|_| invalid_locator())?;
    validate_media_url(Url::parse(decoded).map_err(|_| invalid_locator())?)
}

/// Bounds allocation before decoding and never includes encoded input in errors.
fn decode_base64(encoded: &str, limit: usize) -> Result<Vec<u8>, ProviderError> {
    if encoded.is_empty() || encoded.len() > limit.div_ceil(3) * 4 {
        return Err(invalid_locator());
    }
    let decoded = VK_BASE64.decode(encoded).map_err(|_| invalid_locator())?;
    if decoded.len() > limit {
        return Err(invalid_locator());
    }
    Ok(decoded)
}

/// Applies the bounded account-XOR permutation without quadratic string copies.
fn unshuffle(bytes: &mut [u8], mut seed: u64) {
    let length = bytes.len();
    let mut indexes = vec![0; length];
    for index in (0..length).rev() {
        seed = ((length as u64 * (index as u64 + 1)) ^ (seed + index as u64)) % length as u64;
        // The modulo makes the cast bounded by the at-most-8-KiB input length.
        indexes[index] = usize::try_from(seed).expect("bounded permutation index");
    }
    for index in 1..length {
        bytes.swap(index, indexes[length - 1 - index]);
    }
}

/// Allows only literal, printable URL text rather than URL-parser whitespace repair.
fn printable_ascii(bytes: &[u8]) -> bool {
    bytes.iter().all(|byte| (0x21..=0x7e).contains(byte))
}

/// Enforces the same CDN-origin policy for already-clear and decoded locators.
fn validate_media_url(url: Url) -> Result<VkPlaybackUrl, ProviderError> {
    let host = url.host_str().unwrap_or_default();
    let trusted_host = ["vkuseraudio.ru", "vkuseraudio.net", "vkuseraudio.com"]
        .iter()
        .any(|root| {
            host == *root
                || host
                    .strip_suffix(root)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        });
    if url.as_str().len() > MAX_MEDIA_URL_BYTES
        || url.scheme() != "https"
        || !trusted_host
        || host.split('.').any(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default() != Some(443)
    {
        return Err(invalid_locator());
    }
    Ok(VkPlaybackUrl(url))
}

fn invalid_locator() -> ProviderError {
    ProviderError::InvalidResponse("VK returned an invalid or unsupported audio URL".to_owned())
}

fn unsupported_operation() -> ProviderError {
    ProviderError::InvalidResponse("VK returned an unsupported audio URL encoding".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic inverse-permutation fixtures independently checked by yt-dlp.
    const REFERENCE_VECTORS: &[(u64, &str, &str)] = &[
        (
            123_456,
            "https://m.vk.ru/mp3/audio_api_unavailable.mp3?extra=zt9ODdnOlxrPoNbKzhrPCMvSB3jLyxrHDw94BNvUlML1l3vWlJHPAhn2BY8VCY54BNLOzhvZDJrTEd1LzwzZlY9YywTL#AqS3nZC",
            "https://psv4.vkuseraudio.net/audio/fixture/index.m3u8?extra=test-only",
        ),
        (
            2_147_483_647,
            "https://m.vk.ru/mp3/audio_api_unavailable.mp3?extra=Ahj2yxrLDxnYzxvVB2DOEhrUl2vRyZ1YDc5ZC3uXzgLNoJ8TDs9Oy2DPBY8VAwvZCI4UAxnHB2z1Cgq#AqSW",
            "https://cs1.vkuseraudio.ru/audio/fixture.ogg?sig=not-secret",
        ),
        (
            1,
            "https://m.vk.ru/mp3/audio_api_unavailable.mp3?extra=Ds9PChnVBY9YyY5YA3zKAc91zxrWBwf1CZnLDg14DdPPzI4#AqSXmdiZ",
            "https://vkuseraudio.com/fixture.mp3",
        ),
    ];

    #[test]
    fn matches_reference_decoder_without_rewriting_hls_or_signed_queries() {
        for &(account, encoded, expected) in REFERENCE_VECTORS {
            let locator = decode_audio_url(encoded, account).unwrap();
            assert_eq!(locator.as_url().as_str(), expected);
            assert_eq!(locator.into_url().as_str(), expected);
        }
    }

    #[test]
    fn accepts_direct_https_audio_cdns_but_never_sends_them_cookie_headers() {
        let jar =
            super::super::VkCookieJar::parse(b"vk.ru\tFALSE\t/\tTRUE\t0\tsid\tfixture\n").unwrap();
        for root in ["vkuseraudio.ru", "vkuseraudio.net", "vkuseraudio.com"] {
            for host in [root.to_owned(), format!("psv4.{root}")] {
                let direct = format!("https://{host}/fixture/index.m3u8?sig=fixture");
                let decoded = decode_audio_url(&direct, 123).unwrap();
                assert_eq!(decoded.as_url().as_str(), direct);
                assert!(jar.request_header(decoded.as_url(), 0).is_err());
            }
        }
    }

    #[test]
    fn rejects_unsafe_direct_audio_targets_without_echoing_input() {
        for direct in [
            "http://psv4.vkuseraudio.ru/private-fixture",
            "https://psv4.vkuseraudio.ru.example.test/private-fixture",
            "https://evilvkuseraudio.ru/private-fixture",
            "https://vk.ru/private-fixture",
            "https://127.0.0.1/private-fixture",
            "https://bad..vkuseraudio.ru/private-fixture",
            "https://private-fixture@psv4.vkuseraudio.ru/track",
            "https://psv4.vkuseraudio.ru:444/private-fixture",
            "https://psv4.vkuseraudio.ru/track#private-fixture",
            "https://psv4.vkuseraudio.ru/track\nprivate-fixture",
            " https://psv4.vkuseraudio.ru/private-fixture",
        ] {
            let error = decode_audio_url(direct, 123).unwrap_err();
            assert!(!error.to_string().contains("private-fixture"));
        }
    }

    #[test]
    fn rejects_unknown_and_chained_operations_instead_of_guessing() {
        let (_, masked, _) = REFERENCE_VECTORS[0];
        let (prefix, _) = masked.split_once('#').unwrap();
        for operation in [
            "s\u{b}777",
            "x\u{b}777",
            "i\u{b}-1",
            "i\u{b}",
            "i\u{b}777\ti\u{b}777",
            "i\u{b}777\u{b}extra",
            "i\u{b}4294967296",
        ] {
            let input = format!("{prefix}#{}", VK_BASE64.encode(operation));
            assert!(decode_audio_url(&input, 123_456).is_err());
        }
    }

    #[test]
    fn rejects_malformed_masks_and_account_mismatches() {
        let (account, masked, _) = REFERENCE_VECTORS[0];
        for input in [
            masked.replace("https://m.vk.ru", "http://m.vk.ru"),
            masked.replace("m.vk.ru", "m.vk.ru.example.test"),
            masked.replace("?extra=", "?extra=&second="),
            masked.replace("?extra=", "?extra=%"),
            masked.split_once('#').unwrap().0.to_owned(),
            format!("{masked}&extra"),
            "https://m.vk.ru/mp3/audio_api_unavailable.mp3?extra=a#AqSW".to_owned(),
        ] {
            assert!(decode_audio_url(&input, account).is_err());
        }
        for invalid_account in [0, account + 1, u64::MAX] {
            assert!(decode_audio_url(masked, invalid_account).is_err());
        }
    }

    #[test]
    fn bounds_payloads_and_keeps_debug_output_redacted() {
        let (account, masked, expected) = REFERENCE_VECTORS[0];
        assert!(decode_audio_url(&"x".repeat(MAX_MASKED_BYTES + 1), account).is_err());
        assert!(decode_base64(&"a".repeat(MAX_MEDIA_URL_BYTES * 2), MAX_MEDIA_URL_BYTES).is_err());
        let locator = decode_audio_url(masked, account).unwrap();
        let debug = format!("{locator:?}");
        assert_eq!(debug, "VkPlaybackUrl([REDACTED])");
        assert!(!debug.contains(expected));
        assert!(!debug.contains(&account.to_string()));
    }
}
