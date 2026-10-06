//! Filesystem dates for selected Local files, without exposing extraction-cache dates.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};

/// Reads only the selected file's dates, keeping its display path separate from its locator.
///
/// Creation means filesystem birth time, never Unix metadata-change time. Filesystems
/// without that information and failed reads produce an explicit unavailable value.
pub(super) fn path_description(path: &Path, label: &str) -> String {
    let metadata = std::fs::metadata(path).ok();
    let created = metadata
        .as_ref()
        .and_then(|metadata| metadata.created().ok());
    let modified = metadata
        .as_ref()
        .and_then(|metadata| metadata.modified().ok());
    format!(
        "Full path:\n{label}\nCreated: {}\nModified: {}",
        format_timestamp(created, &Local),
        format_timestamp(modified, &Local),
    )
}

/// Formats an absolute date in an injected timezone without guessing missing timestamps.
///
/// Checked epoch conversion also handles pre-1970 fractional seconds and filesystem
/// timestamps beyond Chrono's supported range without panicking or displaying epoch zero.
fn format_timestamp(time: Option<SystemTime>, timezone: &impl TimeZone) -> String {
    let timestamp = time.and_then(|time| {
        let seconds = match time.duration_since(UNIX_EPOCH) {
            Ok(duration) => i64::try_from(duration.as_secs()).ok()?,
            Err(error) => {
                let duration = error.duration();
                i64::try_from(duration.as_secs())
                    .ok()?
                    .checked_neg()?
                    .checked_sub(i64::from(duration.subsec_nanos() != 0))?
            }
        };
        DateTime::from_timestamp(seconds, 0)
    });
    let Some(date) = timestamp.map(|timestamp| timestamp.with_timezone(timezone)) else {
        return "unavailable".to_owned();
    };
    let date_label = super::format_civil_date(
        i64::from(date.year()),
        i64::from(date.month()),
        i64::from(date.day()),
    )
    .expect("Chrono supplies a valid calendar date");
    format!("{date_label} {:02}:{:02}", date.hour(), date.minute())
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::format_timestamp;
    use chrono::TimeZone;

    /// Absolute English dates use the injected local offset, including calendar rollover.
    #[test]
    fn local_file_dates_format_calendar_clock_and_unavailable_values() {
        let instant: SystemTime = chrono::Utc
            .with_ymd_and_hms(2026, 8, 25, 23, 5, 59)
            .single()
            .unwrap()
            .into();
        let offset = chrono::FixedOffset::east_opt(3 * 3600).unwrap();
        assert_eq!(
            format_timestamp(Some(instant), &offset),
            "2026 August 26 02:05"
        );
        assert_eq!(format_timestamp(None, &offset), "unavailable");
        assert_eq!(
            format_timestamp(
                UNIX_EPOCH.checked_sub(Duration::from_nanos(1)),
                &chrono::Utc
            ),
            "1969 December 31 23:59"
        );
        if let Some(out_of_range) = UNIX_EPOCH.checked_add(Duration::from_secs(10_000_000_000_000))
        {
            assert_eq!(
                format_timestamp(Some(out_of_range), &chrono::Utc),
                "unavailable"
            );
        }
    }

    /// Builds a selected file with real filesystem dates and no metadata subprocess.
    fn fixture(
        kind: crate::local_browser::LocalEntryKind,
    ) -> (tempfile::TempDir, AppController, PathBuf) {
        let temporary = crate::test_support::canonical_tempdir("Local file dates");
        let path = temporary.path().join("track.flac");
        let file = std::fs::File::create(&path).expect("fixture file");
        let modified = Local
            .with_ymd_and_hms(2026, 8, 25, 14, 20, 0)
            .single()
            .expect("local fixture date");
        file.set_times(std::fs::FileTimes::new().set_modified(modified.into()))
            .expect("set fixture mtime");
        let mut controller = AppController::new(
            Config::for_dir(temporary.path().join("config")),
            StateStore::open_in_memory().expect("state"),
            None,
            None,
        );
        controller.view.screen = Screen::Local;
        controller.local_listing = Some(crate::local_browser::LocalDirectoryListing {
            path: temporary.path().to_owned(),
            parent: None,
            entries: vec![crate::local_browser::LocalEntry {
                name: "track.flac".into(),
                path: path.clone(),
                kind,
                size_bytes: Some(0),
                image_dimensions: None,
                directory_identity: None,
            }],
            truncated: false,
            inspected_entries: 1,
        });
        (temporary, controller, path)
    }

    /// Playable and ordinary files share the same dates directly below the path.
    #[test]
    fn local_file_dates_follow_the_path_for_all_file_kinds() {
        use crate::local_browser::LocalEntryKind;
        for kind in [
            LocalEntryKind::Audio,
            LocalEntryKind::Video,
            LocalEntryKind::TrackerModule,
            LocalEntryKind::Image,
            LocalEntryKind::Text,
            LocalEntryKind::Other,
        ] {
            let (_temporary, mut controller, path) = fixture(kind);
            controller.update_local_browser_detail();
            let details = controller.view.details.as_ref().expect("file Details");
            let lines = details.description.lines().collect::<Vec<_>>();
            assert_eq!(
                &lines[..2],
                ["Full path:", controller.local_display_path(&path).as_str()]
            );
            let created = std::fs::metadata(&path)
                .unwrap()
                .created()
                .ok()
                .map(|time| {
                    DateTime::<Local>::from(time)
                        .format("%Y %B %-d %H:%M")
                        .to_string()
                })
                .unwrap_or_else(|| "unavailable".to_owned());
            assert_eq!(lines[2], format!("Created: {created}"));
            assert_eq!(lines[3], "Modified: 2026 August 25 14:20");
            assert!(
                details.timecodes.is_empty(),
                "filesystem clock times are not seek targets"
            );
        }
    }

    /// Reopening the selected file refreshes dates rather than caching a stale media probe.
    #[test]
    fn local_file_dates_refresh_and_handle_missing_files() {
        let (_temporary, mut controller, path) =
            fixture(crate::local_browser::LocalEntryKind::Audio);
        controller.update_local_browser_detail();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new().set_modified(
                    Local
                        .with_ymd_and_hms(2026, 8, 26, 9, 5, 0)
                        .single()
                        .unwrap()
                        .into(),
                ),
            )
            .unwrap();
        controller.update_local_browser_detail();
        assert!(
            controller
                .view
                .details
                .as_ref()
                .unwrap()
                .description
                .contains("Modified: 2026 August 26 09:05")
        );
        std::fs::remove_file(path).unwrap();
        controller.update_local_browser_detail();
        assert!(
            controller
                .view
                .details
                .as_ref()
                .unwrap()
                .description
                .contains("\nCreated: unavailable\nModified: unavailable\n")
        );
    }

    /// Offline audio and video descriptions use the same selected-file facts.
    #[test]
    fn local_file_dates_are_shown_for_downloaded_files() {
        let (_temporary, mut controller, path) =
            fixture(crate::local_browser::LocalEntryKind::Audio);
        controller.view.screen = Screen::Downloaded;
        controller.view.rows = vec![RowView {
            media_id: Some(local_media_id(&path)),
            ..RowView::default()
        }];
        controller.update_downloaded_detail();
        let description = &controller.view.details.as_ref().unwrap().description;
        assert!(description.contains("\nCreated: "));
        assert!(description.ends_with("\nModified: 2026 August 25 14:20"));
    }

    /// An archive member must not inherit the creation or modification time of its cache copy.
    #[cfg(feature = "local-archives")]
    #[test]
    fn local_file_dates_do_not_describe_archive_extraction_cache() {
        for kind in [
            crate::local_browser::LocalEntryKind::Audio,
            crate::local_browser::LocalEntryKind::Text,
        ] {
            let (temporary, mut controller, _path) = fixture(kind);
            controller
                .local_archive_stack
                .push(crate::local_archive::MaterializedLocalArchive {
                    source_path: temporary.path().join("album.zip"),
                    root_path: temporary.path().to_owned(),
                    reused_cache: false,
                });
            controller.local_listing.as_mut().unwrap().parent =
                temporary.path().parent().map(Path::to_owned);
            controller.view.selected = 1;
            controller.update_local_browser_detail();
            let description = &controller.view.details.as_ref().unwrap().description;
            assert!(description.contains("album.zip!/track.flac"));
            assert!(!description.contains("Created:") && !description.contains("Modified:"));
        }
    }

    /// A parked archive route cannot hide the dates of unrelated real files.
    #[cfg(feature = "local-archives")]
    #[test]
    fn local_file_dates_remain_available_outside_a_parked_archive() {
        let (temporary, mut controller, path) =
            fixture(crate::local_browser::LocalEntryKind::Archive);
        controller
            .local_archive_stack
            .push(crate::local_archive::MaterializedLocalArchive {
                source_path: path.clone(),
                root_path: temporary.path().join("extracted"),
                reused_cache: false,
            });
        controller.update_local_browser_detail();
        assert!(
            controller
                .view
                .details
                .as_ref()
                .unwrap()
                .description
                .contains("Modified: 2026 August 25 14:20")
        );
        controller.view.screen = Screen::Downloaded;
        controller.view.rows = vec![RowView {
            media_id: Some(local_media_id(&path)),
            ..RowView::default()
        }];
        controller.update_downloaded_detail();
        assert!(
            controller
                .view
                .details
                .as_ref()
                .unwrap()
                .description
                .contains("Modified: 2026 August 25 14:20")
        );
    }

    /// Reopened private cache copies retain the archive exclusion after route state is gone.
    #[test]
    fn local_file_dates_omit_known_cache_members_without_an_archive_route() {
        let (_temporary, mut controller, _) = fixture(crate::local_browser::LocalEntryKind::Audio);
        let root = controller
            .config
            .cache_dir()
            .join("local-archives")
            .join("fixture");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("member.flac");
        std::fs::write(&path, b"fixture").unwrap();
        controller.view.screen = Screen::Downloaded;
        controller.view.rows = vec![RowView {
            media_id: Some(local_media_id(&path)),
            ..RowView::default()
        }];
        controller.update_downloaded_detail();
        let description = &controller.view.details.as_ref().unwrap().description;
        assert!(!description.contains("Created:") && !description.contains("Modified:"));
    }
}
