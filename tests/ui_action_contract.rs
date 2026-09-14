//! Checks frontend action serialization without native desktop dependencies.

#![cfg(feature = "controller")]

#[path = "support/ui_action_contract.rs"]
mod action_contract;

/// Core builds can omit source features that the native window always enables.
#[test]
fn frontend_action_payload_fixtures_match_the_reducer() {
    action_contract::assert_fixture_contract(|name| {
        action_contract::action_belongs_to_disabled_feature(name, |feature| match feature {
            "ascii-visualizer" => cfg!(feature = "ascii-visualizer"),
            "commons-upload" => cfg!(feature = "commons-upload"),
            "s3-upload" => cfg!(feature = "s3-upload"),
            "archive-upload" => cfg!(feature = "archive-upload"),
            "evernote" => cfg!(feature = "evernote"),
            "youtube-captions" => cfg!(feature = "youtube-captions"),
            "lan-sharing" => cfg!(feature = "lan-sharing"),
            "qr" => cfg!(feature = "qr"),
            "yt-dlp" => cfg!(feature = "yt-dlp"),
            "yandex-music" => cfg!(feature = "yandex-music"),
            _ => panic!("unknown action feature {feature}"),
        })
    });
}
