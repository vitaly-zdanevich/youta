//! Keeps the window's hand-written TypeScript contract honest.
//!
//! `ui/src/contract.ts` restates a subset of the shared crate's view types so
//! the window can be type-checked. Nothing in the build makes those two agree,
//! and a mistyped field is silent in both directions: TypeScript happily reads
//! `details.published_label` from a value that has `published`, and the compiler
//! sees a declared field, so the panel simply renders nothing.
//!
//! That is not hypothetical — it is exactly how the first Details panel shipped
//! with five invented field names. These tests compare each declared interface
//! against the keys `serde_json` actually emits, so drift fails the build
//! instead of quietly emptying a panel.
//!
//! The check is deliberately one-directional. Declaring a field the serializer
//! does not emit is a bug; *omitting* one is how this file stays a subset, which
//! is the whole reason it is hand-written rather than generated.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

#[cfg(feature = "commons-upload")]
use youta::commons_upload::{CommonsCategorySuggestion, CommonsUploadDraft};
use youta::domain::{MediaId, SourceKind};
#[cfg(feature = "evernote")]
use youta::evernote::EvernoteNoteDraft;
use youta::keymap::{PopupGeometry, ScrollGeometry};
#[cfg(feature = "commons-upload")]
use youta::view::CommonsUploadPopupView;
#[cfg(feature = "evernote")]
use youta::view::EvernoteNotePopupView;
use youta::view::{
    AudioQualityPopupView, DetailLinkView, DetailTimecodeView, DetailVideoLinkView, DetailView,
    DetailWikidataEntityView, DownloadView, ErrorPopupView, GitHubIssueSubmissionView,
    LocalMoveDestinationView, NowPlayingView, PlaylistChoiceView, PlaylistPopupView,
    PreferencesPopupView, ProjectCommitView, ProjectHistoryPopupView, QueuePopupView, QueueRowView,
    RowView, SubscriptionsView, VideoCommentView, VideoCommentsPopupView, VideoSummaryPopupView,
    ViewModel, WaveformView, YtDlpForbiddenView, YtDlpGentooVersionView, YtDlpVersionLookupView,
};
use youta::waveform::PeakPyramid;

/// Reads the declarations the window compiles against.
fn contract_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src")
        .join("contract.ts");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// Extracts the field names of every `export interface` block.
///
/// The grammar accepted here is the one this file is written in: one field per
/// line, indented two spaces, `name: type;` or `name?: type;`. Anything more
/// elaborate would mean the contract had grown past what a hand-written subset
/// should be, which is itself worth failing on.
fn declared_interfaces(source: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut interfaces = BTreeMap::new();
    let mut lines = source.lines();
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("export interface ") else {
            continue;
        };
        let Some(name) = rest.split_whitespace().next() else {
            continue;
        };
        let mut fields = BTreeSet::new();
        for body in lines.by_ref() {
            if body == "}" {
                break;
            }
            let Some(declaration) = body.strip_prefix("  ") else {
                continue;
            };
            let Some((field, _)) = declaration.split_once(':') else {
                continue;
            };
            let field = field.trim_end_matches('?');
            if field
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
                && !field.is_empty()
            {
                fields.insert(field.to_owned());
            }
        }
        interfaces.insert(name.to_owned(), fields);
    }
    interfaces
}

/// Serializes one value and returns its top-level key set.
fn emitted_keys(value: &impl Serialize) -> BTreeSet<String> {
    let json = serde_json::to_value(value).expect("encode contract type");
    json.as_object()
        .expect("contract types serialize as objects")
        .keys()
        .cloned()
        .collect()
}

/// Returns the payload keys of one externally tagged enum variant.
///
/// Serde writes a payload variant as `{"Ready": {…}}`, so the interface the
/// window declares describes the *inner* object rather than the wrapper.
fn variant_keys(value: &impl Serialize, variant: &str) -> BTreeSet<String> {
    let json = serde_json::to_value(value).expect("encode contract variant");
    json.get(variant)
        .and_then(serde_json::Value::as_object)
        .unwrap_or_else(|| panic!("{variant} does not serialize as a payload variant"))
        .keys()
        .cloned()
        .collect()
}

/// A ready waveform, whose peaks are deliberately not part of the JSON.
fn ready_waveform() -> WaveformView {
    WaveformView::Ready {
        media_id: MediaId::new(SourceKind::Local, "/music/a.flac"),
        generation: 1,
        duration: Duration::from_secs(1),
        pyramid: Arc::new(PeakPyramid::default()),
    }
}

