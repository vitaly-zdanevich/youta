//! Snapshotted, typed manual download choices, independent of subscription automation.

use super::*;
#[cfg(feature = "archive-org")]
use crate::config::ArchiveDownloadPreference;
use crate::config::DownloadMode;
#[cfg(feature = "archive-org")]
use crate::providers::archive_org::{ArchiveOrgDownloadVariant, ArchiveOrgFileProvenance};
use crate::view::DownloadChoicePopupView;

/// The item captured at invocation, never reconstructed from mutable rendered labels.
pub(super) struct PendingDownloadChoice {
    generation: u64,
    item: QueueItem,
    stage: DownloadChoiceStage,
}

enum DownloadChoiceStage {
    #[cfg(feature = "archive-org")]
    LoadingArchive,
    Options(Vec<DownloadOption>),
}

enum DownloadOption {
    Ready {
        label: String,
        source_url: url::Url,
        format: DownloadFormat,
    },
    #[cfg(feature = "archive-org")]
    Archive {
        label: String,
        variant: ArchiveOrgDownloadVariant,
    },
}

impl DownloadOption {
    fn label(&self) -> &str {
        match self {
            Self::Ready { label, .. } => label,
            #[cfg(feature = "archive-org")]
            Self::Archive { label, .. } => label,
        }
    }
}

impl AppController {
    /// Applies explicit manual preferences, leaving legacy unattended formats alone.
    pub(super) fn choose_manual_download(&mut self, item: QueueItem) {
        if self.pending_download_choice.is_some() {
            return;
        }
        match item.media.id.source {
            SourceKind::YouTube => {
                let source_url = item.media.webpage_url.clone();
                self.choose_video_download(item, source_url, false);
            }
            #[cfg(feature = "archive-org")]
            SourceKind::ArchiveOrg => {
                self.download_choice_generation = self.download_choice_generation.wrapping_add(1);
                let generation = self.download_choice_generation;
                self.view.download_choice_popup = Some(DownloadChoicePopupView {
                    generation,
                    title: "Download from archive.org".to_owned(),
                    explanation: format!(
                        "{}\nLoading available original files and Archive-generated encodings…",
                        item.media.title
                    ),
                    options: Vec::new(),
                    selected: 0,
                });
                self.pending_download_choice = Some(PendingDownloadChoice {
                    generation,
                    item,
                    stage: DownloadChoiceStage::LoadingArchive,
                });
                self.poll_manual_download_choice();
            }
            #[cfg(not(feature = "archive-org"))]
            SourceKind::ArchiveOrg => {
                self.view.status_line = "This build omits the `archive-org` feature".to_owned();
                self.finish_manual_download_choice(false);
            }
            _ => match configured_download_format(&self.config.subscriptions.audio_format) {
                Ok(format) => {
                    let source_url = self.captured_manual_download_url(&item);
                    self.launch_manual_download(item, source_url, format);
                }
                Err(error) => {
                    self.show_error_message("Download format is invalid", error);
                    self.finish_manual_download_choice(false);
                }
            },
        }
    }

    /// Resolves only the captured Archive source through its bounded metadata worker.
    pub(super) fn poll_manual_download_choice(&mut self) {
        #[cfg(feature = "archive-org")]
        {
            let Some(pending) = &self.pending_download_choice else {
                return;
            };
            if !matches!(pending.stage, DownloadChoiceStage::LoadingArchive) {
                return;
            }
            let source = pending.item.media.webpage_url.clone();
            let result = self.archive_download_variants(&source);
            if matches!(result, Ok(None)) {
                return;
            }
            let pending = self
                .pending_download_choice
                .take()
                .expect("owned loading choice");
            self.view.download_choice_popup = None;
            self.cancel_archive_download_lookup();
            match result {
                Ok(Some(variants)) => self.choose_archive_download(pending.item, variants),
                Err(error) => {
                    self.show_error_message("Could not choose a download", error);
                    self.finish_manual_download_choice(false);
                }
                Ok(None) => unreachable!("pending metadata was retained above"),
            }
        }
    }

    /// Keeps a source video unchanged, or extracts audio by stream copy only.
    fn choose_video_download(&mut self, item: QueueItem, source_url: url::Url, exact: bool) {
        let video_format = if exact {
            DownloadFormat::ExactFile
        } else {
            DownloadFormat::BestVideo
        };
        match self.config.downloads.mode {
            DownloadMode::Video => self.launch_manual_download(item, source_url, video_format),
            DownloadMode::AudioOnly => self.launch_manual_download(item, source_url, DownloadFormat::AudioOnlyWithoutReencoding),
            DownloadMode::AskEachTime => self.show_download_choices(
                item,
                "Download video or audio?",
                if exact {
                    "Keep the selected video file unchanged, or extract its audio without re-encoding. Unsupported extraction fails instead of converting."
                } else {
                    "YouTube provides encoded delivery streams, not the uploader's original file. Keep video and audio, or audio only; neither choice re-encodes."
                },
                vec![
                    DownloadOption::Ready {
                        label: if exact { "Selected video (unchanged)" } else { "Video and audio" }.to_owned(),
                        source_url: source_url.clone(), format: video_format,
                    },
                    DownloadOption::Ready {
                        label: "Audio only (no re-encoding)".to_owned(),
                        source_url, format: DownloadFormat::AudioOnlyWithoutReencoding,
                    },
                ],
            ),
        }
    }

