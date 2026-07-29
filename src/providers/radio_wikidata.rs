//! Verified links from built-in radio presets to Wikidata items.
//!
//! The lookup is deliberately static and performs no network requests. Station
//! identifiers are matched exactly and case-sensitively. A slice is returned
//! because one preset may legitimately describe more than one Wikidata item in
//! a future snapshot.
//!
//! The adjacent `radio_wikidata_verified.json` manifest retains the catalogue,
//! label, homepage evidence, and discovery-file digest used to generate this
//! table. Only records marked `verified` are compiled; unconfirmed BBC
//! candidates are intentionally absent.

/// Verified `(station ID, Wikidata QIDs)` rows sorted by station ID.
const VERIFIED_MAPPINGS: &[(&str, &[&str])] = &[
    ("deutschlandfunk", &["Q695328"]),
    ("fip", &["Q961891"]),
    ("france-musique", &["Q19909"]),
    ("france-musique-la-bo", &["Q19909"]),
    ("kexp", &["Q761627"]),
    ("mcot-thinking-radio", &["Q4921537"]),
    ("npr-0550ce9a8bac47b59b58270c5d122946", &["Q2538121"]),
    ("npr-0faffbf5da6a4017a0bdf3b47d3cd780", &["Q7957077"]),
    ("npr-0fdb4fbbc8584b5591ad9819579a4800", &["Q7956421"]),
    ("npr-13efbfa4a8101a148f7a75cef72df682", &["Q4938465"]),
    ("npr-13f0f97cd6700b74b9ea02f16b004165", &["Q7953422"]),
    ("npr-140bb5f1ce501ad495b94d147fa391df", &["Q7956609"]),
    ("npr-18756ed6f42b4a72bbc4421abac5ba5b", &["Q14680497"]),
    ("npr-248c3f87070e4fc3a026586db60e483e", &["Q7953422"]),
    ("npr-319bf4f7a04e466db3f93561a7b25bbf", &["Q7946953"]),
    ("npr-32fa6480d53c4f8cb974b7275ad64f94", &["Q6339479"]),
    ("npr-4845e6714d3f4410ae366954eb90d31a", &["Q7953305"]),
    ("npr-4fcf712f19b4413b80330354e9ed3769", &["Q7956609"]),
    ("npr-4fcf712f20a847138e81faf780f3eb41", &["Q7956609"]),
    ("npr-4fcf713517374aeea549cae46ed92e27", &["Q55457531"]),
    ("npr-4fcf713611314542b4280007519615d3", &["Q4938465"]),
    ("npr-4fcf71380d8a43d2b44151cff2b09117", &["Q6334711"]),
    ("npr-4fcf71381cbf41c7b0312cc8517117f0", &["Q6334711"]),
    ("npr-4fcf713901774fd986f64cf5ae171afd", &["Q6334711"]),
    ("npr-4fcf713c24804135a15728f31849fe7b", &["Q6339651"]),
    ("npr-4fcf7140044745829c1d51accf723565", &["Q588294"]),
    ("npr-4fcf7140160646198d09ddb2d5321329", &["Q2538121"]),
    ("npr-4fcf714017a84927a8a8a5a016b0394a", &["Q2538121"]),
    ("npr-4fcf714322e74aeaac07ed0eaa688aec", &["Q7956640"]),
    ("npr-4fcf714325df44678e840d60dc53d064", &["Q7956640"]),
    ("npr-4fcf71440959424a8eadf4d79423fdef", &["Q7957390"]),
    ("npr-4fcf71440bf641b0ad0b93e189fd83fe", &["Q7957390"]),
    ("npr-4fcf71440d6646df86d04045d75538e7", &["Q7957390"]),
    ("npr-4fcf714908eb49dbbb51209b1882cbb6", &["Q6339448"]),
    ("npr-4fcf714b056c45428d16c7a9b73c0c62", &["Q6326558"]),
    ("npr-4fcf714b0fed46aabb40d8dccfd2b653", &["Q3564621"]),
    ("npr-4fcf714e00e94ec693a9e645b20c66b2", &["Q14710846"]),
    ("npr-4fcf714f0ca641f2bb155a2fa45c71ee", &["Q7957901"]),
    ("npr-4fcf71501fed40a38998ea7960473a4a", &["Q1551068"]),
    ("npr-4fcf71560d524fd08e6a34685d3fbe6e", &["Q5161583"]),
    ("npr-4fcf71561b024af39759655591e7fc55", &["Q5148861"]),
    ("npr-4fcf715b136842d5a311e6fc430f5c66", &["Q7956583"]),
    ("npr-4fcf715b19d84e738e7198f238beeeb2", &["Q7956583"]),
    ("npr-4fcf716105464458abcaa39a8bc27f7c", &["Q7054935"]),
    ("npr-4fcf71651ded4275bd90cea5c8d16748", &["Q7952902"]),
    ("npr-4fcf716725f54aaa9be139cdcaa0e26f", &["Q7956739"]),
    ("npr-4fcf716818be482b8454d0bd1e2608d5", &["Q2538121"]),
    ("npr-4fcf71691dad4981bb739d0e2c0bd41f", &["Q6337228"]),
    ("npr-4fcf716a1e104dffb7fa66211fe2fc14", &["Q7957077"]),
    ("npr-514afff916f74e23895d84c43defa24f", &["Q7951787"]),
    ("npr-514afffb1a10483192fee5ed8c2888ba", &["Q3564621"]),
    ("npr-541bb3dd89894943a7713cae0e201769", &["Q7957233"]),
    ("npr-78fa19a2696747beacd4597558082525", &["Q4938465"]),
    ("npr-7976e6eb076647ac9a6d47e10a2ec951", &["Q7946953"]),
    ("npr-844815615e9643ffb4ee70dfc3fe05fd", &["Q7054574"]),
    ("npr-9717f4c310f84c5a85634a40dc5d971a", &["Q7953842"]),
    ("npr-987c4343b3324caea5e8f52183e09719", &["Q6334154"]),
    ("npr-99cffd6263b24ab7aa9e9404c07171d6", &["Q588294"]),
    ("npr-ae77f58ad68b47e8aac25d9466f7f158", &["Q588294"]),
    ("npr-c0620ba3707b4a4c8e48a7f5fac8ee47", &["Q7956583"]),
    ("npr-c5c25b5a38fb491d9a42674cb014dce7", &["Q7957077"]),
    ("npr-cf05c4c366c94fb2b374018259bdef55", &["Q6328360"]),
    ("npr-d5ccf8ec4d4e4a0c82c359f7bf9dcf90", &["Q7949515"]),
    ("npr-e1c3653c28be49a98ece2890f78f546f", &["Q7956583"]),
    ("npr-st813", &["Q7946953"]),
    ("npr-st985", &["Q7956609"]),
    ("npr-st986", &["Q7956609"]),
    ("npr-st987", &["Q7956609"]),
    ("npr-st988", &["Q7956609"]),
    ("radio-maria-philippines", &["Q56282067"]),
    ("radio-racyja", &["Q815265"]),
    ("retro-fm-russia", &["Q4393717"]),
    ("slay-radio", &["Q4347268"]),
];

