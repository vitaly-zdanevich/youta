//! Display-only home-directory abbreviation; never use these labels as locators.

use std::path::{Component, Path};

/// Formats a local path without filesystem access or changes to its identity.
///
/// Only complete home-directory components may become `~`. Unknown homes,
/// relative paths, and paths outside the home directory retain their spelling.
pub(crate) fn display_path(path: &Path, home: Option<&Path>, show_full: bool) -> String {
    let full = || path.display().to_string();
    let Some(home) = home.filter(|home| home.is_absolute()) else {
        return full();
    };
    if show_full || !path.is_absolute() {
        return full();
    }
    let Ok(relative) = path.strip_prefix(home) else {
        return full();
    };
    if relative
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return full();
    }
    if relative.as_os_str().is_empty() {
        "~".to_owned()
    } else {
        format!("~{}{}", std::path::MAIN_SEPARATOR, relative.display())
    }
}

#[cfg(test)]
mod controller_tests {
    use super::super::*;

    /// Supplies metadata-only rows under the detected home without creating user files.
    fn fixture() -> (tempfile::TempDir, AppController, PathBuf) {
        let temporary = crate::test_support::canonical_tempdir("local path preferences");
        let config = Config::for_dir(temporary.path().join("config"));
        let mut controller = AppController::new(
            config,
            StateStore::open_in_memory().expect("state"),
            None,
            None,
        );
        let home = directories::BaseDirs::new().expect("test home directory");
        let directory = home.home_dir().join("youta-path-display-fixture");
        let track = directory.join("track.opus");
        controller.view.screen = Screen::Local;
        controller.local_listing = Some(crate::local_browser::LocalDirectoryListing {
            path: directory,
            parent: Some(home.home_dir().to_owned()),
            entries: vec![crate::local_browser::LocalEntry {
                name: "track.opus".into(),
                path: track.clone(),
                kind: crate::local_browser::LocalEntryKind::Audio,
                size_bytes: Some(128),
                image_dimensions: None,
                directory_identity: None,
            }],
            truncated: false,
            inspected_entries: 1,
        });
        controller.view.selected = 1;
        controller.refresh_local_browser_rows();
        (temporary, controller, track)
    }

    /// Display abbreviation cannot leak into media IDs, replay paths, or restart state.
    #[test]
    fn abbreviated_local_details_preserve_absolute_identity_and_session() {
        let (_temporary, mut controller, track) = fixture();
        let short = Path::new("~")
            .join("youta-path-display-fixture")
            .join("track.opus");
        assert!(
            controller
                .view
                .details
                .as_ref()
                .unwrap()
                .description
                .starts_with(&format!("Full path:\n{}\n", short.display()))
        );
        assert_eq!(
            controller.view.rows[1].media_id,
            Some(local_media_id(&track))
        );
        assert_eq!(controller.selected_local_path(), Some(track.clone()));
        let item = local_media_item_stub(track.clone(), Some(128));
        let playback = queue_item_from_local(&item).expect("unchanged replay target");
        assert_eq!(
            playback.playback_location,
            url::Url::from_file_path(&track).unwrap().to_string()
        );
        assert!(local_media_description(&item).contains(&track.display().to_string()));
        assert!(controller.save_session());
        assert_eq!(
            controller.store.session().unwrap().unwrap().local_path,
            Some(track.parent().unwrap().display().to_string())
        );
    }

