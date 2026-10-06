//! Cached email actions over unchanged description and comment source bytes.

use super::{AppController, MAX_VIDEO_COMMENTS};
use crate::domain::SourceKind;
use crate::links::{LinkTarget, parse_description_links};
use crate::view::{
    DetailHighlightField, DetailHighlightRange, DetailLinkPresentation, DetailLinkView, DetailView,
    EmailLinkView, VideoCommentsPopupState, ViewModel,
};

/// Matches the parser's email-only source limit without retaining oversized bodies.
const MAX_EMAIL_SOURCE_BYTES: usize = 256 * 1024;

/// One source-text cache; unchanged ticks do not repeat link detection.
#[derive(Default)]
struct EmailTextCache {
    source: Option<String>,
    links: Vec<EmailLinkView>,
    #[cfg(test)]
    scans: usize,
}

impl EmailTextCache {
    /// Refreshes only bounded source changes, retaining exact original UTF-8 offsets.
    fn update(&mut self, source: &str) {
        if source.len() > MAX_EMAIL_SOURCE_BYTES {
            self.source = None;
            self.links.clear();
            return;
        }
        if self.source.as_deref() == Some(source) {
            return;
        }
        self.source = Some(source.to_owned());
        self.links = parse_description_links(source)
            .into_iter()
            .filter_map(|link| {
                if !matches!(&link.target, LinkTarget::Email { .. }) {
                    return None;
                }
                Some(EmailLinkView {
                    start_byte: link.start_byte,
                    end_byte: link.end_byte,
                    url: link.target.canonical_url()?.to_string(),
                })
            })
            .collect();
        #[cfg(test)]
        {
            self.scans += 1;
        }
    }
}

/// At most one Details body and the bounded visible comment list are cached.
#[derive(Default)]
pub(super) struct EmailProjectionCache {
    details: EmailTextCache,
    comments: Vec<EmailTextCache>,
}

impl EmailProjectionCache {
    /// Projects all providers centrally without modifying text or provider-owned links.
    fn apply(&mut self, view: &mut ViewModel) {
        if let Some(details) = view.details.as_mut() {
            self.details.update(&details.description);
            project_detail_emails(
                details,
                &self.details.links,
                &mut view.selected_detail_link,
                &mut view.detail_link_reveal,
            );
        } else {
            self.details = EmailTextCache::default();
        }
        let Some(popup) = view.video_comments_popup.as_mut() else {
            self.comments.clear();
            return;
        };
        let count = popup.comments.len().min(MAX_VIDEO_COMMENTS);
        self.comments.resize_with(count, EmailTextCache::default);
        for (index, comment) in popup.comments.iter_mut().enumerate() {
            if let Some(cache) = self.comments.get_mut(index) {
                cache.update(&comment.text);
                comment.email_links.clone_from(&cache.links);
            } else {
                comment.email_links.clear();
            }
        }
    }
}

/// Keeps existing inline ownership and stable indices, removing only stale generated links.
fn project_detail_emails(
    details: &mut DetailView,
    emails: &[EmailLinkView],
    selected: &mut Option<usize>,
    reveal: &mut Option<usize>,
) {
    let mut additions = emails
        .iter()
        .filter_map(|email| {
            let overlaps = |start, end| email.start_byte < end && email.end_byte > start;
            if details.links.iter().any(|link| {
                !link.generated_email
                    && link
                        .description_range
                        .is_some_and(|range| overlaps(range.start_byte, range.end_byte))
            }) || details
                .timecodes
                .iter()
                .any(|link| overlaps(link.start_byte, link.end_byte))
                || details
                    .video_links
                    .iter()
                    .any(|link| overlaps(link.start_byte, link.end_byte))
            {
                return None;
            }
            Some(DetailLinkView {
                label: details
                    .description
                    .get(email.start_byte..email.end_byte)?
                    .to_owned(),
                url: email.url.clone(),
                presentation: DetailLinkPresentation::LabelOnly,
                description_range: Some(DetailHighlightRange {
                    start_byte: email.start_byte,
                    end_byte: email.end_byte,
                }),
                generated_email: true,
                ..DetailLinkView::default()
            })
        })
        .collect::<Vec<_>>();
    let mut indices = Vec::with_capacity(details.links.len());
    let mut retained = 0;
    details.links.retain(|link| {
        let keep = if link.generated_email {
            additions
                .iter()
                .position(|candidate| candidate == link)
                .map(|index| {
                    additions.remove(index);
                })
                .is_some()
        } else {
            true
        };
        indices.push(keep.then_some(retained));
        retained += usize::from(keep);
        keep
    });
    // Metadata arriving after email projection keeps its original position. Only
    // stale-email removal requires rebasing renderer-owned focus and highlights.
    if retained != indices.len() {
        let rebase =
            |index: Option<usize>| index.and_then(|index| indices.get(index).copied().flatten());
        *selected = rebase(*selected);
        *reveal = rebase(*reveal);
        details.search_highlights.retain_mut(|highlight| {
            let index = match &mut highlight.field {
                DetailHighlightField::LinkLabel(index)
                | DetailHighlightField::LinkPrefix(index)
                | DetailHighlightField::LinkUrl(index) => index,
                _ => return true,
            };
            let Some(updated) = rebase(Some(*index)) else {
                return false;
            };
            *index = updated;
            true
        });
    }
    details.links.extend(additions);
}

impl AppController {
    /// Synchronizes email actions after initialization, UI actions and worker completions.
    pub(super) fn refresh_email_links(&mut self) {
        self.email_projection.apply(&mut self.view);
    }