    #[cfg(feature = "archive-org")]
    /// Chooses only validated variants from the captured track's derivative family.
    pub(super) fn choose_archive_download(
        &mut self,
        item: QueueItem,
        variants: Vec<ArchiveOrgDownloadVariant>,
    ) {
        if variants.is_empty() {
            self.show_error_message(
                "Could not choose a download",
                "No downloadable files remain for this Archive track",
            );
            self.finish_manual_download_choice(false);
            return;
        }
        let preference = self.config.downloads.archive_format;
        let matches: Vec<usize> = variants
            .iter()
            .enumerate()
            .filter_map(|(index, variant)| {
                let matched = match preference {
                    ArchiveDownloadPreference::AskEachTime => variants.len() == 1,
                    ArchiveDownloadPreference::OriginalFile => {
                        variant.provenance == ArchiveOrgFileProvenance::Original
                    }
                    ArchiveDownloadPreference::ArchiveMp3 => {
                        variant.provenance == ArchiveOrgFileProvenance::Derivative
                            && !variant.is_video
                            && variant.filename.to_ascii_lowercase().ends_with(".mp3")
                    }
                };
                matched.then_some(index)
            })
            .collect();
        if let [index] = matches.as_slice() {
            let variant = variants
                .into_iter()
                .nth(*index)
                .expect("matching variant index");
            self.download_archive_variant(item, variant);
            return;
        }
        let explanation = if preference == ArchiveDownloadPreference::AskEachTime {
            "Choose a file as stored by archive.org. Audio files are downloaded unchanged, without conversion."
        } else {
            "The preferred format is unavailable or matches multiple files. Choose an available file; no substitute or conversion is selected automatically."
        };
        let options = variants
            .into_iter()
            .map(|variant| {
                let provenance = match variant.provenance {
                    ArchiveOrgFileProvenance::Original => "Original",
                    ArchiveOrgFileProvenance::Derivative => "Archive-generated",
                    ArchiveOrgFileProvenance::Unknown => "Provenance unknown",
                };
                let size = variant
                    .size_bytes
                    .map_or_else(String::new, |bytes| format!(" · {}", human_bytes(bytes)));
                let label = format!(
                    "{provenance}: {}{size} · {}",
                    variant.format, variant.filename
                );
                DownloadOption::Archive { label, variant }
            })
            .collect();
        self.show_download_choices(item, "Download from archive.org", explanation, options);
    }

    #[cfg(feature = "archive-org")]
    fn download_archive_variant(&mut self, item: QueueItem, variant: ArchiveOrgDownloadVariant) {
        if variant.is_video {
            self.choose_video_download(item, variant.download_url, true);
        } else {
            self.launch_manual_download(item, variant.download_url, DownloadFormat::ExactFile);
        }
    }

    fn show_download_choices(
        &mut self,
        item: QueueItem,
        title: &str,
        explanation: &str,
        options: Vec<DownloadOption>,
    ) {
        self.download_choice_generation = self.download_choice_generation.wrapping_add(1);
        let generation = self.download_choice_generation;
        self.view.download_choice_popup = Some(DownloadChoicePopupView {
            generation,
            title: title.to_owned(),
            explanation: format!("{}\n{explanation}", item.media.title),
            options: options
                .iter()
                .map(|option| option.label().to_owned())
                .collect(),
            selected: 0,
        });
        self.pending_download_choice = Some(PendingDownloadChoice {
            generation,
            item,
            stage: DownloadChoiceStage::Options(options),
        });
    }

    /// Moves only within rendered options; a loading popup has no selectable rows.
    pub(super) fn move_download_choice(&mut self, direction: i32) {
        if let Some(popup) = self.view.download_choice_popup.as_mut() {
            popup.selected = popup
                .selected
                .saturating_add_signed(direction as isize)
                .min(popup.options.len().saturating_sub(1));
        }
    }

    /// Consumes an exact authoritative option once, independent of the current row.
    pub(super) fn confirm_download_choice(&mut self, generation: u64, clicked: Option<usize>) {
        let Some(popup) = &self.view.download_choice_popup else {
            return;
        };
        let index = clicked.unwrap_or(popup.selected);
        let Some(PendingDownloadChoice {
            generation: owner,
            stage: DownloadChoiceStage::Options(options),
            ..
        }) = &self.pending_download_choice
        else {
            return;
        };
        if generation != *owner || popup.generation != *owner {
            return;
        }
        if index >= options.len() {
            return;
        }
        let pending = self
            .pending_download_choice
            .take()
            .expect("validated choice");
        self.view.download_choice_popup = None;
        let options = match pending.stage {
            DownloadChoiceStage::Options(options) => options,
            #[cfg(feature = "archive-org")]
            DownloadChoiceStage::LoadingArchive => return,
        };
        match options
            .into_iter()
            .nth(index)
            .expect("validated option index")
        {
            DownloadOption::Ready {
                source_url, format, ..
            } => self.launch_manual_download(pending.item, source_url, format),
            #[cfg(feature = "archive-org")]
            DownloadOption::Archive { variant, .. } => {
                self.download_archive_variant(pending.item, variant)
            }
        }
    }

    /// Dismisses choices and revokes metadata ownership without starting any child.
    pub(super) fn dismiss_download_choice(&mut self) {
        self.pending_download_choice = None;
        self.view.download_choice_popup = None;
        #[cfg(feature = "archive-org")]
        self.cancel_archive_download_lookup();
        self.finish_manual_download_choice(true);
    }
}
