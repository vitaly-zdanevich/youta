//! Conservative, display-only repair of Windows-1251 decoded as Latin-1.
//!
//! Local tag readers call this only for legacy/Latin-1 metadata; Archive's
//! per-file display titles use the same strong evidence. Stored bytes, paths,
//! URLs and explicitly Unicode-declared local tags are never rewritten.

/// Repairs a strong Windows-1251-as-Latin-1 signature for display only.
///
/// The repair is deliberately restricted to long, Cyrillic-looking words and
/// never writes metadata back to the media file. Ambiguous short strings and
/// ordinary accented Latin text retain the exact value supplied by Lofty.
pub(crate) fn normalized_legacy_windows_1251_value(value: Option<&str>) -> Option<String> {
    let value = value.map(str::trim).filter(|value| !value.is_empty())?;
    let bytes = value
        .chars()
        .map(|character| u8::try_from(u32::from(character)))
        .collect::<Result<Vec<_>, _>>();
    let Ok(bytes) = bytes else {
        return Some(value.to_owned());
    };
    let (decoded, had_errors) =
        encoding_rs::WINDOWS_1251.decode_without_bom_handling(bytes.as_slice());
    if had_errors || !has_strong_legacy_windows_1251_signature(value, decoded.as_ref()) {
        return Some(value.to_owned());
    }
    Some(decoded.into_owned())
}

/// Recognizes word-level Cyrillic evidence while rejecting common Latin names.
fn has_strong_legacy_windows_1251_signature(source: &str, decoded: &str) -> bool {
    #[derive(Default)]
    struct WordEvidence {
        letters: usize,
        source_high_latin: usize,
        decoded_cyrillic: usize,
        cyrillic_vowels: usize,
        cyrillic_consonants: usize,
    }

    fn qualifies(word: &WordEvidence, minimum_letters: usize) -> bool {
        word.letters >= minimum_letters
            && word.source_high_latin.saturating_mul(5) >= word.letters.saturating_mul(4)
            && word.decoded_cyrillic.saturating_mul(5) >= word.letters.saturating_mul(4)
            && word.cyrillic_vowels > 0
            && word.cyrillic_consonants > 0
    }

    fn is_cyrillic(character: char) -> bool {
        matches!(character, '\u{0400}'..='\u{052f}')
    }

    fn is_cyrillic_vowel(character: char) -> bool {
        matches!(
            character,
            'А' | 'Е'
                | 'Ё'
                | 'И'
                | 'О'
                | 'У'
                | 'Ы'
                | 'Э'
                | 'Ю'
                | 'Я'
                | 'а'
                | 'е'
                | 'ё'
                | 'и'
                | 'о'
                | 'у'
                | 'ы'
                | 'э'
                | 'ю'
                | 'я'
                | 'І'
                | 'Ї'
                | 'Є'
                | 'і'
                | 'ї'
                | 'є'
        )
    }

    if source == decoded || source.chars().any(is_cyrillic) {
        return false;
    }
    let mut word = WordEvidence::default();
    let mut strong_words = 0_usize;
    let mut medium_words = 0_usize;
    for (source_character, decoded_character) in
        source.chars().zip(decoded.chars()).chain([(' ', ' ')])
    {
        if decoded_character.is_alphabetic() {
            word.letters = word.letters.saturating_add(1);
            word.source_high_latin = word.source_high_latin.saturating_add(usize::from(matches!(
                source_character,
                '\u{00c0}'..='\u{00ff}'
            )));
            if is_cyrillic(decoded_character) {
                word.decoded_cyrillic = word.decoded_cyrillic.saturating_add(1);
                if is_cyrillic_vowel(decoded_character) {
                    word.cyrillic_vowels = word.cyrillic_vowels.saturating_add(1);
                } else {
                    word.cyrillic_consonants = word.cyrillic_consonants.saturating_add(1);
                }
            }
            continue;
        }
        strong_words = strong_words.saturating_add(usize::from(qualifies(&word, 6)));
        medium_words = medium_words.saturating_add(usize::from(qualifies(&word, 4)));
        word = WordEvidence::default();
    }
    strong_words > 0 || medium_words >= 2
}

#[cfg(test)]
mod tests {
    use super::normalized_legacy_windows_1251_value;

    /// The provider's additional context must never weaken local-tag decisions.
    #[test]
    fn shared_legacy_repair_keeps_its_original_strong_evidence_threshold() {
        for (source, expected) in [
            ("Áåòîíîìåøàëêà", "Бетономешалка"),
            ("Ñêåëåò", "Скелет"),
            ("Îçåðî", "Îçåðî"),
            ("Ôîí", "Ôîí"),
            ("Björk", "Björk"),
            ("Озеро", "Озеро"),
        ] {
            assert_eq!(
                normalized_legacy_windows_1251_value(Some(source)).as_deref(),
                Some(expected)
            );
        }
    }
}