/// A `PreferencesPopupView` has no `Default`, so the editor is spelled out.
fn preferences() -> PreferencesPopupView {
    PreferencesPopupView {
        subscriptions_layout: youta::config::SubscriptionsLayout::default(),
        save_playback_history: true,
        video_summary_backend: youta::config::VideoSummaryBackend::default(),
        video_summary_supported: true,
        skip_advertisement_chapters: false,
        sponsorblock_enabled: true,
        sponsorblock_supported: true,
        nyan_cat_seekbar: true,
        nyan_cat_supported: true,
        youtube_prewarm: false,
        youtube_thumbnail_size: youta::config::YouTubeThumbnailSize::default(),
        show_local_folder_sizes: false,
        show_images_in_tty: false,
        bandcamp_audio_format: youta::config::BandcampAudioFormat::default(),
        config_path: String::new(),
        environment_override: None,
        validation_error: None,
    }
}

#[test]
fn window_filters_and_edits_playback_history_from_the_shared_view_policy() {
    let sources = window_sources();
    let source_named = |suffix: &str| {
        sources
            .iter()
            .find_map(|(path, source)| path.ends_with(suffix).then_some(source.as_str()))
            .unwrap_or_else(|| panic!("missing window source {suffix}"))
    };
    let app = source_named("App.tsx");
    let popups = source_named("components/popups.tsx");

    for required in [
        "view.playback_history_enabled",
        "source.id !== \"History\"",
        "playbackHistoryEnabled={view.playback_history_enabled}",
    ] {
        assert!(app.contains(required), "App no longer contains {required}");
    }
    for required in [
        "Save playback history",
        "popup.save_playback_history",
        "dispatch(\"TogglePlaybackHistorySaving\")",
        "playbackHistoryEnabled ?",
    ] {
        assert!(
            popups.contains(required),
            "Preferences no longer contains {required}"
        );
    }
    assert!(!popups.contains("[h] Save playback history"));
}

