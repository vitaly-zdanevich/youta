//! Regression tests for Youta's feature and release-packaging contract.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::process::Command;

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
    let gpm = feature_closure(&manifest, "gpm");
    let yandex_free = feature_closure(&manifest, "app-core");

    assert!(default.contains("images"));
    assert!(default.contains("qr"));
    assert!(default.contains("dep:qrcode"));
    assert!(default.contains("app"));
    assert!(default.contains("yandex-music"));
    assert!(default.contains("gpm"));
    assert!(default.contains("dep:mio"));
    assert!(!default.contains("sqlite-state"));
    assert!(!default.contains("bundled-sqlite"));
    assert!(!default.contains("dep:rusqlite"));
    assert!(text_only.contains("app-core"));
    assert!(text_only.contains("yandex-music"));
    assert!(!text_only.contains("qr"));
    assert!(!text_only.contains("dep:qrcode"));
    assert!(!text_only.contains("gpm"));
    assert!(!text_only.contains("dep:mio"));
    assert!(!yandex_free.contains("gpm"));
    assert!(!yandex_free.contains("dep:mio"));
    assert_eq!(feature_entries(&manifest, "gpm"), ["tui", "dep:mio"]);
    assert!(gpm.contains("tui"));
    assert!(gpm.contains("dep:mio"));
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
    assert!(
        yandex_free.contains("librivox"),
        "the complete application profile must expose LibriVox by default"
    );
    assert_eq!(
        feature_entries(&manifest, "librivox"),
        ["network"],
        "LibriVox must remain independently removable without source-specific dependencies"
    );
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
fn local_archive_folders_are_default_but_removable_from_both_front_ends() {
    let manifest = manifest();
    let default = feature_closure(&manifest, "default");
    let application = feature_closure(&manifest, "app");
    let archives = feature_closure(&manifest, "local-archives");

    assert!(default.contains("local-archives"));
    assert!(!application.contains("local-archives"));
    assert_eq!(
        feature_entries(&manifest, "local-archives"),
        ["local-browser", "archive-zip", "dep:sha2"]
    );
    for requirement in ["local-browser", "archive-zip", "dep:zip", "dep:sha2"] {
        assert!(archives.contains(requirement));
    }

    let gui: toml::Value = toml::from_str(&read_repository_file("gui/Cargo.toml"))
        .expect("GUI Cargo.toml must remain valid TOML");
    assert!(
        gui["features"]["default"]
            .as_array()
            .expect("GUI defaults")
            .iter()
            .any(|feature| feature.as_str() == Some("local-archives"))
    );
    assert_eq!(
        gui["features"]["local-archives"]
            .as_array()
            .expect("GUI local-archives forwarding")
            .iter()
            .filter_map(toml::Value::as_str)
            .collect::<Vec<_>>(),
        ["youta/local-archives"]
    );
}

#[test]
fn sponsorblock_is_default_but_removable_from_both_front_ends() {
    let manifest = manifest();
    let default = feature_closure(&manifest, "default");
    let application = feature_closure(&manifest, "app");

    assert!(default.contains("sponsorblock"));
    assert!(
        !application.contains("sponsorblock"),
        "`app` must not make SponsorBlock impossible to compile out"
    );
    assert_eq!(feature_entries(&manifest, "sponsorblock"), ["network"]);

    let gui: toml::Value = toml::from_str(&read_repository_file("gui/Cargo.toml"))
        .expect("GUI Cargo.toml must remain valid TOML");
    assert!(
        gui["features"]["default"]
            .as_array()
            .expect("GUI defaults")
            .iter()
            .any(|feature| feature.as_str() == Some("sponsorblock"))
    );
    assert_eq!(
        gui["features"]["sponsorblock"]
            .as_array()
            .expect("GUI SponsorBlock forwarding")
            .iter()
            .filter_map(toml::Value::as_str)
            .collect::<Vec<_>>(),
        ["youta/sponsorblock"]
    );

    let readme = read_repository_file("README.md");
    assert!(readme.contains(
        "`sponsorblock` is the independently removable SponsorBlock client and playback"
    ));
    assert!(readme.contains("`YOUTA_PLAYBACK__SPONSORBLOCK_ENABLED=false`"));
    assert!(readme.contains("`USE=\"-sponsorblock\"` removes its API client"));

    let ci = read_repository_file(".github/workflows/ci.yml");
    assert!(
        ci.contains("cargo check --locked --no-default-features --features sponsorblock --lib")
    );
    assert!(ci.contains("cargo check --locked --no-default-features --features tui,sponsorblock"));
}

