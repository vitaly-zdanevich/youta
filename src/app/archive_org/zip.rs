//! Lazy ZIP navigation uses the existing Archive worker and exact remote-file identities.

use super::*;

/// One bounded ZIP cache entry cannot replace its enclosing item's metadata.
pub(super) struct ArchiveZipCacheEntry {
    identifier: String,
    filename: String,
    result: Result<Arc<ArchiveOrgItemDetails>, String>,
}

/// Extracts the authoritative metadata destination, excluding catalogue requests.
pub(super) fn request_location(request: &ArchiveRequest) -> Option<(&str, Option<&str>)> {
    match request {
        ArchiveRequest::Details { identifier, .. } => Some((identifier, None)),
        ArchiveRequest::Zip {
            parent, archive, ..
        } => Some((&parent.item.identifier, Some(&archive.filename))),
        ArchiveRequest::Search(_) => None,
    }
}

/// Describes playable tracks separately from ZIP containers that require opening.
pub(super) fn items_message(details: &ArchiveOrgItemDetails) -> String {
    if let Some(filename) = &details.archive_filename {
        format!(
            "{filename} · {} audio tracks · Enter: play · d: download · Esc: back",
            details.tracks.len()
        )
    } else if details.archives.is_empty() {
        format!(
            "{} audio tracks · Enter: play · d: download · Esc: back",
            details.tracks.len()
        )
    } else {
        format!(
            "{} audio tracks · {} ZIP folders · Enter: play/open · Esc: back",
            details.tracks.len(),
            details.archives.len()
        )
    }
}

/// Finds a verified ZIP enclosing a canonical member, never an arbitrary filename prefix.
pub(super) fn archive_for_source(
    details: &ArchiveOrgItemDetails,
    source: &url::Url,
) -> Option<ArchiveOrgZip> {
    if archive_download_identifier(source).as_deref() != Some(details.item.identifier.as_str()) {
        return None;
    }
    details
        .archives
        .iter()
        .filter(|archive| {
            let mut prefix = url::Url::parse("https://archive.org/").expect("fixed Archive origin");
            prefix
                .path_segments_mut()
                .expect("hierarchical Archive origin")
                .clear()
                .extend(["download", details.item.identifier.as_str()])
                .extend(archive.filename.split('/'))
                .push("");
            source
                .as_str()
                .strip_prefix(prefix.as_str())
                .is_some_and(|member| !member.is_empty())
        })
        .max_by_key(|archive| archive.filename.len())
        .cloned()
}

impl AppController {
    /// Reveals retained ZIP tracks without assuming their parent remains in the root cache.
    pub(super) fn cached_archive_zip_snapshots(
        &self,
    ) -> impl Iterator<Item = &Arc<ArchiveOrgItemDetails>> {
        self.archive_org
            .zip_cache
            .iter()
            .rev()
            .filter_map(|entry| entry.result.as_ref().ok())
    }

    /// Pins each metadata stage to the same exact download URL without opening navigation.
    #[cfg(any(feature = "yt-dlp", test))]
    pub(super) fn queue_archive_download_lookup(
        &mut self,
        source: &url::Url,
        identifier: String,
        kind: ArchiveRequest,
    ) {
        let archive_filename =
            request_location(&kind).and_then(|(_, filename)| filename.map(str::to_owned));
        if let Some(filename) = &archive_filename {
            self.archive_org
                .zip_cache
                .retain(|entry| entry.identifier != identifier || entry.filename != *filename);
        } else {
            self.archive_org.cache.retain(|(id, _)| id != &identifier);
        }
        let location = (identifier.as_str(), archive_filename.as_deref());
        let existing = self
            .archive_org
            .pending
            .as_mut()
            .filter(|job| request_location(&job.kind) == Some(location));
        let generation = if let Some(job) = existing {
            match &mut job.kind {
                ArchiveRequest::Details { open, .. } | ArchiveRequest::Zip { open, .. } => {
                    *open = false
                }
                ArchiveRequest::Search(_) => {}
            }
            if let Some(request) = &mut self.archive_org.request
                && request.generation == job.generation
            {
                match &mut request.kind {
                    ArchiveRequest::Details { open, .. } | ArchiveRequest::Zip { open, .. } => {
                        *open = false
                    }
                    ArchiveRequest::Search(_) => {}
                }
            }
            job.generation
        } else {
            self.archive_org.generation.wrapping_add(1)
        };
        self.finish_search_activity(SearchActivity::ArchiveOrg);
        self.archive_org.download_lookup = Some(ArchiveDownloadLookup {
            source: source.clone(),
            identifier,
            archive_filename,
            generation,
        });
        self.queue_archive_request(kind, false);
        self.update_archive_back_available();
    }

