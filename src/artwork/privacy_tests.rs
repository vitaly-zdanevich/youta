//! Existing artwork workers obey live privacy changes at each cache operation.

use super::*;

/// Previously created cache handles must neither read nor evict durable entries while private.
#[test]
fn disabled_artwork_cache_preserves_saved_bytes_and_isolates_new_entries() {
    let root = tempfile::tempdir().unwrap();
    let config = crate::config::Config::for_dir(root.path().join("config"));
    let cache = ThumbnailCache::new(config.activity_thumbnail_dir());
    cache.store_key(b"saved", b"saved artwork").unwrap();
    let old_path = cache.entry_path_for_key(b"saved");
    let old_bytes = fs::read(&old_path).unwrap();
    config.set_activity_persistence(false).unwrap();
    assert_eq!(cache.read_key(b"saved").unwrap(), None);
    cache.store_key(b"private", b"private artwork").unwrap();
    cache.remove_key(b"saved");
    cache.evict().unwrap();
    assert_eq!(fs::read(&old_path).unwrap(), old_bytes);
    assert_eq!(
        fs::read_dir(config.thumbnail_cache_dir()).unwrap().count(),
        1
    );
    assert_eq!(
        cache.read_key(b"private").unwrap(),
        Some(b"private artwork".to_vec())
    );
    config.set_activity_persistence(true).unwrap();
    assert_eq!(cache.read_key(b"saved").unwrap(), Some(old_bytes));
    assert_eq!(cache.read_key(b"private").unwrap(), None);
}

/// A queued worker resolves its cache after the preference changes, never at queue time.
#[test]
fn queued_artwork_write_uses_the_current_privacy_policy() {
    let root = tempfile::tempdir().unwrap();
    let config = crate::config::Config::for_dir(root.path().join("config"));
    let cache = ThumbnailCache::new(config.activity_thumbnail_dir());
    let (release, delayed) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        delayed.recv().unwrap();
        cache.store_key(b"delayed", b"private artwork").unwrap();
        assert_eq!(
            cache.read_key(b"delayed").unwrap(),
            Some(b"private artwork".to_vec())
        );
    });
    config.set_activity_persistence(false).unwrap();
    release.send(()).unwrap();
    worker.join().unwrap();
    assert!(!config.thumbnail_cache_dir().exists());
}

/// Finishing a request after logging resumes never makes its private-origin bytes durable.
#[test]
fn private_artwork_operation_stays_private_after_reenable() {
    let root = tempfile::tempdir().unwrap();
    let config = crate::config::Config::for_dir(root.path().join("config"));
    config.set_activity_persistence(false).unwrap();
    let cache = ThumbnailCache::new(config.activity_thumbnail_dir()).for_operation();
    config.set_activity_persistence(true).unwrap();
    cache.store_key(b"private", b"private artwork").unwrap();
    assert_eq!(
        cache.read_key(b"private").unwrap(),
        Some(b"private artwork".to_vec())
    );
    assert!(!config.thumbnail_cache_dir().exists());
}