#[test]
fn audio_quality_is_default_but_remains_a_removable_local_capability() {
    let manifest = manifest();
    let default = feature_closure(&manifest, "default");
    let application = feature_closure(&manifest, "app");
    let audio_quality = feature_closure(&manifest, "audio-quality");

    assert!(default.contains("audio-quality"));
    assert!(default.contains("dep:rustfft"));
    assert!(default.contains("dep:rustix"));
    assert!(audio_quality.contains("local-browser"));
    assert!(audio_quality.contains("dep:rustfft"));
    assert!(audio_quality.contains("dep:rustix"));
    assert!(
        !application.contains("audio-quality"),
        "`app` must not make the default-on analyzer impossible to disable"
    );
    assert_eq!(
        manifest["dependencies"]["rustfft"]["optional"].as_bool(),
        Some(true),
        "custom builds must be able to omit the FFT implementation"
    );

    let readme = read_repository_file("README.md");
    assert!(readme.contains("[V] Analyze quality"));
    assert!(readme.contains("measured cutoff"));
    assert!(readme.contains("cannot recover an exact original bitrate"));
    assert!(readme.contains("https://docs.rs/rustfft/"));
    let architecture = read_repository_file("docs/ARCHITECTURE.md");
    assert!(architecture.contains("Local audio-quality analysis"));
    assert!(architecture.contains("replacement-sensitive file identity"));

    let gui_manifest: toml::Value = toml::from_str(&read_repository_file("gui/Cargo.toml"))
        .expect("gui/Cargo.toml must remain valid TOML");
    assert!(
        gui_manifest["features"]["default"]
            .as_array()
            .expect("the GUI declares its default feature profile")
            .iter()
            .any(|feature| feature.as_str() == Some("audio-quality")),
        "ordinary GUI builds must retain the default-on analyzer"
    );
    assert_eq!(
        gui_manifest["features"]["audio-quality"][0].as_str(),
        Some("youta/audio-quality"),
        "the GUI feature must forward to the removable shared-core analyzer"
    );
}

#[test]
fn video_summary_is_default_but_remains_a_removable_renderer_free_capability() {
    let manifest = manifest();
    let default = feature_closure(&manifest, "default");
    let application = feature_closure(&manifest, "app");
    let video_summary = feature_closure(&manifest, "summary");

    assert!(default.contains("summary"));
    assert!(video_summary.contains("yt-dlp"));
    assert!(video_summary.contains("dep:rustix"));
    assert!(
        !application.contains("summary"),
        "`app` must not make the default-on Codex integration impossible to disable"
    );
    for renderer in ["controller", "tui", "dep:crossterm", "dep:ratatui"] {
        assert!(
            !video_summary.contains(renderer),
            "`summary` must not require `{renderer}`"
        );
    }

    let gui_manifest: toml::Value = toml::from_str(&read_repository_file("gui/Cargo.toml"))
        .expect("gui/Cargo.toml must remain valid TOML");
    assert!(
        gui_manifest["features"]["default"]
            .as_array()
            .expect("the GUI declares its default feature profile")
            .iter()
            .any(|feature| feature.as_str() == Some("summary")),
        "ordinary GUI builds must retain video summaries"
    );
    assert_eq!(
        gui_manifest["features"]["summary"][0].as_str(),
        Some("youta/summary"),
        "the GUI feature must disable the summary UI and backend in both crates"
    );

    let readme = read_repository_file("README.md");
    assert!(readme.contains("`summary` is the independently removable Codex summary"));
    assert!(readme.contains("`USE=\"-summary\"` removes the Codex summary UI and backend"));

    let ci = read_repository_file(".github/workflows/ci.yml");
    assert!(ci.contains("cargo check --locked --no-default-features --features summary --lib"));
    assert!(ci.contains("cargo check --locked --no-default-features --features tui,summary"));
}

