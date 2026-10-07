//! Small, privacy-conscious payloads for user-authored GitHub bug reports.

/// Maximum authored body size, excluding the optional screenshot and footer.
pub const MAX_BODY_BYTES: usize = 16_384;
/// Maximum sanitized text screenshot size, including a truncation notice.
pub const MAX_SCREENSHOT_BYTES: usize = 32_768;

/// Sanitizes and bounds a pre-composer text snapshot, retaining Unicode titles.
///
/// Frontends must mask image cells and omit secret editors before capture. This
/// second boundary rejects unexpected terminal-protocol data rather than trying
/// to interpret it, then applies the existing diagnostic redactor. It is not a
/// guarantee that ordinary visible text contains no personal information.
#[must_use]
pub fn sanitize_screenshot(input: &str) -> String {
    const TRUNCATED: &str = "\n[Screenshot truncated]";
    const MAX_ROWS: usize = 512;
    // Bound work before redaction too: a frontend may supply an oversized row.
    let prefix = utf8_prefix(input, MAX_SCREENSHOT_BYTES);
    if prefix.chars().any(|character| {
        (character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
            || character == '\u{10eeee}'
            || matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }) {
        return "[Screenshot omitted: terminal control data]".to_owned();
    }
    let mut truncated = prefix.len() < input.len();
    let mut normalized = String::new();
    for (index, line) in prefix.lines().enumerate() {
        if index == MAX_ROWS {
            truncated = true;
            break;
        }
        if index > 0 {
            normalized.push('\n');
        }
        normalized.push_str(&line.trim_end().replace('\t', "    "));
    }
    let mut result = crate::diagnostics::redact_diagnostic_text(&normalized);
    if result.len() > MAX_SCREENSHOT_BYTES {
        truncated = true;
    }
    if truncated {
        let prefix_len = utf8_prefix(&result, MAX_SCREENSHOT_BYTES - TRUNCATED.len()).len();
        result.truncate(prefix_len);
        // Prefer whole rows, but retain useful text when one row alone is large.
        if let Some(last_newline) = result.rfind('\n') {
            result.truncate(last_newline);
        }
        result.push_str(TRUNCATED);
    }
    result
}

/// Composes an authored body, optional text screenshot, and final environment footer.
///
/// Indented code prevents embedded Markdown fences in a screenshot from escaping
/// into the report. The controller validates authored body length separately.
#[must_use]
pub fn compose_body(body: &str, screenshot: Option<&str>, footer: &str) -> String {
    let mut result = body.trim_end().to_owned();
    if let Some(screenshot) = screenshot.filter(|text| !text.trim().is_empty()) {
        result.push_str("\n\n### ASCII screenshot\n\n");
        for (index, line) in sanitize_screenshot(screenshot).lines().enumerate() {
            if index > 0 {
                result.push('\n');
            }
            result.push_str("    ");
            result.push_str(line);
        }
    }
    result.push_str("\n\n");
    result.push_str(footer.trim());
    result
}

/// Returns a lightweight version/OS footer without a backtrace or helper probes.
#[must_use]
pub fn environment_footer() -> String {
    format!(
        "---\nYouta {}\nOS: {} ({})",
        env!("CARGO_PKG_VERSION"),
        crate::diagnostics::operating_system_summary(),
        std::env::consts::ARCH
    )
}

/// Returns a byte-bounded prefix without splitting a UTF-8 code point.
fn utf8_prefix(input: &str, max_bytes: usize) -> &str {
    &input[..input.floor_char_boundary(input.len().min(max_bytes))]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bug_report_snapshot_keeps_unicode_layout_and_redacts_common_secrets() {
        let input = "YT   Минск  \r\n\ttrack\napi_key=secret123\n/home/alice/private/song.flac";
        let result = sanitize_screenshot(input);
        assert!(result.starts_with("YT   Минск\n    track\n"));
        assert!(!result.contains("secret123"));
        assert!(!result.contains("/home/alice"));
    }

    #[test]
    fn bug_report_snapshot_omits_unexpected_terminal_payloads() {
        for payload in [
            "track\n\u{1b}_Gf=32;private-base64\nmore-payload\u{1b}\\",
            "track\u{009d}secret-title\u{009c}",
            "track\u{10eeee}\u{305}private-base64",
            "track\0hidden",
        ] {
            let result = sanitize_screenshot(payload);
            assert_eq!(result, "[Screenshot omitted: terminal control data]");
        }
    }

    #[test]
    fn bug_report_snapshot_limits_bytes_at_utf8_boundaries() {
        let result = sanitize_screenshot(&"Минск, Беларусь\n".repeat(10_000));
        assert!(result.len() <= MAX_SCREENSHOT_BYTES);
        assert!(result.starts_with("Минск, Беларусь\n"));
        assert!(result.ends_with("[Screenshot truncated]"));
        assert!(!result.contains('\u{fffd}'));
    }

    #[test]
    fn bug_report_snapshot_bounds_single_long_rows_and_markdown_overhead() {
        for input in ["я".repeat(50_000), "x\n".repeat(100_000)] {
            let result = compose_body("Report", Some(&input), "---\nYouta 1\nOS: test");
            assert!(result.len() < 60_000);
            assert!(result.contains("[Screenshot truncated]"));
        }
        assert!(!compose_body("Report", Some("\n\n"), "footer").contains("screenshot"));
    }

    #[test]
    fn bug_report_body_indents_screenshot_and_finishes_with_footer() {
        let result = compose_body(
            "My report",
            Some("YT\n```\n# not a heading"),
            "---\nYouta 1\nOS: test",
        );
        assert_eq!(
            result,
            "My report\n\n### ASCII screenshot\n\n    YT\n    ```\n    # not a heading\n\n---\nYouta 1\nOS: test"
        );
        let without = compose_body("My report", None, "---\nYouta 1\nOS: test");
        assert_eq!(without, "My report\n\n---\nYouta 1\nOS: test");
    }

    #[test]
    fn bug_report_environment_footer_is_small_and_ends_in_os_identity() {
        let footer = environment_footer();
        assert!(footer.starts_with(&format!("---\nYouta {}\nOS: ", env!("CARGO_PKG_VERSION"))));
        assert!(footer.contains(std::env::consts::ARCH));
        assert_eq!(footer.lines().count(), 3);
        assert!(footer.len() < 1024);
    }
}