    /// Reads one item or ZIP result without confusing their independent cache identities.
    pub(super) fn cached_archive_details_for(
        &self,
        identifier: &str,
        archive_filename: Option<&str>,
    ) -> Option<Result<Arc<ArchiveOrgItemDetails>, String>> {
        if let Some(filename) = archive_filename {
            return self
                .archive_org
                .zip_cache
                .iter()
                .rev()
                .find(|entry| entry.identifier == identifier && entry.filename == filename)
                .map(|entry| entry.result.clone());
        }
        self.archive_org
            .cache
            .iter()
            .rev()
            .find(|(id, _)| id == identifier)
            .map(|(_, result)| result.clone())
    }

    /// Caches stale responses without changing active navigation or another metadata scope.
    pub(super) fn cache_archive_response(
        &mut self,
        identifier: &str,
        archive_filename: Option<&str>,
        result: Result<Arc<ArchiveOrgItemDetails>, String>,
    ) {
        if let Some(filename) = archive_filename {
            self.archive_org
                .zip_cache
                .retain(|entry| entry.identifier != identifier || entry.filename != filename);
            self.archive_org.zip_cache.push_back(ArchiveZipCacheEntry {
                identifier: identifier.into(),
                filename: filename.into(),
                result,
            });
            while self.archive_org.zip_cache.len() > 8 {
                self.archive_org.zip_cache.pop_front();
            }
        } else {
            self.archive_org.cache.retain(|(id, _)| id != identifier);
            self.archive_org
                .cache
                .push_back((identifier.into(), result));
            while self.archive_org.cache.len() > 8 {
                self.archive_org.cache.pop_front();
            }
        }
    }

    /// Queues a verified ZIP lookup while retaining its parent for Back and restart flows.
    pub(super) fn queue_archive_zip_request(
        &mut self,
        parent: Arc<ArchiveOrgItemDetails>,
        archive: ArchiveOrgZip,
        open: bool,
    ) {
        self.cache_archive_response(&parent.item.identifier, None, Ok(Arc::clone(&parent)));
        if open {
            self.archive_org.zip_parent = Some(Arc::clone(&parent));
        }
        self.queue_archive_request(
            ArchiveRequest::Zip {
                parent,
                archive,
                open,
            },
            false,
        );
    }

    /// Maps only appended ZIP rows, leaving existing track indices unchanged.
    pub(super) fn selected_archive_zip(&self) -> Option<&ArchiveOrgZip> {
        let details = self.archive_org.active.as_ref()?;
        details
            .archives
            .get(self.view.selected.checked_sub(details.tracks.len())?)
    }

    /// Opens accepted cached members or starts one explicit, retryable listing request.
    pub(super) fn open_archive_zip(&mut self, archive: ArchiveOrgZip) {
        let Some(parent) = self.archive_org.active.clone() else {
            return;
        };
        self.archive_org.zip_parent = Some(Arc::clone(&parent));
        if let Some(Ok(details)) =
            self.cached_archive_details_for(&parent.item.identifier, Some(&archive.filename))
        {
            self.archive_org.generation = self.archive_org.generation.wrapping_add(1);
            self.archive_org.pending = None;
            self.archive_org.request = None;
            self.archive_org.message = items_message(&details);
            self.archive_org.active = Some(details);
            self.archive_org_selected = 0;
            self.populate_archive_org();
            return;
        }
        self.archive_org.zip_cache.retain(|entry| {
            entry.identifier != parent.item.identifier || entry.filename != archive.filename
        });
        self.queue_archive_zip_request(parent, archive, true);
        self.begin_search_activity(SearchActivity::ArchiveOrg);
        self.view.status_line = "Opening archive.org ZIP...".into();
        self.update_archive_back_available();
    }