/// Every declared field must exist in the JSON the reducer publishes.
#[test]
fn the_typescript_contract_names_only_fields_the_reducer_emits() {
    let source = contract_source();
    let declared = declared_interfaces(&source);

    let view = ViewModel::default();
    let mut emitted: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    emitted.insert("ViewModel", emitted_keys(&view));
    emitted.insert("PopupGeometry", emitted_keys(&PopupGeometry::default()));
    emitted.insert("ScrollGeometry", emitted_keys(&ScrollGeometry::default()));
    emitted.insert("PlaybackStatus", emitted_keys(&view.playback));
    emitted.insert("DetailView", emitted_keys(&DetailView::default()));
    emitted.insert("RowView", emitted_keys(&RowView::default()));
    emitted.insert("DetailLinkView", emitted_keys(&DetailLinkView::default()));
    emitted.insert(
        "DetailTimecodeView",
        emitted_keys(&DetailTimecodeView::default()),
    );
    emitted.insert(
        "DetailVideoLinkView",
        emitted_keys(&DetailVideoLinkView::default()),
    );
    emitted.insert(
        "DetailWikidataEntityView",
        emitted_keys(&DetailWikidataEntityView::default()),
    );
    emitted.insert("ErrorPopupView", emitted_keys(&ErrorPopupView::default()));
    emitted.insert(
        "AudioQualityPopupView",
        emitted_keys(&AudioQualityPopupView::default()),
    );
    emitted.insert(
        "YtDlpForbiddenView",
        emitted_keys(&YtDlpForbiddenView::default()),
    );
    emitted.insert(
        "YtDlpGentooVersionView",
        emitted_keys(&YtDlpGentooVersionView::default()),
    );
    emitted.insert(
        "VideoCommentsPopupView",
        emitted_keys(&VideoCommentsPopupView::default()),
    );
    emitted.insert(
        "VideoSummaryPopupView",
        emitted_keys(&VideoSummaryPopupView::default()),
    );
    emitted.insert(
        "VideoCommentView",
        emitted_keys(&VideoCommentView::default()),
    );
    emitted.insert(
        "ProjectHistoryPopupView",
        emitted_keys(&ProjectHistoryPopupView::default()),
    );
    emitted.insert(
        "ProjectCommitView",
        emitted_keys(&ProjectCommitView::default()),
    );
    emitted.insert("PreferencesPopupView", emitted_keys(&preferences()));
    emitted.insert(
        "PlaylistPopupView",
        emitted_keys(&PlaylistPopupView::default()),
    );
    emitted.insert(
        "PlaylistChoiceView",
        emitted_keys(&PlaylistChoiceView::default()),
    );
    emitted.insert(
        "LocalMoveDestinationView",
        emitted_keys(&LocalMoveDestinationView {
            name: String::new(),
            path: String::new(),
        }),
    );
    emitted.insert("DownloadView", emitted_keys(&DownloadView::default()));
    #[cfg(feature = "commons-upload")]
    {
        emitted.insert(
            "CommonsUploadPopupView",
            emitted_keys(&CommonsUploadPopupView::default()),
        );
        emitted.insert(
            "CommonsUploadDraft",
            emitted_keys(&CommonsUploadDraft::default()),
        );
        emitted.insert(
            "CommonsCategorySuggestion",
            emitted_keys(&CommonsCategorySuggestion {
                name: String::new(),
                url: url::Url::parse("https://commons.wikimedia.org/wiki/Category:Audio")
                    .expect("Commons category fixture URL"),
            }),
        );
    }
    #[cfg(feature = "evernote")]
    {
        emitted.insert(
            "EvernoteNotePopupView",
            emitted_keys(&EvernoteNotePopupView::default()),
        );
        emitted.insert(
            "EvernoteNoteDraft",
            emitted_keys(&EvernoteNoteDraft::default()),
        );
    }
    emitted.insert(
        "SubscriptionsView",
        emitted_keys(&SubscriptionsView::default()),
    );
    emitted.insert("QueuePopupView", emitted_keys(&QueuePopupView::default()));
    // `QueueRowView` has no `Default` because `MediaId` has none, and giving it
    // one would invent an identity that means nothing.
    emitted.insert(
        "QueueRowView",
        emitted_keys(&QueueRowView {
            media_id: MediaId::new(SourceKind::Local, "/music/a.flac"),
            title: String::new(),
            subtitle: String::new(),
            length: String::new(),
        }),
    );
    // `NowPlayingView` has no `Default` for the same reason `QueueRowView`
    // has none: its identity is the field that means something.
    emitted.insert(
        "NowPlayingView",
        emitted_keys(&NowPlayingView {
            media_id: MediaId::new(SourceKind::Local, "/music/a.flac"),
            title: String::new(),
            subtitle: String::new(),
        }),
    );
    emitted.insert("WaveformReady", variant_keys(&ready_waveform(), "Ready"));
    // Not a view type: the source catalogue is assembled here, and the window
    // reads it to label tabs and to decide which screens get a search field.
    emitted.insert(
        "ScreenEntry",
        emitted_keys(&super::ScreenEntry {
            id: youta::view::Screen::Search,
            label: "",
            details_kind: youta::view::InformationPanelKind::Video,
            search_verb: None,
        }),
    );

    let mut problems = Vec::new();
    for (interface, fields) in &declared {
        let Some(keys) = emitted.get(interface.as_str()) else {
            continue;
        };
        for field in fields {
            if !keys.contains(field) {
                problems.push(format!(
                    "{interface}.{field} is declared but never serialized; \
                     the reducer emits: {}",
                    keys.iter().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The interfaces this test checks must actually be present in the file.
///
/// Without this, renaming an interface would quietly remove it from coverage
/// and the check above would keep passing while checking nothing.
#[test]
fn every_checked_interface_is_actually_declared() {
    let source = contract_source();
    let declared = declared_interfaces(&source);
    for interface in [
        "ViewModel",
        "PopupGeometry",
        "ScrollGeometry",
        "PlaybackStatus",
        "DetailView",
        "RowView",
        "DetailLinkView",
        "DetailTimecodeView",
        "DetailVideoLinkView",
        "DetailWikidataEntityView",
        "ErrorPopupView",
        "AudioQualityPopupView",
        "YtDlpForbiddenView",
        "YtDlpGentooVersionView",
        "VideoCommentsPopupView",
        "VideoSummaryPopupView",
        "VideoCommentView",
        "ProjectHistoryPopupView",
        "ProjectCommitView",
        "PreferencesPopupView",
        "PlaylistPopupView",
        "PlaylistChoiceView",
        "LocalMoveDestinationView",
        "DownloadView",
        "CommonsUploadPopupView",
        "CommonsUploadDraft",
        "CommonsCategorySuggestion",
        "EvernoteNotePopupView",
        "EvernoteNoteDraft",
        "SubscriptionsView",
        "QueuePopupView",
        "QueueRowView",
        "NowPlayingView",
        "WaveformReady",
        "ScreenEntry",
    ] {
        assert!(
            declared.contains_key(interface),
            "contract.ts no longer declares {interface}"
        );
    }
}

#[test]
fn yt_dlp_lookup_variants_keep_the_window_contract_shape() {
    assert_eq!(
        serde_json::to_value(YtDlpVersionLookupView::Loading).expect("encode loading lookup"),
        serde_json::json!("Loading")
    );
    assert_eq!(
        variant_keys(
            &YtDlpVersionLookupView::Available {
                version: "2026.08.19".to_owned(),
                released_on: Some("2026-08-19".to_owned()),
            },
            "Available",
        ),
        BTreeSet::from(["released_on".to_owned(), "version".to_owned()])
    );
    assert_eq!(
        variant_keys(
            &YtDlpVersionLookupView::Unavailable {
                reason: "timed out".to_owned(),
            },
            "Unavailable",
        ),
        BTreeSet::from(["reason".to_owned()])
    );
}

#[test]
fn github_issue_submission_variants_keep_the_window_contract_shape() {
    assert_eq!(
        serde_json::to_value(GitHubIssueSubmissionView::Idle).expect("encode idle submission"),
        serde_json::json!("Idle")
    );
    assert_eq!(
        serde_json::to_value(GitHubIssueSubmissionView::Confirming)
            .expect("encode confirming submission"),
        serde_json::json!("Confirming")
    );
    assert_eq!(
        serde_json::to_value(GitHubIssueSubmissionView::Submitting)
            .expect("encode pending submission"),
        serde_json::json!("Submitting")
    );
    assert_eq!(
        variant_keys(
            &GitHubIssueSubmissionView::Submitted {
                url: "https://github.com/vitaly-zdanevich/youta/issues/123".to_owned(),
            },
            "Submitted",
        ),
        BTreeSet::from(["url".to_owned()])
    );
    assert_eq!(
        variant_keys(
            &GitHubIssueSubmissionView::OutcomeUnknown {
                issues_url: "https://github.com/vitaly-zdanevich/youta/issues".to_owned(),
            },
            "OutcomeUnknown",
        ),
        BTreeSet::from(["issues_url".to_owned()])
    );
    assert_eq!(
        variant_keys(
            &GitHubIssueSubmissionView::Failed {
                message: "gh rejected the request".to_owned(),
            },
            "Failed",
        ),
        BTreeSet::from(["message".to_owned()])
    );
}

#[test]
fn the_window_error_component_keeps_the_specialized_message_and_actions() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src")
        .join("components")
        .join("popups.tsx");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    for required in [
        "403 from yt-dlp — try later or update it.",
        "A 403 can be temporary or authentication-related.",
        "OpenYtDlpProject",
        "OpenGentooYtDlpPackage",
        "CopyErrorReport",
        "DismissErrorPopup",
    ] {
        assert!(
            source.contains(required),
            "popup component no longer contains {required}"
        );
    }
    assert!(
        source.contains("popup.yt_dlp_forbidden"),
        "the component must choose the structured body from the view"
    );
}

#[test]
fn the_window_error_component_keeps_the_confirmed_submission_flow() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src")
        .join("components")
        .join("popups.tsx");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    for required in [
        "popup.github_issue_submission",
        "RequestGitHubIssueSubmission",
        "ConfirmGitHubIssueSubmission",
        "CancelGitHubIssueSubmission",
        "OpenGitHubIssueSubmissionTarget",
        "Submit GitHub issue",
        "Retry submission",
        "GitHub issue submission failed:",
        "complete diagnostic report as a public GitHub issue",
        "dismissDisabled={submitting}",
    ] {
        assert!(
            source.contains(required),
            "popup component no longer contains {required}"
        );
    }
}

#[test]
fn the_window_error_component_hides_issue_actions_for_setup_guidance() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src")
        .join("components")
        .join("popups.tsx");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    for required in [
        "const reportable = popup.reportable;",
        "reportable && submission === \"Confirming\"",
        "reportable && (submission === \"Idle\" || failed)",
        "requestable && popup.gh_available",
        "requestable && externalOpener",
    ] {
        assert!(
            source.contains(required),
            "setup guidance no longer gates issue actions with {required}"
        );
    }
}

/// Collects every action the window names inline at a `dispatch` call.
///
/// The grammar accepted is the one the window is written in: `dispatch("Name")`
/// for an action without a payload and `dispatch({ Name: … })` for one with one,
/// the argument possibly starting on the next line. A call whose argument is a
/// variable is skipped, because the name is then not in this file to check.
fn dispatched_actions(source: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut rest = source;
    while let Some(index) = rest.find("dispatch(") {
        rest = &rest[index + "dispatch(".len()..];
        let argument = rest.trim_start();
        let name = if let Some(quoted) = argument.strip_prefix('"') {
            quoted.split('"').next()
        } else if let Some(object) = argument.strip_prefix('{') {
            object.split(':').next()
        } else {
            None
        };
        let Some(name) = name.map(str::trim) else {
            continue;
        };
        if !name.is_empty()
            && name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            names.insert(name.to_owned());
        }
    }
    names
}

/// Yields every TypeScript source the window is built from.
fn window_sources() -> Vec<(PathBuf, String)> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src");
    let mut sources = Vec::new();
    let mut directories = vec![root.clone()];
    while let Some(directory) = directories.pop() {
        let entries = std::fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()));
        for entry in entries {
            let path = entry.expect("read a window source entry").path();
            if path.is_dir() {
                directories.push(path);
            } else if matches!(
                path.extension().and_then(std::ffi::OsStr::to_str),
                Some("ts" | "tsx")
            ) {
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
                sources.push((path, text));
            }
        }
    }
    assert!(
        !sources.is_empty(),
        "no window sources found under {}",
        root.display()
    );
    sources
}

