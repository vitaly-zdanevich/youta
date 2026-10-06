//! Copy leaves source identities intact while sharing the modal destination browser.

use super::*;

impl AppController {
    /// Captures exact source paths before opening the cancellable Copy chooser.
    pub(super) fn begin_local_copy(&mut self) {
        if self.view.screen != Screen::Local || self.local_move_is_executing() {
            return;
        }
        if self.local_archive_read_only() {
            self.view.status_line = "Archive folders are read-only".to_owned();
            return;
        }
        let Some(listing) = self.local_listing.as_ref() else {
            self.view.status_line = "No local folder is loaded".to_owned();
            return;
        };
        let mut sources = listing
            .entries
            .iter()
            .filter(|entry| self.local_move_marks.contains(&entry.path))
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        if sources.is_empty()
            && let Some(index) = self.local_entry_index()
            && let Some(entry) = listing.entries.get(index)
        {
            sources.push(entry.path.clone());
        }
        if sources.is_empty() {
            self.view.status_line = "Select a local file or folder before choosing Copy".to_owned();
            return;
        }
        let destination = listing
            .parent
            .clone()
            .unwrap_or_else(|| listing.path.clone());
        let source_names = sources
            .iter()
            .map(|path| {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        self.local_move_selection = Some(LocalMoveSelection {
            source_directory: listing.path.clone(),
            sources,
            destination_directory: destination.clone(),
            destination_rows: Vec::new(),
        });
        self.view.local_file_popup = Some(LocalFilePopupView::Copy {
            source_names,
            destination: destination.display().to_string(),
            directories: Vec::new(),
            selected: 0,
            pending: true,
            error: None,
        });
        self.request_local_move_destinations(destination);
    }

    /// Locks the dialog immediately; validation and copying run on the filesystem worker.
    pub(super) fn confirm_local_copy_here(&mut self) {
        let Some(LocalFilePopupView::Copy { pending: false, .. }) =
            self.view.local_file_popup.as_ref()
        else {
            return;
        };
        let Some(selection) = self.local_move_selection.clone() else {
            return;
        };
        if let Some(LocalFilePopupView::Copy { pending, error, .. }) =
            self.view.local_file_popup.as_mut()
        {
            *pending = true;
            *error = None;
        }
        self.local_move_execution_pending = true;
        self.view.local_file_progress = Some(crate::view::LocalFileProgressView {
            completed_bytes: 0,
            total_bytes: None,
            completed_entries: 0,
            total_entries: selection.sources.len(),
        });
        self.local_move_generation = self.local_move_generation.wrapping_add(1);
        if !self.send_local_browse_request(
            LocalBrowseRequest::Copy {
                generation: self.local_move_generation,
                selection,
            },
            "Could not copy Local entries",
        ) {
            self.local_move_execution_pending = false;
            self.view.local_file_progress = None;
            if let Some(LocalFilePopupView::Copy { pending, error, .. }) =
                self.view.local_file_popup.as_mut()
            {
                *pending = false;
                *error = Some("The Local filesystem worker is unavailable".to_owned());
            }
        }
    }

    /// Reports published copies without moving playback, notes, history, or queue identities.
    pub(super) fn handle_local_copy_response(
        &mut self,
        generation: u64,
        result: Result<crate::local_move::LocalCopyReport, crate::local_move::LocalCopyError>,
    ) {
        if generation != self.local_move_generation
            || !self.local_move_execution_pending
            || !matches!(
                self.view.local_file_popup,
                Some(LocalFilePopupView::Copy { .. })
            )
        {
            return;
        }
        self.local_move_execution_pending = false;
        self.view.local_file_progress = None;
        let (completed, failure, recovery) = match result {
            Ok(report) => (report.completed, None, report.recovery),
            Err(error) => (
                error.completed().to_vec(),
                Some(error.to_string()),
                Vec::new(),
            ),
        };
        let count = completed.len();
        for mapping in &completed {
            self.local_move_marks.remove(&mapping.source);
        }
        if let Some(failure) = failure {
            if let Some(selection) = self.local_move_selection.as_mut() {
                selection
                    .sources
                    .retain(|source| !completed.iter().any(|mapping| &mapping.source == source));
            }
            if let Some(LocalFilePopupView::Copy {
                source_names,
                pending,
                error,
                ..
            }) = self.view.local_file_popup.as_mut()
            {
                *pending = false;
                *error = Some(failure);
                if let Some(selection) = self.local_move_selection.as_ref() {
                    *source_names = selection
                        .sources
                        .iter()
                        .map(|path| {
                            path.file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned()
                        })
                        .collect();
                }
            }
            self.view.status_line =
                format!("Copied {count} local entries before an error; originals unchanged");
        } else {
            self.local_move_marks.clear();
            self.local_move_selection = None;
            self.view.local_file_popup = None;
            self.view.status_line = format!(
                "Copied {count} local entr{}; originals unchanged",
                if count == 1 { "y" } else { "ies" }
            );
        }
        if count > 0 {
            self.invalidate_local_folder_sizes();
            self.rebuild_local_browser_rows();
        }
        let retained = recovery
            .iter()
            .filter_map(|entry| {
                if let crate::local_move::LocalMoveRecovery::SourceAndStagingRetained {
                    staging,
                    ..
                } = entry
                {
                    Some(staging.display().to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        if !retained.is_empty() {
            self.show_error_message("Local copies completed with cleanup warnings", format!(
                "Originals and published copies are intact. Staging paths require inspection:\n{}", retained.join("\n")
            ));
        }
    }
}
