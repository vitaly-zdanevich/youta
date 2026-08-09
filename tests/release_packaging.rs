//! Regression tests for Youta's feature and release-packaging contract.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

fn repository_path(relative: impl AsRef<Path>) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn read_repository_file(relative: impl AsRef<Path>) -> String {
    let path = repository_path(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

fn manifest() -> toml::Value {
    toml::from_str(&read_repository_file("Cargo.toml")).expect("Cargo.toml must remain valid TOML")
}

fn feature_entries<'a>(manifest: &'a toml::Value, name: &str) -> Vec<&'a str> {
    manifest["features"][name]
        .as_array()
        .unwrap_or_else(|| panic!("feature `{name}` must be an array"))
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .unwrap_or_else(|| panic!("feature `{name}` entries must be strings"))
        })
        .collect()
}

fn feature_closure(manifest: &toml::Value, root: &str) -> BTreeSet<String> {
    fn visit(manifest: &toml::Value, name: &str, closure: &mut BTreeSet<String>) {
        if !closure.insert(name.to_owned()) {
            return;
        }
        for entry in feature_entries(manifest, name) {
            let candidate = entry.split('/').next().unwrap_or(entry);
            if manifest["features"].get(candidate).is_some() {
                visit(manifest, candidate, closure);
            } else {
                closure.insert(entry.to_owned());
            }
        }
    }

    let mut closure = BTreeSet::new();
    visit(manifest, root, &mut closure);
    closure
}

#[test]
fn default_release_features_keep_images_qr_and_sqlite_independent() {
    let manifest = manifest();
    let default = feature_closure(&manifest, "default");
    let text_only = feature_closure(&manifest, "app");
    let tui = feature_closure(&manifest, "tui");
    let qr = feature_closure(&manifest, "qr");
    let yandex_free = feature_closure(&manifest, "app-core");

    assert!(default.contains("images"));
    assert!(default.contains("qr"));
    assert!(default.contains("dep:qrcode"));
    assert!(default.contains("app"));
    assert!(default.contains("yandex-music"));
    assert!(!default.contains("sqlite-state"));
    assert!(!default.contains("bundled-sqlite"));
    assert!(!default.contains("dep:rusqlite"));
    assert!(text_only.contains("app-core"));
    assert!(text_only.contains("yandex-music"));
    assert!(!text_only.contains("qr"));
    assert!(!text_only.contains("dep:qrcode"));
    assert!(!tui.contains("dep:qrcode"));
    // QR encoding produces a module matrix and renders nothing, so it must not
    // drag a front-end into a build that only wants the encoder.
    assert_eq!(feature_entries(&manifest, "qr"), ["dep:qrcode"]);
    assert!(!qr.contains("tui"));
    assert!(!qr.contains("dep:ratatui"));
    assert!(qr.contains("dep:qrcode"));
    assert_eq!(
        manifest["dependencies"]["qrcode"]["optional"].as_bool(),
        Some(true),
        "QR encoding must remain removable from custom builds"
    );
    assert_eq!(
        manifest["dependencies"]["qrcode"]["default-features"].as_bool(),
        Some(false),
        "QR encoding must not pull the crate's image renderer into text builds"
    );
    assert!(!yandex_free.contains("yandex-music"));
    for yandex_only_dependency in ["dep:aes", "dep:ctr", "dep:hmac"] {
        assert!(
            !yandex_free.contains(yandex_only_dependency),
            "`app-core` unexpectedly enables `{yandex_only_dependency}`"
        );
    }
    assert!(
        yandex_free.is_subset(&text_only),
        "`app-core` must stay a Yandex-free subset of the complete `app` profile"
    );

    for image_feature in [
        "images",
        "thumbnails",
        "dep:image",
        "dep:jpeg-decoder",
        "dep:ratatui-image",
    ] {
        assert!(
            !text_only.contains(image_feature),
            "the `app` text-only profile unexpectedly enables `{image_feature}`"
        );
    }

    assert_eq!(feature_entries(&manifest, "thumbnails"), ["images"]);
}