#[test]
fn evernote_is_default_but_remains_a_removable_export_capability() {
    let manifest = manifest();
    let default = feature_closure(&manifest, "default");
    let application = feature_closure(&manifest, "app");
    let evernote = feature_closure(&manifest, "evernote");

    assert!(default.contains("evernote"));
    for requirement in [
        "controller",
        "network",
        "yt-dlp",
        "dep:evernote",
        "dep:md5",
        "dep:thrift",
    ] {
        assert!(evernote.contains(requirement));
    }
    assert!(
        !application.contains("evernote"),
        "`app` must not make the default-on Evernote export impossible to disable"
    );

    let gui_manifest: toml::Value = toml::from_str(&read_repository_file("gui/Cargo.toml"))
        .expect("gui/Cargo.toml must remain valid TOML");
    assert!(
        gui_manifest["features"]["default"]
            .as_array()
            .expect("the GUI declares its default feature profile")
            .iter()
            .any(|feature| feature.as_str() == Some("evernote")),
        "ordinary GUI builds must retain Evernote export"
    );
    assert_eq!(
        gui_manifest["features"]["evernote"][0].as_str(),
        Some("youta/evernote"),
        "the GUI feature must disable the Evernote UI and backend in both crates"
    );

    let readme = read_repository_file("README.md");
    assert!(readme.contains("The default-on `evernote` feature"));
    assert!(readme.contains("`USE=\"-evernote\"` removes the Evernote EDAM client"));

    let ci = read_repository_file(".github/workflows/ci.yml");
    assert!(ci.contains("cargo check --locked --no-default-features --features evernote --lib"));
    assert!(ci.contains("cargo check --locked --no-default-features --features tui,evernote"));
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
    assert!(readme.contains("--features app,audio-quality,qr"));
    assert!(readme.contains("--features app-core,audio-quality,images,qr"));
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
fn release_script_builds_gpm_and_linux_no_gpm_non_sqlite_executables() {
    let script = read_repository_file("scripts/package-release.sh");

    assert!(!script.contains("--features bundled-sqlite"));
    for feature_set in [
        "cargo_features=app,audio-quality,commons-upload,evernote,gpm,images,local-archives,qr,sponsorblock,summary",
        "cargo_features=app,audio-quality,commons-upload,evernote,gpm,local-archives,qr,sponsorblock,summary",
        "cargo_features=app,audio-quality,commons-upload,evernote,gpm,images,local-archives,sponsorblock,summary",
        "cargo_features=app,audio-quality,commons-upload,evernote,gpm,local-archives,sponsorblock,summary",
        "cargo_features=app,audio-quality,commons-upload,evernote,images,local-archives,qr,sponsorblock,summary",
        "cargo_features=app,audio-quality,commons-upload,evernote,local-archives,qr,sponsorblock,summary",
        "cargo_features=app,audio-quality,commons-upload,evernote,images,local-archives,sponsorblock,summary",
        "cargo_features=app,audio-quality,commons-upload,evernote,local-archives,sponsorblock,summary",
    ] {
        assert!(
            script
                .lines()
                .map(str::trim)
                .any(|line| line == feature_set),
            "release script omits feature set `{feature_set}`"
        );
    }
    assert!(script.contains("executable_suffix=-text"));
    assert!(script.contains("executable_suffix=-no-qr"));
    assert!(script.contains("executable_suffix=-text-no-qr"));
    for variant in [
        "images)",
        "text)",
        "images-no-qr)",
        "text-no-qr)",
        "images-no-gpm)",
        "text-no-gpm)",
        "images-no-qr-no-gpm)",
        "text-no-qr-no-gpm)",
    ] {
        assert!(
            script.contains(variant),
            "release script omits variant `{variant}`"
        );
    }
    for suffix in [
        "executable_suffix=-no-gpm",
        "executable_suffix=-text-no-gpm",
        "executable_suffix=-no-qr-no-gpm",
        "executable_suffix=-text-no-qr-no-gpm",
    ] {
        assert!(script.contains(suffix), "release script omits `{suffix}`");
    }
    assert!(script.contains("No-GPM release variants are supported only on Linux"));
    assert!(script.contains("--features \"${cargo_features}\""));
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
    assert!(script.contains("executable=\"${output_directory}/${package_name}\""));
    assert!(script.contains("install -m 755"));
    assert!(!script.contains(".tar.gz"));
    assert!(!script.contains("gtar"));
    assert!(!script.contains("gzip"));
    assert!(script.contains("shasum -a 256"));

    let readme = read_repository_file("README.md");
    assert!(readme.contains("same four combinations with a trailing `-no-gpm`\nsuffix"));
    assert!(readme.contains("neither `app` nor `app-core` adds it transitively"));
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
    assert!(release.contains("dist images-no-gpm"));
    assert!(release.contains("dist text-no-gpm"));
    assert!(release.contains("dist images-no-qr-no-gpm"));
    assert!(release.contains("dist text-no-qr-no-gpm"));
    for suffix in [
        "-no-gpm",
        "-text-no-gpm",
        "-no-qr-no-gpm",
        "-text-no-qr-no-gpm",
    ] {
        let upload_path = format!(
            "path: dist/youta-${{{{ env.PACKAGE_VERSION }}}}-${{{{ matrix.artifact_os }}}}-${{{{ matrix.architecture }}}}{suffix}"
        );
        assert!(
            release.contains(&upload_path),
            "release workflow omits no-GPM upload `{upload_path}`"
        );
    }
    assert!(release.contains("fetch-depth: 10"));
    assert!(!release.contains("brew install gnu-tar"));
    assert!(release.contains("architecture: i686"));
    assert!(release.contains("sudo apt-get install --yes gcc-multilib libc6-dev-i386"));
    for contract in [
        "platforms=(linux-amd64 linux-i686 linux-arm64 macos-amd64 macos-arm64)",
        "suffixes=('' -text -no-qr -text-no-qr)",
        "linux_platforms=(linux-amd64 linux-i686 linux-arm64)",
        "no_gpm_suffixes=(-no-gpm -text-no-gpm -no-qr-no-gpm -text-no-qr-no-gpm)",
        // Build jobs still transfer one internal checksum beside each of the 48
        // deliverables so publication can verify every byte. GitHub Releases
        // publish only those 48 deliverables because GitHub exposes their
        // computed hashes in the Digest column.
        "readonly expected_download_count=96",
        "readonly expected_release_asset_count=48",
        "executable=\"youta-${version}-${platform}${suffix}\"",
        "desktop_executables=(linux-amd64 linux-i686 linux-arm64 macos-amd64 macos-arm64 windows-amd64)",
        "gui_executable=\"youta-gui-${version}-${platform}${extension}\"",
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
    for obsolete_notice in [
        "Prepare the license notice",
        "Upload the license notice",
        "youta-*-LICENSE.txt",
        "license_notice=",
    ] {
        assert!(
            !release.contains(obsolete_notice),
            "release workflow still publishes the standalone notice `{obsolete_notice}`"
        );
    }
    let gui_main = read_repository_file("gui/src/main.rs");
    let gui_entrypoint = gui_main
        .find("fn main()")
        .expect("the desktop executable has an entry point");
    let license_preflight = gui_main[gui_entrypoint..]
        .find("requested_license(std::env::args_os())")
        .expect("the desktop executable checks its embedded license option");
    let configuration_startup = gui_main[gui_entrypoint..]
        .find("Config::load()")
        .expect("the desktop executable loads its configuration");
    assert!(
        license_preflight < configuration_startup,
        "the portable GUI must print its embedded notice before configuration or Tauri startup"
    );
    assert!(
        read_repository_file("gui/src/desktop.rs")
            .contains("license: Some(youta::LICENSE_TEXT.to_owned())"),
        "the native GUI About dialog must expose the embedded notice where supported"
    );
    assert_eq!(
        release.matches("uses: actions/upload-artifact@v7").count(),
        release.matches("archive: false").count(),
        "every workflow artifact must be the file itself, not an implicit ZIP"
    );

    let publish = release
        .split_once("\n  publish:\n")
        .map(|(_, publish)| publish)
        .expect("release workflow must define its publish job");
    assert!(
        publish.contains("find dist -maxdepth 1 -type f ! -name '*.sha256' -print0"),
        "standalone checksum files must remain internal instead of becoming GitHub Release assets"
    );
    assert!(
        publish.contains("gh release delete-asset \"${RELEASE_TAG}\" \"${checksum_asset}\" --yes"),
        "rerunning publication must remove checksum assets left by an older workflow"
    );
    assert!(
        publish.contains("desktop_version=${version}"),
        "the checkout-free publish job must derive the synchronized GUI version from the tag"
    );
    assert!(
        !publish.contains("gui/Cargo.toml"),
        "the checkout-free publish job cannot inspect a repository file"
    );
    assert!(
        release
            .find("Validate the complete release asset set")
            .expect("release workflow validates assets")
            < release
                .find("Create or update the tagged release")
                .expect("release workflow publishes assets"),
        "release assets must be validated before publication"
    );
    assert!(
        read_repository_file("README.md").contains("GitHub's Digest column"),
        "release documentation must direct users to GitHub's built-in checksums"
    );

    let ci = read_repository_file(".github/workflows/ci.yml");
    assert!(ci.contains("Linux compile (i686)"));
    assert!(ci.contains("i686-unknown-linux-gnu"));
    assert!(ci.contains("sudo apt-get install --yes gcc-multilib libc6-dev-i386"));
    assert!(ci.contains("cargo build --locked --release --target i686-unknown-linux-gnu"));
    for feature_set in [
        "app,audio-quality,commons-upload,evernote,gpm,images,local-archives,qr,sponsorblock,summary",
        "app,audio-quality,commons-upload,evernote,gpm,local-archives,qr,sponsorblock,summary",
        "app,audio-quality,commons-upload,evernote,gpm,images,local-archives,sponsorblock,summary",
        "app,audio-quality,commons-upload,evernote,gpm,local-archives,sponsorblock,summary",
        "app,audio-quality,commons-upload,evernote,images,local-archives,qr,sponsorblock,summary",
        "app,audio-quality,commons-upload,evernote,local-archives,qr,sponsorblock,summary",
        "app,audio-quality,commons-upload,evernote,images,local-archives,sponsorblock,summary",
        "app,audio-quality,commons-upload,evernote,local-archives,sponsorblock,summary",
    ] {
        assert!(
            ci.contains(&format!(
                "--target i686-unknown-linux-gnu --no-default-features --features {feature_set}\n"
            )),
            "i686 CI omits published feature set `{feature_set}`"
        );
    }
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
        assert!(
            workflow.contains("--features tui,librivox\n"),
            "workflow omits the standalone LibriVox lane without Wikidata"
        );
        assert!(
            workflow.contains("--features librivox\n"),
            "workflow omits the isolated LibriVox provider boundary"
        );
        assert!(
            workflow.contains("--features tui,librivox,wikidata"),
            "workflow omits the LibriVox and Wikidata composition boundary"
        );
    }
}

#[test]
fn vendor_archive_carries_the_built_gui_frontend_for_offline_packages() {
    let script = read_repository_file("scripts/package-vendor.sh");
    for asset in ["index.html", "app.js", "app.css"] {
        assert!(
            script.contains(&format!("[[ -f gui/frontend/{asset} ]]")),
            "vendor packaging must reject a missing production GUI asset: {asset}"
        );
    }
    assert!(
        script.contains("${package_root}/gui/frontend"),
        "the source-only vendor archive must carry the production GUI frontend"
    );
    assert!(
        script.contains("--workspace \\\n\t\t\t--all-features"),
        "offline verification must compile the GUI workspace member too"
    );

    let release = read_repository_file(".github/workflows/release.yml");
    let build_frontend = release
        .find("npm --prefix gui/ui run build")
        .expect("the vendor job builds the production GUI frontend");
    let package_vendor = release
        .find("scripts/package-vendor.sh dist")
        .expect("the vendor job packages the offline source inputs");
    assert!(
        build_frontend < package_vendor,
        "the GUI frontend must exist before the vendor archive is assembled"
    );
}

/// The desktop window ships a standalone executable and native installers.
///
/// The terminal and window are both published as directly downloadable
/// executables. The window additionally retains native installers produced by
/// a bundler that only runs on that platform. Nothing about the installer set is
/// derivable, so it is written down in `gui/tauri.conf.json`,
/// `scripts/package-desktop.sh`, both workflows, and `README.md` — and this is
/// what keeps those five copies the same answer.
#[test]
fn the_desktop_window_ships_one_bundle_contract_across_every_file_that_states_it() {
    let configuration: serde_json::Value =
        serde_json::from_str(&read_repository_file("gui/tauri.conf.json"))
            .expect("the desktop configuration is valid JSON");
    let bundle = &configuration["bundle"];

    assert_eq!(
        bundle["active"].as_bool(),
        Some(true),
        "the bundler produces the release artefact and cannot be inactive"
    );
    assert_eq!(
        bundle["licenseFile"].as_str(),
        Some("../LICENSE"),
        "native desktop packages must retain the full MIT notice"
    );
    assert!(
        repository_path(
            Path::new("gui").join(
                bundle["licenseFile"]
                    .as_str()
                    .expect("the desktop bundle declares its license file")
            )
        )
        .is_file(),
        "the desktop package license file must resolve to a repository file"
    );
    let targets: BTreeSet<String> = bundle["targets"]
        .as_array()
        .expect("the bundle targets are listed rather than inferred")
        .iter()
        .filter_map(|target| target.as_str().map(str::to_owned))
        .collect();
    for target in ["deb", "rpm", "appimage", "app", "dmg", "nsis"] {
        assert!(
            targets.contains(target),
            "the desktop bundle contract omits `{target}`"
        );
    }

    // A window with no icon is a window the desktop cannot show in a launcher,
    // and the bundler fails late and obscurely when one is missing.
    for icon in bundle["icon"]
        .as_array()
        .expect("the bundle declares its icons")
    {
        let icon = icon.as_str().expect("an icon path is a string");
        assert!(
            repository_path(Path::new("gui").join(icon)).exists(),
            "the desktop bundle names a missing icon: {icon}"
        );
    }

    let nsis = &bundle["windows"]["nsis"];
    for field in ["installerIcon", "uninstallerIcon"] {
        assert_eq!(
            nsis[field].as_str(),
            Some("icons/icon.ico"),
            "the NSIS {field} must use the same Youta icon as the portable Windows executable"
        );
    }

    // The Linux web view is a runtime dependency of the package, not of the
    // build, so it belongs in the package metadata as well as in the workflow.
    let deb_depends: BTreeSet<String> = bundle["linux"]["deb"]["depends"]
        .as_array()
        .expect("the Debian package declares its dependencies")
        .iter()
        .filter_map(|entry| entry.as_str().map(str::to_owned))
        .collect();
    assert!(deb_depends.contains("libwebkit2gtk-4.1-0"));

    let script = read_repository_file("scripts/package-desktop.sh");
    assert!(script.contains("gui/Cargo.toml"));
    assert!(script.contains("npm --prefix gui/ui run build"));
    assert!(script.contains(
        "youta-gui-${version}-${operating_system}-${architecture}${executable_extension}"
    ));
    assert!(script.contains("target/release/youta-gui${executable_extension}"));
    assert!(script.contains("install -m 755"));
    assert!(script.contains("youta-desktop-${version}-${operating_system}-${architecture}"));
    assert!(
        script.contains("-name '*-setup.exe'"),
        "Tauri v2 names NSIS installers with a hyphen before `setup`"
    );
    assert!(
        !script.contains("-name '*_setup.exe'"),
        "the obsolete underscore pattern misses Tauri v2 NSIS installers"
    );
    assert!(
        script.contains("No installable bundle was produced"),
        "a bundler that produced nothing must fail rather than report success"
    );
    assert!(
        script.contains("refusing to choose between them"),
        "two bundles claiming one asset name must fail rather than pick one"
    );
    // The read-write staging image the DMG is assembled inside sits beside the
    // `.app`, is a `.dmg` by name, and is five times the size. Collecting from
    // the format directories rather than the whole tree is what keeps it out.
    assert!(
        script.contains("for format in deb rpm appimage dmg nsis msi"),
        "bundles must be collected from the bundler's format directories"
    );

    let release = read_repository_file(".github/workflows/release.yml");
    for platform in [
        "linux-amd64:deb rpm AppImage",
        "linux-arm64:deb rpm AppImage",
        "macos-amd64:dmg",
        "macos-arm64:dmg",
        "windows-amd64:exe",
    ] {
        assert!(
            release.contains(platform),
            "the release asset contract omits `{platform}`"
        );
    }
    assert!(
        release.contains("desktop_executables=(linux-amd64 linux-i686 linux-arm64"),
        "the desktop executable contract must include Linux i686"
    );
    assert!(release.contains("readonly expected_download_count=96"));
    assert!(release.contains("readonly expected_release_asset_count=48"));
    assert!(release.contains("scripts/package-desktop.sh dist-desktop"));
    assert!(
        release
            .contains("scripts/package-desktop-executable.sh i686-unknown-linux-gnu dist-desktop")
    );
    let cross_packaging = read_repository_file("scripts/package-desktop-executable.sh");
    assert!(cross_packaging.contains("@tauri-apps/cli@2.11.4"));
    assert!(cross_packaging.contains("--target \"${target}\""));
    assert!(cross_packaging.contains("--no-bundle"));
    assert!(
        !cross_packaging.contains("\ncargo build"),
        "a direct Cargo build omits Tauri's production custom protocol"
    );
    for dependency in [
        "libwebkit2gtk-4.1-dev:i386",
        "libgtk-3-dev:i386",
        "libdbus-1-dev:i386",
    ] {
        assert!(
            release.contains(dependency),
            "the i686 desktop build omits `{dependency}`"
        );
    }
    assert!(release.contains("PKG_CONFIG_ALLOW_CROSS: '1'"));
    assert!(release.contains("libwebkit2gtk-4.1-dev"));
    assert!(release.contains("libdbus-1-dev"));
    assert!(
        release.contains("APPLE_SIGNING_IDENTITY"),
        "signing must be wired even while the certificate does not exist"
    );

    let ci = read_repository_file(".github/workflows/ci.yml");
    assert!(
        ci.contains("cargo test --locked -p youta-gui --all-targets"),
        "the desktop crate needs a lane that runs its tests"
    );
    assert!(
        ci.contains("cargo test --locked -p youta-gui --all-targets --no-default-features"),
        "the desktop crate must prove that its default audio analyzer is removable"
    );
    assert!(ci.contains("libwebkit2gtk-4.1-dev"));
    assert!(ci.contains("libdbus-1-dev"));
    assert!(ci.contains("name: Desktop window (Linux i686)"));
    assert!(
        ci.contains("scripts/package-desktop-executable.sh i686-unknown-linux-gnu dist-desktop")
    );
    assert!(ci.contains("path: dist-desktop/youta-gui-*-linux-i686"));
    assert!(
        ci.contains("The desktop window links ${renderer}."),
        "the renderer-free claim must be checked by machine, not by hand"
    );

    let readme = read_repository_file("README.md");
    assert!(readme.contains("WEBKIT_DISABLE_DMABUF_RENDERER=1"));
    assert!(readme.contains("libwebkit2gtk-4.1-dev"));
    assert!(
        readme.contains("not signed"),
        "an unsigned installer is something a user meets before Youta starts"
    );
    assert!(readme.contains("standalone GUI executable"));
}

#[test]
fn readme_displays_the_canonical_desktop_icon_below_its_badges() {
    let readme = read_repository_file("README.md");
    let badges_end = readme
        .find("[![Technical Debt]")
        .expect("README must retain its final badge");
    let logo = readme
        .find("![Youta logo](gui/icons/icon.png)")
        .expect("README must display the canonical desktop icon");
    let introduction = readme
        .find("Youta is a low-resource")
        .expect("README must retain its introduction");

    assert!(
        badges_end < logo && logo < introduction,
        "the logo must appear after all badges and before the README introduction"
    );
}

/// Empty repository secrets are exported by Actions as present-but-empty.
///
/// The executable probe is Unix-only because resolving `bash` on Windows can
/// select the WSL launcher instead of the Git Bash used by the release job.
/// Windows still checks the integration contract in the platform-independent
/// test below.
#[cfg(unix)]
#[test]
fn desktop_signing_mode_distinguishes_unsigned_and_configured_macos_builds() {
    let helper = repository_path("scripts/desktop-signing-mode.sh");
    let probe = r#"
set -Eeuo pipefail
source "$1"

unset APPLE_CERTIFICATE
configure_desktop_tauri_signing_args macos
[[ ${#tauri_signing_args[@]} -eq 1 && ${tauri_signing_args[0]} == --no-sign ]]

export APPLE_CERTIFICATE=
configure_desktop_tauri_signing_args macos
[[ ${#tauri_signing_args[@]} -eq 1 && ${tauri_signing_args[0]} == --no-sign ]]
declare -p APPLE_CERTIFICATE > /dev/null
[[ -z ${APPLE_CERTIFICATE} ]]

export APPLE_CERTIFICATE=configured-certificate
configure_desktop_tauri_signing_args macos
[[ ${#tauri_signing_args[@]} -eq 0 ]]
[[ ${APPLE_CERTIFICATE} == configured-certificate ]]
bash -c '[[ ${APPLE_CERTIFICATE} == configured-certificate ]]'

APPLE_CERTIFICATE=
configure_desktop_tauri_signing_args linux
[[ ${#tauri_signing_args[@]} -eq 0 ]]
"#;

    let output = Command::new("bash")
        .arg("-c")
        .arg(probe)
        .arg("desktop-signing-mode-test")
        .arg(&helper)
        .output()
        .expect("bash must execute the signing-mode regression probe");

    assert!(
        output.status.success(),
        "desktop signing mode selection failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Every host validates that desktop packaging selects signing mode before a
/// pinned Tauri invocation, without requiring that host to launch Bash.
#[test]
fn desktop_signing_mode_precedes_the_pinned_tauri_invocation() {
    let helper = read_repository_file("scripts/desktop-signing-mode.sh");
    assert!(helper.contains("configure_desktop_tauri_signing_args"));
    assert!(helper.contains("tauri_signing_args+=(--no-sign)"));

    let packaging = read_repository_file("scripts/package-desktop.sh");
    let source_helper = packaging
        .find("desktop-signing-mode.sh")
        .expect("desktop packaging sources the signing-mode helper");
    let configure = packaging
        .find("configure_desktop_tauri_signing_args")
        .expect("desktop packaging configures Tauri's signing arguments");
    let tauri = packaging
        .find("@tauri-apps/cli@2.11.4")
        .expect("desktop packaging pins the Tauri CLI with --no-sign support");
    assert!(
        source_helper < configure && configure < tauri,
        "unsigned macOS mode must be selected before Tauri starts"
    );
    assert!(
        !packaging.contains("\"@tauri-apps/cli@2\""),
        "desktop packaging must not float across Tauri 2.x releases"
    );
}

#[test]
fn desktop_and_core_versions_remain_in_sync() {
    let core_manifest = manifest();
    let core_version = core_manifest["package"]["version"]
        .as_str()
        .expect("the core package has a version");
    let gui_manifest: toml::Value = toml::from_str(&read_repository_file("gui/Cargo.toml"))
        .expect("gui/Cargo.toml must remain valid TOML");
    let gui_version = gui_manifest["package"]["version"]
        .as_str()
        .expect("the GUI package has a version");
    let gui_core_features: BTreeSet<&str> = gui_manifest["dependencies"]["youta"]["features"]
        .as_array()
        .expect("the GUI declares its shared-core feature profile")
        .iter()
        .map(|feature| feature.as_str().expect("GUI core features are strings"))
        .collect();
    for required in [
        "controller",
        "sources",
        "remote-artwork",
        "qr",
        "yandex-music",
    ] {
        assert!(
            gui_core_features.contains(required),
            "the GUI core profile omitted `{required}`"
        );
    }
    assert_eq!(
        gui_manifest["features"]["default"][0].as_str(),
        Some("audio-quality"),
        "ordinary desktop builds must enable audio quality analysis"
    );
    assert_eq!(
        gui_manifest["features"]["audio-quality"][0].as_str(),
        Some("youta/audio-quality"),
        "desktop distributors need one feature that disables the analyzer in both crates"
    );
    let tauri: serde_json::Value =
        serde_json::from_str(&read_repository_file("gui/tauri.conf.json"))
            .expect("gui/tauri.conf.json must remain valid JSON");

    assert_eq!(gui_version, core_version, "the GUI crate version drifted");
    assert_eq!(
        tauri["version"].as_str(),
        Some(core_version),
        "the Tauri bundle version drifted"
    );

    let ui_package: serde_json::Value =
        serde_json::from_str(&read_repository_file("gui/ui/package.json"))
            .expect("gui/ui/package.json must remain valid JSON");
    let ui_lock: serde_json::Value =
        serde_json::from_str(&read_repository_file("gui/ui/package-lock.json"))
            .expect("gui/ui/package-lock.json must remain valid JSON");
    assert_eq!(
        ui_package["version"].as_str(),
        Some(core_version),
        "the GUI page version drifted"
    );
    assert_eq!(
        ui_lock["version"].as_str(),
        Some(core_version),
        "the GUI package lock version drifted"
    );
    assert_eq!(
        ui_lock["packages"][""]["version"].as_str(),
        Some(core_version),
        "the locked GUI root package version drifted"
    );
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
fn live_librivox_workflow_guards_api_html_and_audio_contracts() {
    let ci = read_repository_file(".github/workflows/ci.yml");
    let job = ci
        .split_once("  live-librivox:\n")
        .expect("CI retains the live LibriVox job")
        .1
        .split_once("\n  live-wikidata:\n")
        .expect("live LibriVox remains before live Wikidata")
        .0;

    assert!(job.contains("timeout-minutes: 360"));
    assert!(job.contains("YOUTA_RUN_LIVE_LIBRIVOX_TEST: '1'"));
    assert!(job.contains("--features librivox"));
    assert!(job.contains("--exact librivox_catalogue_book_author_and_audio_are_usable"));
    assert!(job.contains("for attempt in 1 2; do"));
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
