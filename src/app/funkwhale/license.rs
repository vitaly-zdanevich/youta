//! Display-only links for license codes explicitly defined by Funkwhale.
//!
//! The source of these codes and URLs is Funkwhale's `music/licenses.py`:
//! <https://dev.funkwhale.audio/funkwhale/funkwhale/-/blob/stable/api/funkwhale_api/music/licenses.py>.
//! This projection never changes provider metadata or decides upload permissions.

use crate::view::{DetailLinkPresentation, DetailLinkView};

/// Projects a known code without guessing URLs or permissions for unknown licenses.
pub(super) fn link(raw: &str) -> Option<DetailLinkView> {
    let normalized = raw.trim().to_ascii_lowercase();
    let (label, url) = match normalized.as_str() {
        "cc0-1.0" => (
            "CC0 1.0".to_owned(),
            "https://creativecommons.org/publicdomain/zero/1.0/".to_owned(),
        ),
        "lal-1.3" => (
            "Licence Art Libre 1.3".to_owned(),
            "https://artlibre.org/licence/lal".to_owned(),
        ),
        _ => creative_commons_code(normalized.strip_prefix("cc-")?)?,
    };
    Some(DetailLinkView {
        prefix: "License: ".to_owned(),
        label,
        url,
        presentation: DetailLinkPresentation::LabelAndUrl,
        ..DetailLinkView::default()
    })
}

/// Accepts only the six standard combinations and versions shipped by Funkwhale.
fn creative_commons_code(code: &str) -> Option<(String, String)> {
    let (terms, remainder) = ["by-nc-nd", "by-nc-sa", "by-nd", "by-nc", "by-sa", "by"]
        .into_iter()
        .find_map(|terms| {
            code.strip_prefix(terms)
                .and_then(|suffix| suffix.strip_prefix('-'))
                .map(|suffix| (terms, suffix))
        })?;
    let (version, country) = remainder
        .split_once('-')
        .map_or((remainder, None), |(version, country)| {
            (version, Some(country))
        });
    if !matches!(version, "1.0" | "2.0" | "2.5" | "3.0" | "4.0") {
        return None;
    }
    if let Some(country) = country {
        let allowed = match version {
            "3.0" => &[
                "at", "au", "br", "ch", "cl", "cn", "cr", "cz", "de", "ec", "ee", "eg", "es", "fr",
                "gr", "gt", "hk", "hr", "ie", "igo", "it", "lu", "nl", "no", "nz", "ph", "pl",
                "pr", "pt", "ro", "rs", "sg", "th", "tw", "ug", "us", "ve", "vn", "za",
            ][..],
            "2.5" => &[
                "ar", "bg", "ca", "co", "dk", "hu", "il", "in", "mk", "mt", "mx", "my", "pe",
                "scotland",
            ][..],
            _ => return None,
        };
        if !allowed.contains(&country) {
            return None;
        }
    }
    let mut label = format!("CC {} {version}", terms.to_ascii_uppercase());
    let mut url = format!("https://creativecommons.org/licenses/{terms}/{version}/");
    if let Some(country) = country {
        label.push(' ');
        label.push_str(&country.to_ascii_uppercase());
        url.push_str(country);
        url.push('/');
    }
    Some((label, url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_codes_keep_noncommercial_and_no_derivatives_restrictions_in_the_label() {
        for version in ["1.0", "2.0", "2.5", "3.0", "4.0"] {
            for terms in ["by", "by-sa", "by-nc", "by-nd", "by-nc-sa", "by-nc-nd"] {
                let link = link(&format!("cc-{terms}-{version}")).unwrap();
                assert_eq!(
                    link.label,
                    format!("CC {} {version}", terms.to_ascii_uppercase())
                );
                assert_eq!(
                    link.url,
                    format!("https://creativecommons.org/licenses/{terms}/{version}/")
                );
                assert_eq!(link.prefix, "License: ");
                assert_eq!(link.presentation, DetailLinkPresentation::LabelAndUrl);
                assert!(link.internal_target.is_none());
            }
        }
    }

    #[test]
    fn public_domain_art_libre_and_known_jurisdictions_have_exact_original_license_urls() {
        for (raw, label, url) in [
            (
                "cc0-1.0",
                "CC0 1.0",
                "https://creativecommons.org/publicdomain/zero/1.0/",
            ),
            (
                "LAL-1.3",
                "Licence Art Libre 1.3",
                "https://artlibre.org/licence/lal",
            ),
            (
                "cc-by-nc-nd-3.0-us",
                "CC BY-NC-ND 3.0 US",
                "https://creativecommons.org/licenses/by-nc-nd/3.0/us/",
            ),
            (
                "cc-by-2.5-scotland",
                "CC BY 2.5 SCOTLAND",
                "https://creativecommons.org/licenses/by/2.5/scotland/",
            ),
        ] {
            let projected = link(raw).unwrap();
            assert_eq!(projected.label, label);
            assert_eq!(projected.url, url);
        }
        assert_eq!(link(" CC-BY-4.0 ").unwrap().label, "CC BY 4.0");
    }

    #[test]
    fn unknown_restricted_or_malformed_codes_never_gain_an_invented_license_link() {
        for raw in [
            "",
            "all-rights-reserved",
            "For preview only",
            "cc-by-5.0",
            "cc-by-4.0-us",
            "cc-by-sa-nc-4.0",
            "cc-by-3.0-zz",
            "cc-by-3.0-us/elsewhere",
            "cc0-4.0",
            "https://example.com/custom-license",
            "cc-by-4.0?redirect=https://elsewhere.example/",
        ] {
            assert!(link(raw).is_none(), "{raw}");
        }
    }
}
