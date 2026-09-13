//! The video-upload choice is durable, independently of any upload credentials.

use youta::config::Config;

#[test]
fn archive_video_upload_defaults_to_audio() {
    let directory = tempfile::tempdir().expect("configuration directory");
    let config = Config::for_dir(directory.path());
    let serialized = serde_json::to_value(config).expect("public settings");
    assert_eq!(serialized["archive_upload"]["upload_video"], false);
}

#[cfg(feature = "archive-upload")]
#[test]
fn archive_video_choice_round_trips_and_keeps_other_settings() {
    let directory = tempfile::tempdir().expect("configuration directory");
    let path = directory.path().join("config.toml");
    std::fs::write(&path, "# user comment\n[playback]\nvolume_percent = 37\n").unwrap();
    let mut config = Config::load_from_dir(directory.path()).unwrap();
    for choice in [true, false] {
        config.save_archive_upload_video(choice).unwrap();
        let reopened = Config::load_from_dir(directory.path()).unwrap();
        assert_eq!(reopened.archive_upload.upload_video, choice);
        assert_eq!(reopened.playback.volume_percent, 37);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("# user comment")
        );
    }
}

#[cfg(feature = "archive-upload")]
#[test]
fn failed_archive_video_write_keeps_memory_and_file_unchanged() {
    let directory = tempfile::tempdir().expect("configuration directory");
    let path = directory.path().join("config.toml");
    let contents = "archive_upload = 42\n";
    std::fs::write(&path, contents).unwrap();
    let mut config = Config::for_dir(directory.path());
    assert!(config.save_archive_upload_video(true).is_err());
    assert!(!config.archive_upload.upload_video);
    assert_eq!(std::fs::read_to_string(path).unwrap(), contents);
}

#[test]
fn stored_archive_video_choice_loads_even_when_upload_support_is_disabled() {
    let directory = tempfile::tempdir().expect("configuration directory");
    std::fs::write(
        directory.path().join("config.toml"),
        "[archive_upload]\nupload_video = true\n",
    )
    .unwrap();
    let config = Config::load_from_dir(directory.path()).unwrap();
    assert!(config.archive_upload.upload_video);
}