    /// Moving away from a ZIP row revokes its open intent without stopping pinned lookups.
    pub(super) fn cancel_archive_zip_open_for_selection(&mut self) {
        if self.archive_org.now_playing.is_some()
            || self.archive_org.restoring.is_some()
            || self.archive_download_lookup_pending()
        {
            return;
        }
        let superseded = self.archive_org.pending.as_ref().is_some_and(|job| {
            matches!(&job.kind, ArchiveRequest::Zip { parent, archive, open: true }
                if self.archive_org.active.as_ref().is_none_or(|active| active.item.identifier != parent.item.identifier)
                    || self.selected_archive_zip().is_none_or(|selected| selected.filename != archive.filename))
        });
        if superseded {
            self.archive_org.generation = self.archive_org.generation.wrapping_add(1);
            self.archive_org.pending = None;
            self.archive_org.request = None;
            self.finish_search_activity(SearchActivity::ArchiveOrg);
        }
    }

    /// Esc first cancels a pending ZIP, then returns an open ZIP to its exact parent row.
    pub(super) fn leave_archive_zip(&mut self) -> bool {
        if self.archive_org.zip_return_filename.is_some()
            && self
                .archive_org
                .pending
                .as_ref()
                .is_some_and(|job| matches!(job.kind, ArchiveRequest::Details { open: true, .. }))
        {
            // A second Back cancels parent recovery through ordinary catalogue
            // navigation instead of scheduling the missing parent again.
            self.archive_org.zip_return_filename = None;
            return false;
        }
        if self
            .archive_org
            .pending
            .as_ref()
            .is_some_and(|job| matches!(job.kind, ArchiveRequest::Zip { open: true, .. }))
        {
            self.archive_org.generation = self.archive_org.generation.wrapping_add(1);
            self.archive_org.pending = None;
            self.archive_org.request = None;
            self.archive_org.restoring = None;
            if let Some(details) = &self.archive_org.active {
                self.archive_org.message = items_message(details);
            }
            self.populate_archive_org();
            return true;
        }
        let Some((identifier, filename)) = self.archive_org.active.as_ref().and_then(|details| {
            Some((
                details.item.identifier.clone(),
                details.archive_filename.clone()?,
            ))
        }) else {
            return false;
        };
        let parent = self
            .archive_org
            .zip_parent
            .take()
            .filter(|parent| {
                parent.item.identifier == identifier && parent.archive_filename.is_none()
            })
            .or_else(|| {
                self.cached_archive_details(&identifier)
                    .and_then(Result::ok)
            });
        self.archive_org.restoring = None;
        self.archive_org.generation = self.archive_org.generation.wrapping_add(1);
        self.archive_org.pending = None;
        self.archive_org.request = None;
        if let Some(parent) = parent {
            self.archive_org_selected = parent
                .archives
                .iter()
                .position(|archive| archive.filename == filename)
                .map_or(0, |index| parent.tracks.len() + index);
            self.archive_org.message = items_message(&parent);
            self.archive_org.active = Some(parent);
            self.populate_archive_org();
        } else {
            self.archive_org.zip_return_filename = Some(filename);
            self.queue_archive_request(
                ArchiveRequest::Details {
                    identifier,
                    open: true,
                },
                false,
            );
            self.begin_search_activity(SearchActivity::ArchiveOrg);
            self.view.status_line = "Opening enclosing archive.org item...".into();
        }
        true
    }
}