    /// Preferences are drafted, cancellable, durable, and immediately reflected in Details.
    #[test]
    fn full_local_paths_preference_applies_only_after_save() {
        let (_temporary, mut controller, track) = fixture();
        let abbreviated = controller
            .view
            .details
            .as_ref()
            .unwrap()
            .description
            .clone();
        controller.open_preferences();
        controller
            .view
            .preferences_popup
            .as_mut()
            .unwrap()
            .environment_override = None;
        controller.dispatch(UiAction::ToggleFullLocalPaths);
        assert!(
            controller
                .view
                .preferences_popup
                .as_ref()
                .unwrap()
                .show_full_local_paths
        );
        assert!(!controller.config.ui.show_full_local_paths);
        assert_eq!(
            controller.view.details.as_ref().unwrap().description,
            abbreviated
        );
        controller.dispatch(UiAction::DismissPreferences);
        assert!(!controller.config.ui.show_full_local_paths);

        controller.open_preferences();
        controller
            .view
            .preferences_popup
            .as_mut()
            .unwrap()
            .environment_override = None;
        controller.dispatch(UiAction::ToggleFullLocalPaths);
        controller.submit_preferences();
        assert!(controller.view.preferences_popup.is_none());
        assert!(controller.config.ui.show_full_local_paths);
        assert!(
            controller
                .view
                .details
                .as_ref()
                .unwrap()
                .description
                .starts_with(&format!("Full path:\n{}\n", track.display()))
        );
        assert!(
            Config::load_from_dir(controller.config.config_dir())
                .unwrap()
                .ui
                .show_full_local_paths
        );
        assert_eq!(controller.selected_local_path(), Some(track));

        controller.open_preferences();
        controller
            .view
            .preferences_popup
            .as_mut()
            .unwrap()
            .environment_override = None;
        controller.dispatch(UiAction::ToggleFullLocalPaths);
        controller.submit_preferences();
        assert_eq!(
            controller.view.details.as_ref().unwrap().description,
            abbreviated
        );
        assert!(
            !Config::load_from_dir(controller.config.config_dir())
                .unwrap()
                .ui
                .show_full_local_paths
        );
    }

    /// Non-playable entries and parent navigation use the same home abbreviation.
    #[test]
    fn local_folder_and_parent_details_use_the_path_preference() {
        let (_temporary, mut controller, track) = fixture();
        controller.view.selected = 0;
        controller.update_local_browser_detail();
        assert_eq!(
            controller.view.details.as_ref().unwrap().description,
            "Full path:\n~"
        );
        controller.local_listing.as_mut().unwrap().entries[0].kind =
            crate::local_browser::LocalEntryKind::Directory;
        controller.view.selected = 1;
        controller.update_local_browser_detail();
        let short = Path::new("~")
            .join("youta-path-display-fixture")
            .join("track.opus");
        assert_eq!(
            controller.view.details.as_ref().unwrap().description,
            format!("Full path:\n{}", short.display())
        );
        controller.config.ui.show_full_local_paths = true;
        controller.update_local_browser_detail();
        assert_eq!(
            controller.view.details.as_ref().unwrap().description,
            format!("Full path:\n{}", track.display())
        );
    }

    /// Folder and non-playable file paths each get the full next line in both display modes.
    #[test]
    fn non_playable_local_paths_start_below_the_heading() {
        use crate::local_browser::LocalEntryKind;

        let (_temporary, mut controller, track) = fixture();
        for kind in [
            LocalEntryKind::Directory,
            LocalEntryKind::Image,
            LocalEntryKind::Text,
            LocalEntryKind::Other,
        ] {
            controller.local_listing.as_mut().unwrap().entries[0].kind = kind;
            for show_full in [false, true] {
                controller.config.ui.show_full_local_paths = show_full;
                controller.update_local_browser_detail();
                let description = &controller.view.details.as_ref().unwrap().description;
                let expected_path = controller.local_display_path(&track);
                let mut lines = description.lines();
                assert_eq!(lines.next(), Some("Full path:"));
                assert_eq!(lines.next(), Some(expected_path.as_str()));
                assert_eq!(controller.selected_local_path(), Some(track.clone()));
            }
        }
    }

    /// Offline files use the same two-line presentation without changing their replay identity.
    #[test]
    fn downloaded_local_paths_start_below_the_heading() {
        let (_temporary, mut controller, track) = fixture();
        let media_id = local_media_id(&track);
        controller.view.screen = Screen::Downloaded;
        controller.view.rows = vec![RowView {
            media_id: Some(media_id.clone()),
            title: "Downloaded track".to_owned(),
            ..RowView::default()
        }];
        controller.view.selected = 0;
        for show_full in [false, true] {
            controller.config.ui.show_full_local_paths = show_full;
            controller.update_downloaded_detail();
            let details = controller.view.details.as_ref().unwrap();
            assert_eq!(
                details.description,
                format!("Full path:\n{}", controller.local_display_path(&track))
            );
            assert_eq!(details.media_id.as_ref(), Some(&media_id));
        }
    }