/// Returns verified Wikidata QIDs for a built-in radio station identifier.
///
/// The returned slice is empty when the identifier has no verified mapping.
/// The data is compile-time static and this function performs no allocation or
/// I/O.
#[must_use]
pub fn wikidata_item_ids_for_station(station_id: &str) -> &'static [&'static str] {
    VERIFIED_MAPPINGS
        .binary_search_by_key(&station_id, |(mapped_station_id, _)| mapped_station_id)
        .map_or(&[], |index| VERIFIED_MAPPINGS[index].1)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde::Deserialize;

    use super::{VERIFIED_MAPPINGS, wikidata_item_ids_for_station};
    use crate::providers::radio::{NPR_STATIONS, STATIONS};

    const SOURCE_SHA256: &str = "666368568a32f67b9582ff91984cae002455778ece15a287e86d613b86f741c9";

    #[derive(Debug, Deserialize)]
    struct EvidenceManifest {
        source_file: String,
        source_sha256: String,
        selection: String,
        verification_basis: String,
        mappings: Vec<EvidenceRecord>,
    }

    #[derive(Debug, Deserialize)]
    struct EvidenceRecord {
        catalogue: String,
        status: String,
        station_id: String,
        qid: String,
        label: String,
        evidence_url: String,
    }

    fn evidence_manifest() -> EvidenceManifest {
        serde_json::from_str(include_str!("radio_wikidata_verified.json"))
            .expect("the checked-in radio-to-Wikidata evidence manifest should be valid JSON")
    }

    fn is_valid_qid(item_id: &str) -> bool {
        item_id
            .strip_prefix('Q')
            .is_some_and(|digits| !digits.is_empty() && !digits.starts_with('0'))
            && item_id[1..].bytes().all(|byte| byte.is_ascii_digit())
    }

    #[test]
    fn lookup_returns_representative_curated_and_npr_items() {
        assert_eq!(
            wikidata_item_ids_for_station("france-musique-la-bo"),
            &["Q19909"]
        );
        assert_eq!(
            wikidata_item_ids_for_station("npr-0550ce9a8bac47b59b58270c5d122946"),
            &["Q2538121"]
        );
    }

    #[test]
    fn lookup_returns_empty_slice_for_unknown_station() {
        assert!(wikidata_item_ids_for_station("unknown-radio-station").is_empty());
    }

    #[test]
    fn mappings_are_sorted_and_have_valid_unique_pairs() {
        assert_eq!(VERIFIED_MAPPINGS.len(), 73);
        assert!(
            VERIFIED_MAPPINGS
                .windows(2)
                .all(|pair| pair[0].0 < pair[1].0)
        );

        let mut pairs = BTreeSet::new();
        for (station_id, item_ids) in VERIFIED_MAPPINGS {
            assert!(!item_ids.is_empty(), "{station_id}");
            for item_id in *item_ids {
                assert!(is_valid_qid(item_id), "{item_id}");
                assert!(
                    pairs.insert((*station_id, *item_id)),
                    "duplicate station/QID pair: {station_id} {item_id}"
                );
            }
        }
    }

    #[test]
    fn every_mapping_names_a_checked_in_radio_preset() {
        let station_ids = STATIONS
            .iter()
            .chain(NPR_STATIONS)
            .map(|station| station.id)
            .collect::<BTreeSet<_>>();

        for (station_id, _) in VERIFIED_MAPPINGS {
            assert!(
                station_ids.contains(station_id),
                "mapping references missing station {station_id}"
            );
        }
    }

    #[test]
    fn compiled_mappings_match_the_verified_evidence_manifest() {
        let manifest = evidence_manifest();
        assert_eq!(manifest.source_file, "radio-wikidata-discovery.json");
        assert_eq!(manifest.source_sha256, SOURCE_SHA256);
        assert_eq!(manifest.selection, "mappings[].status == \"verified\"");
        assert!(!manifest.verification_basis.trim().is_empty());
        assert_eq!(manifest.mappings.len(), 73);

        let mut manifest_pairs = BTreeSet::new();
        for record in &manifest.mappings {
            assert_eq!(record.status, "verified");
            assert!(matches!(record.catalogue.as_str(), "curated" | "npr"));
            assert!(!record.label.trim().is_empty());
            let (station, expected_catalogue) = STATIONS
                .iter()
                .find(|station| station.id == record.station_id)
                .map(|station| (station, "curated"))
                .or_else(|| {
                    NPR_STATIONS
                        .iter()
                        .find(|station| station.id == record.station_id)
                        .map(|station| (station, "npr"))
                })
                .expect("manifest station should exist in the checked-in catalogue");
            assert_eq!(record.catalogue, expected_catalogue);
            assert_eq!(record.evidence_url, station.homepage);
            assert!(
                station.name.contains(&record.label) || station.summary.contains(&record.label),
                "verified label {:?} is absent from station {}",
                record.label,
                record.station_id
            );
            assert!(
                manifest_pairs.insert((record.station_id.clone(), record.qid.clone())),
                "duplicate manifest pair: {} {}",
                record.station_id,
                record.qid
            );
        }

        let compiled_pairs = VERIFIED_MAPPINGS
            .iter()
            .flat_map(|(station_id, item_ids)| {
                item_ids
                    .iter()
                    .map(|item_id| ((*station_id).to_owned(), (*item_id).to_owned()))
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(compiled_pairs, manifest_pairs);
    }
}