    /// Resolves only a current popup-owned index; IPC never supplies an unchecked target.
    pub(super) fn activate_comment_email(
        &mut self,
        source: SourceKind,
        video_id: &str,
        comment_index: usize,
        email_index: usize,
    ) {
        let Some(popup) = self.view.video_comments_popup.as_ref().filter(|popup| {
            popup.source == source
                && popup.video_id == video_id
                && popup.state == VideoCommentsPopupState::Ready
        }) else {
            return;
        };
        let Some(url) = popup
            .comments
            .get(comment_index)
            .and_then(|comment| comment.email_links.get(email_index))
            .map(|email| email.url.clone())
        else {
            return;
        };
        self.open_external_url(&url);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{DetailHighlightView, VideoCommentView, VideoCommentsPopupView};

    /// Repaints reuse scans, and returning to a cloned navigation snapshot adds no duplicates.
    #[test]
    fn email_projection_cache_reuses_text_and_navigation_provenance() {
        let mut cache = EmailProjectionCache::default();
        let mut view = ViewModel {
            details: Some(DetailView {
                description: "Contact inbox@example.org".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        cache.apply(&mut view);
        assert_eq!(cache.details.scans, 1);
        assert_eq!(view.details.as_ref().unwrap().links.len(), 1);
        view.selected_detail_link = Some(0);
        view.detail_link_reveal = Some(0);
        let snapshot = view.details.clone();
        cache.apply(&mut view);
        assert_eq!(cache.details.scans, 1);
        assert_eq!(view.selected_detail_link, Some(0));
        assert_eq!(view.detail_link_reveal, Some(0));
        view.details.as_mut().unwrap().description = "A different page".to_owned();
        cache.apply(&mut view);
        assert!(view.details.as_ref().unwrap().links.is_empty());
        assert_eq!(view.selected_detail_link, None);
        view.details = snapshot;
        view.selected_detail_link = Some(0);
        cache.apply(&mut view);
        assert_eq!(view.details.as_ref().unwrap().links.len(), 1);
        assert_eq!(view.selected_detail_link, Some(0));
        let json = serde_json::to_value(&view.details.as_ref().unwrap().links[0]).unwrap();
        assert!(json.get("generated_email").is_none());
    }

    /// Late provider links retain their position and highlights when generated spans expire.
    #[test]
    fn email_projection_rebases_only_removed_generated_indices() {
        let mut cache = EmailProjectionCache::default();
        let mut view = ViewModel {
            details: Some(DetailView {
                description: "inbox@example.org".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        cache.apply(&mut view);
        let owned = DetailLinkView {
            label: "Provider".to_owned(),
            url: "https://example.org/".to_owned(),
            ..DetailLinkView::default()
        };
        let details = view.details.as_mut().unwrap();
        details.links.push(owned.clone());
        details.search_highlights.push(DetailHighlightView {
            field: DetailHighlightField::LinkLabel(1),
            ranges: vec![DetailHighlightRange {
                start_byte: 0,
                end_byte: 1,
            }],
        });
        view.selected_detail_link = Some(1);
        view.detail_link_reveal = Some(1);
        cache.apply(&mut view);
        assert_eq!(view.details.as_ref().unwrap().links[1], owned);
        assert_eq!(view.selected_detail_link, Some(1));
        view.details.as_mut().unwrap().description.clear();
        cache.apply(&mut view);
        assert_eq!(view.details.as_ref().unwrap().links, [owned]);
        assert_eq!(view.selected_detail_link, Some(0));
        assert_eq!(view.detail_link_reveal, Some(0));
        assert_eq!(
            view.details.as_ref().unwrap().search_highlights[0].field,
            DetailHighlightField::LinkLabel(0)
        );
    }

    /// Oversized bodies stay untouched and uncached; comments share the bounded projection.
    #[test]
    fn email_projection_bounds_bodies_comments_and_cached_scans() {
        let mut cache = EmailProjectionCache::default();
        let oversized = format!("{} inbox@example.org", "x".repeat(MAX_EMAIL_SOURCE_BYTES));
        let mut view = ViewModel {
            details: Some(DetailView {
                description: oversized.clone(),
                ..DetailView::default()
            }),
            video_comments_popup: Some(VideoCommentsPopupView {
                source: SourceKind::SoundCloud,
                comments: vec![
                    VideoCommentView {
                        text: "0:15 — Привет inbox@example.org".to_owned(),
                        ..VideoCommentView::default()
                    };
                    MAX_VIDEO_COMMENTS + 1
                ],
                ..VideoCommentsPopupView::default()
            }),
            ..ViewModel::default()
        };
        cache.apply(&mut view);
        cache.apply(&mut view);
        assert_eq!(view.details.as_ref().unwrap().description, oversized);
        assert!(view.details.as_ref().unwrap().links.is_empty());
        assert!(cache.details.source.is_none());
        assert_eq!(cache.details.scans, 0);
        assert_eq!(cache.comments.len(), MAX_VIDEO_COMMENTS);
        assert!(cache.comments.iter().all(|comment| comment.scans == 1));
        let comments = &view.video_comments_popup.as_ref().unwrap().comments;
        assert!(
            comments[..MAX_VIDEO_COMMENTS]
                .iter()
                .all(|comment| comment.email_links.len() == 1)
        );
        assert!(comments[MAX_VIDEO_COMMENTS].email_links.is_empty());
    }
}
