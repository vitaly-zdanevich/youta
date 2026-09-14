//! Action fixture parity shared by the core-only and native-window test targets.

/// Whether one frontend action belongs to a capability omitted from this build.
///
/// Action names remain in the shared page so one frontend can serve every
/// feature set. Runtime support flags prevent these dispatch sites from becoming
/// reachable when the corresponding Rust enum variants are not compiled.
pub(crate) fn action_belongs_to_disabled_feature(
    name: &str,
    has_feature: impl Fn(&str) -> bool,
) -> bool {
    (!has_feature("ascii-visualizer") && name == "DismissAsciiVisualizer")
        || (!has_feature("commons-upload")
            && matches!(
                name,
                "AddCommonsCategorySuggestionAt"
                    | "CycleCommonsAuthMethod"
                    | "CycleCommonsUploadLicense"
                    | "DismissCommonsCredentials"
                    | "DismissCommonsUpload"
                    | "OpenCommonsAccountRegistration"
                    | "OpenCommonsBotPasswordGuide"
                    | "OpenCommonsCategorySuggestionAt"
                    | "OpenCommonsUpload"
                    | "OpenCommonsUploadResult"
                    | "RemoveCommonsUploadCategory"
                    | "SelectCommonsCredentialField"
                    | "SelectCommonsUploadField"
                    | "SubmitCommonsCredentials"
                    | "SubmitCommonsUpload"
            ))
        || (!has_feature("s3-upload")
            && matches!(
                name,
                "OpenS3Upload"
                    | "SelectS3UploadField"
                    | "ToggleS3UploadVideo"
                    | "SubmitS3Upload"
                    | "DismissS3Upload"
                    | "OpenS3Credentials"
                    | "SelectS3CredentialField"
                    | "SubmitS3Credentials"
                    | "DismissS3Credentials"
            ))
        || (!has_feature("archive-upload")
            && matches!(
                name,
                "OpenArchiveUpload"
                    | "SelectArchiveUploadField"
                    | "ToggleArchiveUploadVideo"
                    | "SubmitArchiveUpload"
                    | "DismissArchiveUpload"
                    | "OpenArchiveUploadResult"
                    | "SelectArchiveCredentialField"
                    | "SubmitArchiveCredentials"
                    | "DismissArchiveCredentials"
                    | "OpenArchiveCredentialsGuide"
            ))
        || (!has_feature("evernote")
            && matches!(
                name,
                "DismissEvernoteCredentials"
                    | "DismissEvernoteNote"
                    | "InsertEvernoteCaptions"
                    | "OpenEvernoteDeveloperTokenGuide"
                    | "OpenEvernoteNote"
                    | "OpenEvernoteNoteResult"
                    | "SelectEvernoteNoteField"
                    | "SubmitEvernoteCredentials"
                    | "SubmitEvernoteNote"
            ))
        || (!has_feature("youtube-captions")
            && matches!(name, "ActivateYouTubeCaption" | "DismissYouTubeCaptions"))
        || (!has_feature("lan-sharing")
            && matches!(
                name,
                "ConfirmPodcastFeed"
                    | "DismissLanShare"
                    | "DismissPodcastFeed"
                    | "ShareLocalFiles"
                    | "ShareLocalPodcast"
                    | "ShareYouTubeChannelPodcast"
                    | "StopLanShare"
                    | "TogglePodcastFeedIgnoreBefore"
                    | "TogglePodcastFeedSkipShorts"
            ))
        || (!has_feature("qr") && matches!(name, "OpenVideoQr" | "DismissVideoQr"))
        || (!has_feature("yt-dlp")
            && matches!(
                name,
                "OpenChannelDownload"
                    | "ConfirmChannelDownload"
                    | "ToggleChannelDownloadIgnoreBefore"
                    | "ToggleChannelDownloadSkipShorts"
                    | "DismissChannelDownload"
            ))
        || (!has_feature("yt-dlp") && !has_feature("yandex-music") && name == "CancelDownload")
}

/// Checks the same payload examples TypeScript compiles, without starting a GUI.
///
/// Only explicitly listed, disabled variants can be exempted. A supported
/// variant must deserialize and serialize the complete fixture unchanged.
pub(crate) fn assert_fixture_contract(is_disabled: impl Fn(&str) -> bool) {
    let fixtures: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../fixtures/ui-actions.json"))
            .expect("parse shared frontend action fixtures");
    assert!(
        fixtures.len() >= 195,
        "action fixture coverage unexpectedly shrank"
    );
    for fixture in fixtures {
        let name = fixture.as_str().unwrap_or_else(|| {
            let object = fixture.as_object().expect("action is a string or object");
            assert_eq!(object.len(), 1, "action must have exactly one external tag");
            object.keys().next().expect("action tag")
        });
        let decoded = serde_json::from_value::<youta::view::UiAction>(fixture.clone());
        if is_disabled(name) {
            let error = decoded.expect_err("exemption must name an uncompiled variant");
            assert!(
                error.to_string().contains("unknown variant"),
                "disabled {name} failed for a payload reason instead: {error}"
            );
            continue;
        }
        let action = decoded.unwrap_or_else(|error| panic!("invalid frontend {name}: {error}"));
        assert_eq!(
            serde_json::to_value(action).expect("serialize frontend action"),
            fixture,
            "frontend {name} payload drifted"
        );
    }
}
