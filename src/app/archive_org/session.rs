//! Public restart snapshots reuse bounded Archive Back navigation, without retaining caches.

use super::*;
use crate::domain::ArchiveOrgSessionLocation;

/// Identifies either a playable file or a non-playable ZIP row in the item listing.
pub(super) fn selected_filename(details: &ArchiveOrgItemDetails, index: usize) -> Option<String> {
    details
        .tracks
        .get(index)
        .map(|track| track.filename.clone())
        .or_else(|| {
            details
                .archives
                .get(index.checked_sub(details.tracks.len())?)
                .map(|archive| archive.filename.clone())
        })
}

/// Restores exact file identity across changes in track and ZIP ordering.
pub(super) fn filename_index(details: &ArchiveOrgItemDetails, filename: &str) -> Option<usize> {
    details
        .tracks
        .iter()
        .position(|track| track.filename == filename)
        .or_else(|| {
            details
                .archives
                .iter()
                .position(|archive| archive.filename == filename)
                .map(|index| details.tracks.len() + index)
        })
}

impl ArchiveOrgState {
    /// Parks valid public navigation until the frontend's first Archive tick.
    pub(in crate::app) fn restored(saved: &SessionState) -> Self {
        Self {
            restart: saved
                .archive_org_location
                .clone()
                .filter(ArchiveOrgSessionLocation::is_valid),
            ..Self::default()
        }
    }
}

impl AppController {
    /// Captures the logical destination even while its metadata is still loading.
    ///
    /// Only provider identifiers and exact relative filenames are durable; metadata,
    /// image URLs, credentials, and playback state are deliberately excluded.
    pub(in crate::app) fn archive_org_session_location(&self) -> Option<ArchiveOrgSessionLocation> {
        if let Some(location) = &self.archive_org.restart {
            return Some(location.clone());
        }
        if let Some(location) = &self.archive_org.restoring {
            return location.session_location();
        }
        let active = self.archive_org.active.as_ref()?;
        let opening_zip = self
            .archive_org
            .pending
            .as_ref()
            .and_then(|job| match &job.kind {
                ArchiveRequest::Zip {
                    parent,
                    archive,
                    open: true,
                } if parent.item.identifier == active.item.identifier => Some(archive),
                _ => None,
            });
        let location = ArchiveOrgSessionLocation {
            query: self.archive_org.submitted_query.clone(),
            scope: self.archive_org.submitted_scope,
            catalogue_selected: self.archive_org.search_selected,
            catalogue_identifier: self
                .archive_org
                .items
                .get(self.archive_org.search_selected)
                .map(|item| item.identifier.clone()),
            identifier: active.item.identifier.clone(),
            archive_filename: opening_zip
                .map(|archive| archive.filename.clone())
                .or_else(|| active.archive_filename.clone()),
            filename: if opening_zip.is_some() {
                None
            } else {
                selected_filename(active, self.archive_org_selected)
            },
        };
        location.is_valid().then_some(location)
    }

    /// Begins the existing bounded catalogue/item restore path, never playback.
    pub(super) fn begin_archive_session_restore(&mut self) -> bool {
        let Some(location) = self.archive_org.restart.take() else {
            return false;
        };
        self.archive_org
            .history
            .push_back(history::ArchiveLocation::from_session(location));
        self.restore_archive_location()
    }
}
