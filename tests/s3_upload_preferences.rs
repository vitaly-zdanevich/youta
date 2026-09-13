//! Non-secret S3 defaults survive restart without authorizing an upload.

#![cfg(feature = "s3-upload")]

use youta::config::{Config, S3UploadConfig};

#[test]
fn s3_choices_preserve_unrelated_config_and_never_store_keys() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(&path, "# keep this comment\n[ui]\nshow_images = false\n").unwrap();
    let mut config = Config::for_dir(directory.path());
    let settings = S3UploadConfig {
        bucket: "my-audio".to_owned(),
        region: "eu-central-1".to_owned(),
        profile: "personal".to_owned(),
        upload_video: true,
    };
    config.save_s3_upload_choices(settings.clone()).unwrap();
    assert_eq!(config.s3_upload, settings);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# keep this comment"));
    assert!(text.contains("show_images = false"));
    assert!(!text.contains("secret"));
    assert!(!text.contains("access_key"));
    assert!(!text.contains("object_key"));
    let document: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(document["s3_upload"]["bucket"].as_str(), Some("my-audio"));
    assert_eq!(document["s3_upload"]["upload_video"].as_bool(), Some(true));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn malformed_s3_config_does_not_change_memory_or_disk() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let original = "s3_upload = 'not a table'\n";
    std::fs::write(&path, original).unwrap();
    let mut config = Config::for_dir(directory.path());
    let previous = config.s3_upload.clone();
    assert!(
        config
            .save_s3_upload_choices(S3UploadConfig {
                upload_video: true,
                ..S3UploadConfig::default()
            })
            .is_err()
    );
    assert_eq!(config.s3_upload, previous);
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}

#[test]
fn default_s3_choices_do_not_select_a_destination_or_enable_video() {
    let settings = S3UploadConfig::default();
    assert!(settings.bucket.is_empty());
    assert!(settings.region.is_empty());
    assert!(settings.profile.is_empty());
    assert!(!settings.upload_video);
}