/// Whether the reducer's vocabulary contains an action by this name.
///
/// A variant that carries a payload cannot be built here without knowing what
/// the payload is, so the question asked is the narrower one this test is
/// about: does the name exist at all? Serde answers that distinctly — an
/// unrecognised name is an "unknown variant", a recognised one with the wrong
/// payload is anything else.
fn reducer_knows_action(name: &str) -> bool {
    match serde_json::from_str::<youta::view::UiAction>(&format!("{{\"{name}\":null}}")) {
        Ok(_) => true,
        Err(error) => !error.to_string().contains("unknown variant"),
    }
}

/// Every action the window names by hand must be one the reducer answers.
///
/// The reducer rejects a misspelled action rather than acting on it, and the
/// only symptom is a control that does nothing when it is clicked — which is
/// how the window first shipped a timecode that dispatched `SeekAbsoluteSeconds`,
/// a name that has never existed. Nothing in either toolchain relates the two
/// vocabularies, so this does.
#[test]
fn the_window_dispatches_only_actions_the_reducer_declares() {
    let mut checked = 0_usize;
    let mut problems = Vec::new();
    for (path, source) in window_sources() {
        for name in dispatched_actions(&source) {
            checked += 1;
            if !reducer_knows_action(&name) {
                problems.push(format!("{}: dispatches {name}", path.display()));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    // A scanner that silently stopped matching would leave this test passing
    // while checking nothing at all, which is the failure mode it exists to
    // prevent elsewhere.
    assert!(
        checked >= 20,
        "only {checked} dispatch sites were found; the scanner has stopped matching"
    );
}

#[test]
fn the_window_exposes_the_youtube_shorts_toggle_with_pressed_state() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src")
        .join("components")
        .join("Subscriptions.tsx");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    for required in [
        "aria-pressed={pressed}",
        "subscriptions.source_kind === \"you-tube\"",
        "pressed={subscriptions.show_youtube_shorts}",
        "dispatch(\"ToggleSubscriptionShorts\")",
        "Shorts: {subscriptions.show_youtube_shorts ? \"on\" : \"off\"}",
    ] {
        assert!(
            source.contains(required),
            "Subscriptions component no longer contains {required}"
        );
    }
    let refresh = source
        .find("dispatch(\"RefreshSubscriptionVideos\")")
        .expect("Refresh control");
    let shorts = source
        .find("dispatch(\"ToggleSubscriptionShorts\")")
        .expect("Shorts control");
    let details = source
        .find("dispatch(\"ToggleSubscriptionDescription\")")
        .expect("Details control");
    assert!(
        refresh < shorts && shorts < details,
        "Shorts must follow Refresh and precede Details"
    );
}

#[test]
fn the_window_virtualizes_subscription_lists_and_pages_the_focused_viewport() {
    let sources = window_sources();
    let source_named = |suffix: &str| {
        sources
            .iter()
            .find_map(|(path, source)| path.ends_with(suffix).then_some(source.as_str()))
            .unwrap_or_else(|| panic!("missing window source {suffix}"))
    };
    let app = source_named("App.tsx");
    let subscriptions = source_named("components/Subscriptions.tsx");

    for required in [
        "data-subscriptions-screen",
        "[data-subscription-pane='focused']",
        "subscriptionPageRows(height)",
    ] {
        assert!(app.contains(required), "App no longer contains {required}");
    }
    for required in [
        "useVirtualizer",
        "SUBSCRIPTION_ROW_HEIGHT",
        "virtualizer.scrollToIndex(selected, { align: 'auto' })",
        "[selected, selectedIdentity, virtualizer]",
        "data-subscription-pane={focused ? 'focused' : 'inactive'}",
        "FocusSubscriptionPane",
        "PrefetchSubscriptionVideosThrough",
        "onPointerDown={focusPane}",
        "onFocusCapture={focusPane}",
        "onScroll={() => reportViewport()}",
        "lastReportedViewportEnd",
        "[rows.length, virtualizer]",
        "event.deltaY > 0",
        "onDoubleClick={() => void dispatch(\"ActivateSelection\")}",
        "aria-current={current}",
        "aria-posinset={item.index + 1}",
        "aria-setsize={rows.length}",
        "className=\"overflow-y-auto\"",
        "role=\"region\"",
    ] {
        assert!(
            subscriptions.contains(required),
            "Subscriptions component no longer contains {required}"
        );
    }
    assert!(
        !subscriptions.contains("[focused, selected, selectedIdentity, virtualizer]"),
        "a focus-only transition must not snap a manually scrolled pane to selection"
    );
    assert!(
        !subscriptions.contains("[selected, rows.length, virtualizer]"),
        "appending a continuation must not snap native scrolling back to selection"
    );
}

#[test]
fn subscription_continuation_has_a_static_indicator_without_animating_refresh() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src")
        .join("components")
        .join("Subscriptions.tsx");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    for required in [
        "subscriptions.loading && !subscriptions.loading_more",
        "subscriptions.loading_more ?",
        "Loading more…",
        "role='status'",
    ] {
        assert!(
            source.contains(required),
            "subscription continuation UI no longer contains {required}"
        );
    }
}

#[test]
fn the_window_help_documents_the_youtube_shorts_hotkey() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src")
        .join("components")
        .join("popups.tsx");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    assert!(
        source.contains("[\"R · h\", \"refresh subscription videos · show/hide Shorts\"]"),
        "window Help must document the same contextual Shorts binding as the terminal"
    );
    assert!(
        source.contains("[\"PageUp · PageDown\", \"page through Subscriptions\"]"),
        "window Help must document subscription page navigation"
    );
}