    /// Archive members show their logical source, not a private extraction-cache path.
    #[cfg(feature = "local-archives")]
    #[test]
    fn local_archive_details_abbreviate_the_original_archive_location() {
        let (_temporary, mut controller, track) = fixture();
        let root = controller.config.cache_dir().join("archive-fixture");
        let source = track.with_file_name("album.zip");
        let member = root.join("track.opus");
        controller
            .local_archive_stack
            .push(crate::local_archive::MaterializedLocalArchive {
                source_path: source.clone(),
                root_path: root.clone(),
                reused_cache: false,
            });
        let listing = controller.local_listing.as_mut().unwrap();
        listing.path = root;
        listing.entries[0].path = member.clone();
        controller.refresh_local_browser_rows();
        let short_source = Path::new("~")
            .join("youta-path-display-fixture")
            .join("album.zip");
        assert!(
            controller
                .view
                .details
                .as_ref()
                .unwrap()
                .description
                .starts_with(&format!(
                    "Full path:\n{}!/track.opus\n",
                    short_source.display()
                ))
        );
        assert_eq!(controller.selected_local_path(), Some(member));
        assert_eq!(
            controller.view.local_path,
            format!("{}!/", source.display())
        );
    }
}

#[cfg(test)]
mod tests {
    use super::display_path;
    use std::path::{Path, PathBuf};

    /// Provides a platform-native absolute home without accessing the filesystem.
    fn home() -> PathBuf {
        std::env::temp_dir().join("youta-fixture-home")
    }

    /// Home and its descendants use shell-style labels, including Unicode names.
    #[test]
    fn abbreviates_home_and_descendants_by_default() {
        let home = home();
        assert_eq!(display_path(&home, Some(&home), false), "~");
        assert_eq!(
            display_path(&home.join("Music/Песня.flac"), Some(&home), false),
            format!("~{}Music/Песня.flac", std::path::MAIN_SEPARATOR)
        );
    }

    /// A full-path preference leaves the entire original spelling intact.
    #[test]
    fn full_path_preference_disables_abbreviation() {
        let home = home();
        let path = home.join("Music/track.flac");
        assert_eq!(
            display_path(&path, Some(&home), true),
            path.display().to_string()
        );
        assert_eq!(
            display_path(&home, Some(&home), true),
            home.display().to_string()
        );
    }

    /// Shared username prefixes must not mistake another user's files for home.
    #[test]
    fn preserves_paths_outside_home_and_missing_home() {
        let home = home();
        for path in [
            home.with_file_name("youta-fixture-home2")
                .join("track.flac"),
            PathBuf::from("/media/music/track.flac"),
            PathBuf::from("Music/track.flac"),
        ] {
            assert_eq!(
                display_path(&path, Some(&home), false),
                path.display().to_string()
            );
        }
        assert_eq!(display_path(&home, None, false), home.display().to_string());
    }

    /// Invalid home hints and unresolved traversal do not produce misleading labels.
    #[test]
    fn preserves_paths_with_unknown_boundaries() {
        for home in [Path::new(""), Path::new("home/listener")] {
            assert_eq!(
                display_path(Path::new("Music/song.ogg"), Some(home), false),
                "Music/song.ogg"
            );
        }
        let home = home();
        let path = home.join("../elsewhere/song.ogg");
        assert_eq!(
            display_path(&path, Some(&home), false),
            path.display().to_string()
        );
    }

    /// Archive labels preserve every member component after abbreviating the source.
    #[test]
    fn preserves_nested_archive_member_labels() {
        let home = home();
        let path = home.join("Music/album.zip!/disc.rar!/track.opus");
        assert_eq!(
            display_path(&path, Some(&home), false),
            format!(
                "~{}Music/album.zip!/disc.rar!/track.opus",
                std::path::MAIN_SEPARATOR
            )
        );
    }
}
