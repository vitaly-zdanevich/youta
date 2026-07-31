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
fn default_release_features_keep_images_and_sqlite_independent() {
    let manifest = manifest();
    let default = feature_closure(&manifest, "default");
    let text_only = feature_closure(&manifest, "app");
    let yandex_free = feature_closure(&manifest, "app-core");

    assert!(default.contains("images"));
    assert!(default.contains("app"));
    assert!(default.contains("yandex-music"));
    assert!(!default.contains("sqlite-state"));
    assert!(!default.contains("bundled-sqlite"));
    assert!(!default.contains("dep:rusqlite"));
    assert!(text_only.contains("app-core"));
    assert!(text_only.contains("yandex-music"));
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
    assert!(readme.contains("--features app-core,images"));
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

#[test]
fn release_script_builds_both_portable_non_sqlite_variants() {
    let script = read_repository_file("scripts/package-release.sh");

    assert!(!script.contains("--features bundled-sqlite"));
    assert!(script.contains("--features app"));
    assert!(script.contains("archive_suffix=-text"));
    assert!(script.contains("images)"));
    assert!(script.contains("text)"));
    assert!(!script.contains("install -D"));
    assert!(script.contains("x86_64-unknown-linux-gnu"));
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
        "aarch64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
    ] {
        assert!(release.contains(target), "release matrix omits `{target}`");
    }
    assert!(release.contains("dist images"));
    assert!(release.contains("dist text"));
    assert!(release.contains("brew install gnu-tar"));

    let ci = read_repository_file(".github/workflows/ci.yml");
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
            workflow.contains("--features tui,yandex-music\n"),
            "workflow omits the standalone Yandex Music lane without Wikidata"
        );
    }
}