#[test]
fn the_window_help_names_each_chapter_hotkey_direction() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src")
        .join("components")
        .join("popups.tsx");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    assert!(
        source.contains("[\"[ · ]\", \"previous · next chapter\"]"),
        "window Help must name the direction owned by each chapter key"
    );

    let player_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src")
        .join("components")
        .join("Player.tsx");
    let player = std::fs::read_to_string(&player_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", player_path.display()));
    for label in ["Previous chapter (hotkey: [)", "Next chapter (hotkey: ])"] {
        assert!(
            player.contains(label),
            "the window player must expose the {label} tooltip"
        );
    }
}

#[test]
fn the_window_exposes_local_audio_quality_analysis_and_its_result_section() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src")
        .join("components")
        .join("Details.tsx");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    for required in [
        "view.audio_quality_supported",
        "details.local_audio_quality_available",
        "details.local_audio_quality_pending",
        "dispatch(\"AnalyzeLocalAudioQuality\")",
        "Cancel analysis",
        "Analyze quality",
        "Audio quality analysis",
        "details.local_audio_quality_description",
    ] {
        assert!(
            source.contains(required),
            "Details component no longer contains {required}"
        );
    }
    let fingerprint = source
        .find("dispatch(\"FingerprintLocalAudio\")")
        .expect("fingerprint control");
    let quality = source
        .find("dispatch(\"AnalyzeLocalAudioQuality\")")
        .expect("audio quality control");
    assert!(
        fingerprint < quality,
        "audio quality analysis must immediately follow fingerprinting"
    );
}

