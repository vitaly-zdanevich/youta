//! Cheap canonical selection checks shared by explicit Funkwhale publishers.

use super::*;

/// Accepts a finite exact track page, never an artist row or a resolved media URL.
pub(super) fn supported(item: &QueueItem) -> bool {
    #[cfg(feature = "funkwhale")]
    {
        item.media.id.source == SourceKind::Funkwhale
            && item.media.kind == MediaKind::Audio
            && item.media.id.external_id == item.media.webpage_url.as_str()
            && item.playback_location == item.media.webpage_url.as_str()
            && crate::archive_upload_media::funkwhale::canonical_source(&item.media.webpage_url)
                .is_ok()
    }
    #[cfg(not(feature = "funkwhale"))]
    {
        let _ = item;
        false
    }
}
