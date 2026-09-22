//! Public restart snapshots reuse bounded Archive Back navigation, without retaining caches.

use super::*;
use crate::domain::ArchiveOrgSessionLocation;

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
            filename: active
                .tracks
                .get(self.archive_org_selected)
                .map(|track| track.filename.clone()),
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