#[test]
fn yandex_music_feature_and_credentials_remain_optional_and_documented() {
    let manifest = manifest();
    let yandex_music = feature_closure(&manifest, "yandex-music");
    for requirement in [
        "network",
        "dep:aes",
        "dep:base64",
        "dep:ctr",
        "dep:hmac",
        "dep:sha2",
    ] {
        assert!(
            yandex_music.contains(requirement),
            "`yandex-music` omits `{requirement}`"
        );
    }
    for dependency in ["aes", "base64", "ctr", "hmac", "sha2"] {
        assert_eq!(
            manifest["dependencies"][dependency]["optional"].as_bool(),
            Some(true),
            "`{dependency}` must not enter Yandex-free builds"
        );
    }

    let example = read_repository_file("config.example.toml");
    assert!(example.contains("yandex_music_token = '...'"));
    assert!(example.contains("YOUTA_PROVIDERS__YANDEX_MUSIC_TOKEN='...'"));
    assert!(example.contains("This is not an"));
    assert!(example.contains("API key"));

    let readme = read_repository_file("README.md");
    assert!(readme.contains("private client API"));
    assert!(readme.contains("--features app,qr"));
    assert!(readme.contains("--features app-core,images,qr"));
    assert!(readme.contains("Audiobook search is best-effort"));
    assert!(readme.contains("no stable first-class audiobook search or playback"));
}

#[test]
fn local_capability_umbrella_and_ratatui_features_remain_intentional() {
    let manifest = manifest();
    let local = feature_closure(&manifest, "local");
    for capability in [
        "local-browser",
        "local-metadata",
        "local-rename",
        "local-trash",
        "local-move",
        "local-artwork",
    ] {
        assert!(local.contains(capability), "`local` omits `{capability}`");
    }

    let ratatui_features = manifest["dependencies"]["ratatui"]["features"]
        .as_array()
        .expect("ratatui dependency features must be explicit");
    assert!(
        ratatui_features
            .iter()
            .all(|feature| feature.as_str() != Some("macros")),
        "the unused ratatui `macros` feature must stay disabled"
    );
}

/// Front-end-free capabilities must stay reachable without a front-end.
///
/// Each of these produces data rather than pixels, so a build that wants only
/// the data must not be forced to link a renderer. The assertions are here
/// because the coupling is easy to reintroduce by adding one entry to a feature
/// list, and nothing else would notice.
#[test]
fn renderer_free_capabilities_never_require_a_front_end() {
    let manifest = manifest();

    for capability in ["remote-artwork", "local-artwork", "qr"] {
        let closure = feature_closure(&manifest, capability);
        for renderer in ["tui", "dep:ratatui", "dep:crossterm", "dep:ratatui-image"] {
            assert!(
                !closure.contains(renderer),
                "`{capability}` must not require `{renderer}`"
            );
        }
    }

    // Terminal artwork is that pipeline plus decoding and graphics protocols,
    // so `images` keeps both halves.
    let images = feature_closure(&manifest, "images");
    assert!(images.contains("remote-artwork"));
    assert!(images.contains("tui"));
    assert!(images.contains("dep:ratatui-image"));

    // Local covers are found by reading tags and directory entries, so that
    // capability must not drag in the HTTP client either: a text-only local
    // build stays offline.
    let local_artwork = feature_closure(&manifest, "local-artwork");
    assert!(!local_artwork.contains("network"));
    assert!(!local_artwork.contains("dep:ureq"));
}