#[test]
fn the_window_renders_dearrow_title_before_the_original_description() {
    let details = source_named("components/Details.tsx");
    let alternate = details
        .find("DeArrow title: {details.dearrow_title}")
        .expect("labelled DeArrow title");
    let description = details
        .find("text={details.description}")
        .expect("original description");

    assert!(
        alternate < description,
        "the alternate title must precede the original description"
    );
}

#[cfg(feature = "commons-upload")]
#[test]
fn the_window_exposes_commons_review_without_putting_the_hotkey_on_the_button() {
    let details = source_named("components/Details.tsx");
    for required in [
        "view.commons_upload_available",
        "dispatch(\"OpenCommonsUpload\")",
        ">Upload to Commons</Action>",
    ] {
        assert!(
            details.contains(required),
            "Commons Details control no longer contains {required}"
        );
    }
    assert!(
        !details.contains("Upload to Commons (U)")
            && !details.contains("[U] Upload to Commons")
            && !details.contains("<u>U</u>pload to Commons"),
        "the Commons button must not render its hotkey"
    );

    let popups = source_named("components/popups.tsx");
    for required in [
        "[\"U\", \"upload selected YouTube, Yandex Music, or Apple Podcasts audio to Commons\"]",
        "Youta currently uploads audio only",
        "CommonsField label=\"Title *\"",
        "OpenCommonsCategorySuggestionAt",
        "📁 {suggestion.name}",
        "<progress",
        "Thanks for preserving the history",
        "OpenCommonsUploadResult",
        "CommonsCredentialsPopup",
        "SelectCommonsCredentialField",
        "CycleCommonsAuthMethod",
        "SubmitCommonsCredentials",
        "OpenCommonsBotPasswordGuide",
        "OpenCommonsAccountRegistration",
        "~/.config/youta/secrets/credentials.toml",
        "~/.pywikibot/",
    ] {
        assert!(
            popups.contains(required),
            "Commons window flow no longer contains {required}"
        );
    }
}

