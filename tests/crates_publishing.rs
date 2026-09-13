//! Regression contracts for authenticated, release-only crates.io publication.

/// Reads workspace-only fixtures at runtime, since the core archive omits GUI.
fn repository_file(relative: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

/// Isolates the crate job so other jobs cannot satisfy its security checks.
fn publishing_job() -> String {
    let workflow = repository_file(".github/workflows/release.yml");
    let (_, body) = workflow
        .split_once("\n  publish-crates:\n")
        .expect("the release workflow must define a crates.io publishing job");
    body.split_inclusive('\n')
        .take_while(|line| {
            !line.starts_with("  ") || line.starts_with("    ") || line.trim().is_empty()
        })
        .collect()
}

/// Publishing requires a successful canonical tagged release, not branch CI.
#[test]
fn crates_io_publication_requires_the_completed_tagged_release() {
    let job = publishing_job();
    for required in [
        "needs: publish",
        "github.repository == 'vitaly-zdanevich/youta'",
        "startsWith(github.ref, 'refs/tags/v')",
        "vars.PUBLISH_CRATES_IO == 'true'",
        "node-version: '24'",
        "timeout-minutes: 360",
        "contents: read",
        "id-token: write",
        "persist-credentials: false",
        "group: crates-io-${{ github.repository }}-${{ github.ref }}",
        "cancel-in-progress: false",
    ] {
        assert!(job.contains(required), "crate job omits `{required}`");
    }
    assert!(!job.contains("contents: write"));
    assert!(!job.contains("continue-on-error:"));
}

/// A package is verified before a short-lived token is acquired and used.
#[test]
fn crates_io_authentication_follows_verification_and_is_step_scoped() {
    let job = publishing_job();
    let verification = job
        .find("cargo publish --dry-run --locked --package youta --package youta-gui --registry crates-io")
        .expect("verify the package before requesting a token");
    let authentication = job
        .find("rust-lang/crates-io-auth-action@c6f97d42243bad5fab37ca0427f495c86d5b1a18")
        .expect("pin the official Node 24 authentication action");
    let publication = job
        .find("cargo publish --locked --package youta --package youta-gui --registry crates-io")
        .expect("publish both verified packages in dependency order");
    assert!(verification < authentication && authentication < publication);
    let (preparation, publish_step) = job
        .split_once("      - name: Publish both crates to crates.io\n")
        .expect("the token belongs to a dedicated publication step");
    assert!(!preparation.contains("CARGO_REGISTRY_TOKEN"));
    assert!(publish_step.contains("CARGO_REGISTRY_TOKEN: ${{ steps.auth.outputs.token }}"));
    assert!(!job.contains("secrets."));
}

/// Registry writes retain Cargo's clean-source, package, and verification gates.
#[test]
fn crates_io_publication_preserves_verification_and_surfaces_failures() {
    let job = publishing_job();
    assert_eq!(job.matches("cargo publish ").count(), 2);
    assert_eq!(
        job.matches("test -z \"$(git status --porcelain --untracked-files=all)\"")
            .count(),
        2
    );
    // Cargo counts the intentionally included, ignored frontend as dirty;
    // explicit source-cleanliness checks must guard that exception.
    for forbidden in ["--no-verify", "--workspace", "|| true"] {
        assert!(
            !job.contains(forbidden),
            "crate job bypasses a gate: `{forbidden}`"
        );
    }
}

/// Registry installation must use the matching core and include the real page.
#[test]
fn gui_package_is_self_contained_and_uses_the_matching_core() {
    let core: toml::Value = toml::from_str(include_str!("../Cargo.toml")).unwrap();
    let gui: toml::Value = toml::from_str(&repository_file("gui/Cargo.toml")).unwrap();
    assert_eq!(gui["package"]["publish"][0].as_str(), Some("crates-io"));
    assert_eq!(
        gui["dependencies"]["youta"]["version"].as_str(),
        Some(format!("={}", core["package"]["version"].as_str().unwrap()).as_str())
    );
    let files = gui["package"]["include"].as_array().unwrap();
    for path in [
        "/src/**",
        "/LICENSE",
        "/build.rs",
        "/frontend/**",
        "/icons/**",
        "/capabilities/**",
        "/tauri.conf.json",
    ] {
        assert!(
            files.iter().any(|file| file.as_str() == Some(path)),
            "GUI package omits `{path}`"
        );
    }
    let defaults = gui["features"]["default"].as_array().unwrap();
    assert!(
        defaults
            .iter()
            .any(|feature| feature.as_str() == Some("custom-protocol"))
    );
    assert_eq!(
        gui["features"]["custom-protocol"][0].as_str(),
        Some("tauri/custom-protocol")
    );
}

/// Both independently distributed crates carry the complete license notice.
#[test]
fn gui_crate_carries_the_shared_license_notice() {
    assert_eq!(repository_file("gui/LICENSE"), include_str!("../LICENSE"));
}

/// The real frontend is built and checked before either package can be uploaded.
#[test]
fn gui_frontend_is_prepared_before_package_verification() {
    let job = publishing_job();
    let install = job
        .find("npm --prefix gui/ui ci")
        .expect("install locked UI dependencies");
    let build = job
        .find("npm --prefix gui/ui run build")
        .expect("build the real page");
    let assets = job
        .find("test -s gui/frontend/app.js")
        .expect("reject a placeholder page");
    let verify = job.find("cargo publish --dry-run").unwrap();
    assert!(install < build && build < assets && assets < verify);
    assert!(job[assets..verify].contains("git status --porcelain --untracked-files=all"));
    assert!(job.contains("libwebkit2gtk-4.1-dev"));
}

/// Pull requests validate the actual archives without publishing credentials.
#[test]
fn branch_ci_verifies_both_packages_without_authentication() {
    let ci = repository_file(".github/workflows/ci.yml");
    assert!(ci.contains("cargo publish --dry-run --locked --package youta --package youta-gui --registry crates-io --allow-dirty"));
    assert!(!ci.contains("crates-io-auth-action"));
    assert!(!ci.contains("CARGO_REGISTRY_TOKEN"));
}