#[test]
fn release_script_builds_four_portable_non_sqlite_variants() {
    let script = read_repository_file("scripts/package-release.sh");

    assert!(!script.contains("--features bundled-sqlite"));
    assert!(script.contains("--features app,qr"));
    assert!(script.contains("--features app,images"));
    assert!(script.contains("\t\t\t--features app\n"));
    assert!(script.contains("archive_suffix=-text"));
    assert!(script.contains("archive_suffix=-no-qr"));
    assert!(script.contains("archive_suffix=-text-no-qr"));
    assert!(script.contains("images)"));
    assert!(script.contains("text)"));
    assert!(script.contains("images-no-qr)"));
    assert!(script.contains("text-no-qr)"));
    assert!(script.contains("Supported variants: images, text, images-no-qr, text-no-qr"));
    assert!(script.contains("YOUTA_BUILD_ORIGIN=github-release"));
    assert!(!script.contains("install -D"));
    assert!(script.contains("x86_64-unknown-linux-gnu"));
    assert!(
        script
            .contains("i686-unknown-linux-gnu)\n\t\toperating_system=linux\n\t\tarchitecture=i686")
    );
    assert!(script.contains("aarch64-unknown-linux-gnu"));
    assert!(script.contains("x86_64-apple-darwin"));
    assert!(script.contains("aarch64-apple-darwin"));
    assert!(script.contains("gtar"));
    assert!(script.contains("shasum -a 256"));
}

#[test]
fn workflows_validate_and_publish_the_documented_platform_contract() {
    let release = read_repository_file(".github/workflows/release.yml");
    for target in [
        "x86_64-unknown-linux-gnu",
        "i686-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
    ] {
        assert!(release.contains(target), "release matrix omits `{target}`");
    }
    assert!(release.contains("dist images"));
    assert!(release.contains("dist text"));
    assert!(release.contains("dist images-no-qr"));
    assert!(release.contains("dist text-no-qr"));
    assert!(release.contains("fetch-depth: 10"));
    assert!(release.contains("brew install gnu-tar"));
    assert!(release.contains("architecture: i686"));
    assert!(release.contains("sudo apt-get install --yes gcc-multilib libc6-dev-i386"));
    for contract in [
        "platforms=(linux-amd64 linux-i686 linux-arm64 macos-amd64 macos-arm64)",
        "suffixes=('' -text -no-qr -text-no-qr)",
        "readonly expected_asset_count=42",
        "archive=\"youta-${version}-${platform}${suffix}.tar.gz\"",
        "vendor_archive=\"youta-${version}-vendor.tar.xz\"",
        "--label expected-assets",
        "--label downloaded-assets",
        "sha256sum --check --strict -- *.sha256",
    ] {
        assert!(
            release.contains(contract),
            "release workflow omits asset contract `{contract}`"
        );
    }
    assert!(
        release
            .find("Validate the complete release asset set")
            .expect("release workflow validates assets")
            < release
                .find("Create or update the tagged release")
                .expect("release workflow publishes assets"),
        "release assets must be validated before publication"
    );

    let ci = read_repository_file(".github/workflows/ci.yml");
    assert!(ci.contains("Linux compile (i686)"));
    assert!(ci.contains("i686-unknown-linux-gnu"));
    assert!(ci.contains("sudo apt-get install --yes gcc-multilib libc6-dev-i386"));
    assert!(ci.contains("cargo build --locked --release --target i686-unknown-linux-gnu"));
    assert!(ci.contains("ELF 32-bit"));
    assert!(ci.contains("/lib/ld-linux.so.2"));
    assert!(ci.contains("target/i686-unknown-linux-gnu/release/youta --version"));
    assert!(ci.contains("x86_64-pc-windows-msvc"));
    assert!(ci.contains("aarch64-pc-windows-msvc"));
    assert!(ci.contains("x86_64-unknown-freebsd"));
    for workflow in [ci.as_str(), release.as_str()] {
        assert!(
            workflow.contains("--features app-core"),
            "workflow omits the complete Yandex-free application lane"
        );
        assert!(
            workflow.contains("--features app-core,images"),
            "workflow omits the documented Yandex-free graphical application lane"
        );
        assert!(
            workflow.contains("--features app,images"),
            "workflow omits the image-enabled, QR-disabled release boundary"
        );
        assert!(
            workflow.contains("--features app,qr"),
            "workflow omits the text-only, QR-enabled release boundary"
        );
        assert!(
            workflow.contains("--features qr"),
            "workflow omits the standalone QR feature boundary"
        );
        assert!(
            workflow.contains("--features tui,yandex-music\n"),
            "workflow omits the standalone Yandex Music lane without Wikidata"
        );
    }
}