#[cfg(feature = "evernote")]
#[test]
fn the_window_exposes_evernote_review_without_putting_the_hotkey_on_the_button() {
    let details = source_named("components/Details.tsx");
    for required in [
        "view.evernote_available",
        "dispatch('OpenEvernoteNote')",
        ">Save audio to Evernote</Action>",
    ] {
        assert!(
            details.contains(required),
            "Evernote Details control no longer contains {required}"
        );
    }
    assert!(
        !details.contains("Save audio to Evernote (E)")
            && !details.contains("[E] Save audio to Evernote")
            && !details.contains("<u>E</u>Save audio to Evernote"),
        "the Evernote button must not render its hotkey"
    );

    let popups = source_named("components/popups.tsx");
    for required in [
        "['E', 'save selected audio to Evernote']",
        "Youta currently saves audio only",
        "EvernoteField label='Title (optional)'",
        "EvernoteField label='Description / body'",
        "EvernoteField label='Tags (optional)'",
        "Not available for local files",
        "Add YouTube captions",
        "InsertEvernoteCaptions",
        "Ctrl+Z undoes changes to the note body.",
        "Thanks for preserving the history",
        "OpenEvernoteNoteResult",
        "EvernoteCredentialsPopup",
        "OpenEvernoteDeveloperTokenGuide",
        "SubmitEvernoteCredentials",
        "~/.config/youta/secrets/credentials.toml",
    ] {
        assert!(
            popups.contains(required),
            "Evernote window flow no longer contains {required}"
        );
    }
}

#[test]
fn the_window_keeps_radio_homepages_visible_and_openable() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src")
        .join("components")
        .join("Details.tsx");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    for required in [
        "link.presentation.startsWith(\"LabelAndUrl\")",
        "{link.url}",
        "kind === \"Radio\" ? \"Open homepage\" : \"Open page\"",
        "{details.webpage_url}",
        "{details.channel_webpage_url}",
        "dispatch({ ActivateDetailLink: index })",
        "dispatch(\"OpenInBrowser\")",
    ] {
        assert!(
            source.contains(required),
            "Radio Details no longer contains {required}"
        );
    }
}

#[test]
fn the_window_help_documents_the_local_audio_quality_hotkey() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src")
        .join("components")
        .join("popups.tsx");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    assert!(
        source.contains("[\"V\", \"analyze selected/marked files or folder\"]"),
        "window Help must document the same contextual audio-quality binding as the terminal"
    );
}

#[test]
fn the_window_audio_quality_popup_keeps_progress_copy_cancel_and_close_actions() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src")
        .join("components")
        .join("popups.tsx");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    for required in [
        "AudioQualityPopupView",
        "popup.completed",
        "popup.total",
        "popup.report",
        "popup.action_status",
        "Discovering audio files…",
        "role=\"status\"",
        "CopyAudioQualityReport",
        "CancelAudioQualityAnalysis",
        "DismissAudioQualityPopup",
        "dismissLabel={popup.pending ? \"Cancel analysis\" : \"Close\"}",
        "SetAudioQualityPopupScroll",
        "disabled={popup.report.length === 0}",
    ] {
        assert!(
            source.contains(required),
            "audio-quality popup no longer contains {required}"
        );
    }
}

