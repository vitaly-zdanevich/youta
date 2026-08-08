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

use youta::domain::{MediaId, SourceKind};
use youta::view::{
    DetailLinkView, DetailTimecodeView, DetailVideoLinkView, DetailView, DetailWikidataEntityView,
    DownloadView, ErrorPopupView, LocalMoveDestinationView, PlaylistChoiceView, PlaylistPopupView,
    PreferencesPopupView, ProjectCommitView, ProjectHistoryPopupView, RowView, SubscriptionsView,
    VideoCommentView, VideoCommentsPopupView, ViewModel, WaveformView,
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
        skip_advertisement_chapters: false,
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

/// Every declared field must exist in the JSON the reducer publishes.
#[test]
fn the_typescript_contract_names_only_fields_the_reducer_emits() {
    let source = contract_source();
    let declared = declared_interfaces(&source);

    let view = ViewModel::default();
    let mut emitted: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    emitted.insert("ViewModel", emitted_keys(&view));
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
        "VideoCommentsPopupView",
        emitted_keys(&VideoCommentsPopupView::default()),
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
    emitted.insert(
        "SubscriptionsView",
        emitted_keys(&SubscriptionsView::default()),
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
        "PlaybackStatus",
        "DetailView",
        "RowView",
        "DetailLinkView",
        "DetailTimecodeView",
        "DetailVideoLinkView",
        "DetailWikidataEntityView",
        "ErrorPopupView",
        "VideoCommentsPopupView",
        "VideoCommentView",
        "ProjectHistoryPopupView",
        "ProjectCommitView",
        "PreferencesPopupView",
        "PlaylistPopupView",
        "PlaylistChoiceView",
        "LocalMoveDestinationView",
        "DownloadView",
        "SubscriptionsView",
        "WaveformReady",
        "ScreenEntry",
    ] {
        assert!(
            declared.contains_key(interface),
            "contract.ts no longer declares {interface}"
        );
    }
}

/// The four credential-bearing editors must stay out of the window's vocabulary.
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
        "youtube_setup_popup",
        "yandex_music_setup_popup",
        "rss_subscription_popup",
        "private_note_popup",
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