#[test]
fn live_radio_workflow_retries_each_provider_independently() {
    let ci = read_repository_file(".github/workflows/ci.yml");
    let live_radio = ci
        .split_once("  live-radio:\n")
        .expect("CI workflow retains the live Radio job")
        .1
        .split_once("\n  coverage:\n")
        .expect("live Radio job remains before Coverage")
        .0;
    let tests = [
        (
            "Decode curated public streams and parse passive metadata",
            "radio_stream_and_passive_metadata_are_usable",
        ),
        (
            "Decode a generated NPR stream and parse its current programme",
            "generated_npr_station_stream_and_program_are_usable",
        ),
        (
            "Resolve and decode BBC Sounds radio",
            "bbc_sounds_resolution_and_audio_are_usable",
        ),
    ];

    for (step_name, test_name) in tests {
        let marker = format!("      - name: {step_name}\n");
        let step = live_radio
            .split_once(&marker)
            .unwrap_or_else(|| panic!("live Radio workflow omits `{step_name}`"))
            .1
            .split("\n      - name: ")
            .next()
            .expect("workflow step has content");
        assert!(
            step.contains("for attempt in 1 2; do"),
            "`{step_name}` has no independent retry loop"
        );
        assert!(
            step.contains("sleep 10"),
            "`{step_name}` has no bounded pause before its retry"
        );
        assert!(
            step.contains(&format!("--exact {test_name} \\")),
            "`{step_name}` does not execute `{test_name}`"
        );
        assert_eq!(
            step.matches("--exact ").count(),
            1,
            "`{step_name}` contains more than one exact test invocation"
        );
        assert_eq!(
            tests
                .iter()
                .filter(|(_, candidate)| step.contains(candidate))
                .count(),
            1,
            "`{step_name}` chains another live Radio test inside its retry boundary"
        );
    }
}

#[test]
fn live_wikidata_workflow_retries_each_probe_independently() {
    let ci = read_repository_file(".github/workflows/ci.yml");
    let live_wikidata = ci
        .split_once("  live-wikidata:\n")
        .expect("CI workflow retains the live Wikidata job")
        .1
        .split_once("\n  live-radio:\n")
        .expect("live Wikidata job remains before live Radio")
        .0;
    let tests = [
        (
            "Find the public YouTube video fixture",
            "wikidata_finds_the_youtube_video_fixture_item",
        ),
        (
            "Find the public YouTube channel fixture",
            "wikidata_finds_the_youtube_channel_fixture_item",
        ),
        (
            "Load public media statements",
            "wikidata_loads_the_media_fixture_statements",
        ),
        (
            "Load public follower history",
            "wikidata_loads_the_follower_history_fixture",
        ),
    ];

    for (step_name, test_name) in tests {
        let marker = format!("      - name: {step_name}\n");
        let step = live_wikidata
            .split_once(&marker)
            .unwrap_or_else(|| panic!("live Wikidata workflow omits `{step_name}`"))
            .1
            .split("\n      - name: ")
            .next()
            .expect("workflow step has content");
        assert!(
            step.contains("for attempt in 1 2; do"),
            "`{step_name}` has no independent retry loop"
        );
        assert!(
            step.contains("sleep 10"),
            "`{step_name}` has no bounded pause before its retry"
        );
        assert!(
            step.contains(&format!("--exact {test_name} \\")),
            "`{step_name}` does not execute `{test_name}`"
        );
        assert_eq!(
            step.matches("--exact ").count(),
            1,
            "`{step_name}` contains more than one exact test invocation"
        );
        assert_eq!(
            tests
                .iter()
                .filter(|(_, candidate)| step.contains(candidate))
                .count(),
            1,
            "`{step_name}` chains another live Wikidata test inside its retry boundary"
        );
    }
}