#[test]
fn the_window_marks_local_batch_rows_and_hides_unsupported_quality_help() {
    let row_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src")
        .join("components")
        .join("RowList.tsx");
    let row_source = std::fs::read_to_string(&row_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", row_path.display()));
    assert!(row_source.contains("row.local_marked"));

    let popup_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src")
        .join("components")
        .join("popups.tsx");
    let popup_source = std::fs::read_to_string(&popup_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", popup_path.display()));
    let guarded_quality_help = popup_source
        .split_once("...(audioQualitySupported")
        .and_then(|(_, conditional)| conditional.split_once(": [])"))
        .map(|(supported, _)| supported)
        .expect("audio-quality Help row must remain inside a supported-only conditional");
    assert!(guarded_quality_help.contains("[\"V\", \"analyze selected/marked files or folder\"]"));
    assert!(
        popup_source.contains("[\"Shift+J · Shift+K\", \"mark Local row and move down · up\"]")
    );

    let app_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src")
        .join("App.tsx");
    let app_source = std::fs::read_to_string(&app_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", app_path.display()));
    assert!(app_source.contains("audioQualitySupported={view.audio_quality_supported}"));
}

#[test]
fn the_window_exposes_only_available_codex_video_summaries() {
    let sources = window_sources();
    let source_named = |suffix: &str| {
        sources
            .iter()
            .find_map(|(path, source)| path.ends_with(suffix).then_some(source.as_str()))
            .unwrap_or_else(|| panic!("missing window source {suffix}"))
    };
    let app = source_named("App.tsx");
    let details = source_named("components/Details.tsx");
    let popups = source_named("components/popups.tsx");

    for required in [
        "view.video_summary_available",
        "isYouTube",
        "dispatch(\"GenerateVideoSummary\")",
        "Summarize",
    ] {
        assert!(
            details.contains(required),
            "Details no longer contains {required}"
        );
    }
    for required in [
        "videoSummarySupported={view.video_summary_supported}",
        "view.video_summary_popup",
        "<VideoSummaryPopup popup={view.video_summary_popup}",
    ] {
        assert!(app.contains(required), "App no longer contains {required}");
    }
    for required in [
        "videoSummarySupported",
        "[\"G\", \"summarize selected YouTube video with Codex\"]",
    ] {
        assert!(
            popups.contains(required),
            "window Help no longer contains {required}"
        );
    }
}

#[test]
fn the_window_preferences_make_codex_summary_consent_explicit() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src")
        .join("components")
        .join("popups.tsx");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    for required in [
        "popup.video_summary_supported",
        "popup.video_summary_backend",
        "dispatch(\"CycleVideoSummaryBackend\")",
        "Video summaries",
        "authenticated Codex CLI",
        "only when you request a summary",
        "does not store an API key",
    ] {
        assert!(
            source.contains(required),
            "Preferences no longer contains {required}"
        );
    }
}

#[test]
fn the_window_video_summary_popup_keeps_progress_copy_cancel_close_and_scroll_actions() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join("src")
        .join("components")
        .join("popups.tsx");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    for required in [
        "VideoSummaryPopupView",
        "FetchingCaptions",
        "Generating",
        "Ready",
        "Cancelled",
        "state.Failed",
        "popup.caption_source",
        "popup.report",
        "popup.action_status",
        "CopyVideoSummary",
        "CancelVideoSummary",
        "DismissVideoSummary",
        "SetVideoSummaryScroll",
        "popup=\"video_summary\"",
        "disabled={!ready || popup.report.length === 0}",
    ] {
        assert!(
            source.contains(required),
            "video-summary popup no longer contains {required}"
        );
    }
}

/// Credential-bearing editors must stay out of the window's vocabulary.
///
/// `src/view.rs` skips them when serializing, so declaring them here could only
/// ever produce a type that is always `undefined` — and would signal to the next
/// reader that the window is allowed to receive them.
#[test]
fn the_contract_never_names_a_credential_bearing_editor() {
    let source = contract_source();
    for forbidden in [
        "YouTubeSetupPopupView",
        "YandexMusicSetupPopupView",
        "RssSubscriptionPopupView",
        "PrivateNotePopupView",
        "CommonsCredentialsPopupView",
        "EvernoteCredentialsPopupView",
        "youtube_setup_popup",
        "yandex_music_setup_popup",
        "rss_subscription_popup",
        "private_note_popup",
        "commons_credentials_popup",
        "evernote_credentials_popup",
    ] {
        assert!(
            !declared_interfaces(&source)
                .values()
                .any(|fields| fields.contains(forbidden))
                && !source
                    .lines()
                    .filter(|line| !line.trim_start().starts_with("//")
                        && !line.trim_start().starts_with('*'))
                    .any(|line| line.contains(forbidden)),
            "contract.ts refers to {forbidden}, which the reducer never sends"
        );
    }
}
