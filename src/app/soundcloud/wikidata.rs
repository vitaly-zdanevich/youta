//! Selection-owned, debounced SoundCloud P3040 enrichment.

use super::*;
use crate::providers::wikidata::{WikidataExternalKind, soundcloud_external_id};

/// Retains ownership after completion so Details refreshes cannot retry a lookup.
#[derive(Clone)]
pub(super) struct ScheduledSoundCloudWikidata {
    generation: u64,
    external_id: String,
    due_at: Option<Instant>,
}

impl AppController {
    /// Reads P3040 identity from the original page, never from a proxy or stream URL.
    fn selected_soundcloud_wikidata_id(&self) -> Option<String> {
        if self.view.screen != Screen::SoundCloud {
            return None;
        }
        let details = self.view.details.as_ref()?;
        if details.media_id.as_ref()?.source != SourceKind::SoundCloud {
            return None;
        }
        soundcloud_external_id(details.webpage_url.as_ref()?)
    }

    /// Restores cached matches immediately; otherwise waits for a settled selection.
    ///
    /// The existing P3040 provider checks the exact track path and its exact account
    /// ID in one bounded query. Empty cached results are useful too. Retaining an
    /// already requested owner prevents optional failures from becoming retry loops.
    pub(in crate::app) fn schedule_selected_soundcloud_wikidata(&mut self, now: Instant) {
        let Some(external_id) = self.selected_soundcloud_wikidata_id() else {
            if let Some(previous) = self.soundcloud.scheduled_wikidata.take()
                && previous.generation == self.wikidata_generation
            {
                self.invalidate_wikidata_lookup();
            }
            return;
        };
        if self
            .soundcloud
            .scheduled_wikidata
            .as_ref()
            .is_some_and(|previous| {
                previous.generation == self.wikidata_generation
                    && previous.external_id == external_id
            })
        {
            return;
        }
        self.invalidate_wikidata_lookup();
        let cached =
            self.apply_fresh_cached_wikidata(WikidataExternalKind::SoundCloud, &external_id);
        self.soundcloud.scheduled_wikidata = Some(ScheduledSoundCloudWikidata {
            generation: self.wikidata_generation,
            external_id,
            due_at: (!cached).then_some(now + CHANNEL_DETAILS_DEBOUNCE),
        });
    }

    /// Sends at most one request for the still-visible owner after its quiet period.
    pub(in crate::app) fn request_due_soundcloud_wikidata(&mut self, now: Instant) {
        let Some(mut scheduled) = self.soundcloud.scheduled_wikidata.take() else {
            return;
        };
        if scheduled.generation != self.wikidata_generation
            || self.selected_soundcloud_wikidata_id().as_deref() != Some(&scheduled.external_id)
        {
            return;
        }
        if self.soundcloud.pending_wikidata.is_none()
            && scheduled.due_at.is_some_and(|due_at| now >= due_at)
        {
            scheduled.due_at = None;
            if self.send_soundcloud_wikidata_request(scheduled.generation, &scheduled.external_id) {
                self.soundcloud.pending_wikidata = Some(scheduled.generation);
            }
        }
        self.soundcloud.scheduled_wikidata = Some(scheduled);
    }

    /// Sends optional enrichment without blocking or invoking foreground error dialogs.
    ///
    /// The caller has already consumed this owner's deadline, so an unavailable
    /// worker cannot cause automatic retries when Details is repainted.
    fn send_soundcloud_wikidata_request(&mut self, generation: u64, external_id: &str) -> bool {
        let sent = self.provider_requests.as_ref().is_some_and(|sender| {
            sender
                .try_send(ProviderRequest::Wikidata {
                    generation,
                    kind: WikidataExternalKind::SoundCloud,
                    external_id: external_id.to_owned(),
                })
                .is_ok()
        });
        if let Some(details) = self.view.details.as_mut() {
            details.wikidata = if sent {
                "loading P3040 lazily…"
            } else {
                "provider worker unavailable"
            }
            .to_owned();
        }
        sent
    }

    /// Releases the in-flight slot even when navigation made its response stale.
    ///
    /// The shared provider worker must not accumulate a queue of obsolete track
    /// lookups. While one request runs, only the newest selected identity waits.
    pub(in crate::app) fn finish_soundcloud_wikidata(
        &mut self,
        generation: u64,
        property_id: &str,
    ) {
        if property_id == WikidataExternalKind::SoundCloud.property_id()
            && self.soundcloud.pending_wikidata == Some(generation)
        {
            self.soundcloud.pending_wikidata = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captures requests locally; constructing Details never contacts a provider.
    fn fixture() -> (AppController, Receiver<ProviderRequest>) {
        let config = Config::for_dir("/tmp/youta-soundcloud-wikidata-test");
        let mut controller =
            AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
        controller.view.screen = Screen::SoundCloud;
        let (requests, captured) = unbounded();
        controller.provider_requests = Some(requests);
        select(&mut controller, "artist/first");
        (controller, captured)
    }

    /// Models the canonical identity projected by the SoundCloud Details builder.
    fn select(controller: &mut AppController, external_id: &str) {
        let url = url::Url::parse(&format!("https://soundcloud.com/{external_id}")).unwrap();
        controller.view.details = Some(DetailView {
            media_id: Some(MediaId::new(SourceKind::SoundCloud, url.as_str())),
            webpage_url: Some(url),
            ..DetailView::default()
        });
    }

    /// One related entity is sufficient to exercise the existing disclosure cache.
    fn entity() -> crate::domain::WikidataLink {
        crate::domain::WikidataLink {
            item_id: "Q42".to_owned(),
            label: "Fixture entity".to_owned(),
            description: Some("Fixture description".to_owned()),
            url: url::Url::parse("https://www.wikidata.org/wiki/Q42").unwrap(),
        }
    }

    #[test]
    fn soundcloud_wikidata_debounces_and_replaces_obsolete_selection() {
        let (mut controller, requests) = fixture();
        let now = Instant::now();
        controller.schedule_selected_soundcloud_wikidata(now);
        let old_generation = controller.wikidata_generation;
        controller.request_due_soundcloud_wikidata(now);
        assert!(requests.try_recv().is_err());
        select(&mut controller, "artist/second");
        controller.schedule_selected_soundcloud_wikidata(now);
        assert_ne!(controller.wikidata_generation, old_generation);
        controller.request_due_soundcloud_wikidata(now + CHANNEL_DETAILS_DEBOUNCE);
        assert!(
            matches!(requests.try_recv().unwrap(), ProviderRequest::Wikidata {
			kind: WikidataExternalKind::SoundCloud, external_id, ..
		} if external_id == "artist/second")
        );
        controller.request_due_soundcloud_wikidata(now + Duration::from_secs(10));
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn soundcloud_wikidata_refresh_does_not_requeue_or_extend_debounce() {
        let (mut controller, requests) = fixture();
        let now = Instant::now();
        controller.schedule_selected_soundcloud_wikidata(now);
        let generation = controller.wikidata_generation;
        controller.schedule_selected_soundcloud_wikidata(now + Duration::from_millis(20));
        assert_eq!(controller.wikidata_generation, generation);
        controller.request_due_soundcloud_wikidata(now + CHANNEL_DETAILS_DEBOUNCE);
        assert!(requests.try_recv().is_ok());
        controller.handle_provider_response(ProviderResponse::Wikidata {
            generation,
            property_id: "P3040".to_owned(),
            external_id: "artist/first".to_owned(),
            result: Err("timeout".to_owned()),
        });
        controller.schedule_selected_soundcloud_wikidata(now + Duration::from_secs(1));
        controller.request_due_soundcloud_wikidata(now + Duration::from_secs(10));
        assert!(
            requests.try_recv().is_err(),
            "an optional failure must not create a retry loop"
        );
        assert!(controller.view.error_popup.is_none());
    }

    #[test]
    fn soundcloud_wikidata_reuses_positive_and_empty_cache() {
        for items in [vec![entity()], Vec::new()] {
            let (mut controller, requests) = fixture();
            controller
                .store
                .put_cached_wikidata(&CachedWikidataLookup {
                    property_id: "P3040".to_owned(),
                    external_id: "artist/first".to_owned(),
                    items: items.clone(),
                    fetched_at: unix_time(),
                    expires_at: i64::MAX,
                })
                .unwrap();
            let now = Instant::now();
            controller.schedule_selected_soundcloud_wikidata(now);
            assert_eq!(
                controller.view.details.as_ref().unwrap().links.len(),
                items.len()
            );
            controller.schedule_selected_soundcloud_wikidata(now);
            controller.request_due_soundcloud_wikidata(now + Duration::from_secs(10));
            assert!(requests.try_recv().is_err());
        }
    }

    #[test]
    fn soundcloud_wikidata_rejects_old_response_after_continuation_selection() {
        let (mut controller, requests) = fixture();
        let now = Instant::now();
        controller.schedule_selected_soundcloud_wikidata(now);
        let generation = controller.wikidata_generation;
        controller.request_due_soundcloud_wikidata(now + CHANNEL_DETAILS_DEBOUNCE);
        assert!(requests.try_recv().is_ok());
        controller.view.details = None;
        controller.schedule_selected_soundcloud_wikidata(now);
        controller.handle_provider_response(ProviderResponse::Wikidata {
            generation,
            property_id: "P3040".to_owned(),
            external_id: "artist/first".to_owned(),
            result: Ok(vec![entity()]),
        });
        assert!(controller.view.details.is_none());
        assert!(controller.soundcloud.scheduled_wikidata.is_none());
        assert!(
            controller
                .store
                .cached_wikidata("P3040", "artist/first")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn soundcloud_wikidata_never_looks_up_proxy_or_inactive_details() {
        for (screen, source, page) in [
            (
                Screen::Search,
                SourceKind::SoundCloud,
                "https://soundcloud.com/artist/first",
            ),
            (
                Screen::SoundCloud,
                SourceKind::YouTube,
                "https://soundcloud.com/artist/first",
            ),
            (
                Screen::SoundCloud,
                SourceKind::SoundCloud,
                "https://soundcloak.example/artist/first",
            ),
        ] {
            let (mut controller, requests) = fixture();
            controller.view.screen = screen;
            controller.view.details = Some(DetailView {
                media_id: Some(MediaId::new(source, page)),
                webpage_url: Some(url::Url::parse(page).unwrap()),
                ..DetailView::default()
            });
            let now = Instant::now();
            controller.schedule_selected_soundcloud_wikidata(now);
            controller.request_due_soundcloud_wikidata(now + Duration::from_secs(10));
            assert!(requests.try_recv().is_err());
            assert!(controller.soundcloud.scheduled_wikidata.is_none());
        }
    }

    #[test]
    fn soundcloud_wikidata_drops_pending_lookup_after_tab_switch() {
        let (mut controller, requests) = fixture();
        let now = Instant::now();
        controller.schedule_selected_soundcloud_wikidata(now);
        controller.view.screen = Screen::Search;
        controller.request_due_soundcloud_wikidata(now + CHANNEL_DETAILS_DEBOUNCE);
        assert!(requests.try_recv().is_err());
        assert!(controller.soundcloud.scheduled_wikidata.is_none());
    }

    #[test]
    fn soundcloud_wikidata_serializes_requests_and_keeps_only_latest_selection() {
        let (mut controller, requests) = fixture();
        let now = Instant::now();
        controller.schedule_selected_soundcloud_wikidata(now);
        let first_generation = controller.wikidata_generation;
        controller.request_due_soundcloud_wikidata(now + CHANNEL_DETAILS_DEBOUNCE);
        assert!(requests.try_recv().is_ok());
        for path in ["artist/second", "artist/third"] {
            select(&mut controller, path);
            controller.schedule_selected_soundcloud_wikidata(now);
            controller.request_due_soundcloud_wikidata(now + CHANNEL_DETAILS_DEBOUNCE);
            assert!(
                requests.try_recv().is_err(),
                "one in-flight P3040 lookup at a time"
            );
        }
        controller.handle_provider_response(ProviderResponse::Wikidata {
            generation: first_generation,
            property_id: "P3040".to_owned(),
            external_id: "artist/first".to_owned(),
            result: Ok(vec![entity()]),
        });
        assert!(controller.view.details.as_ref().unwrap().links.is_empty());
        controller.request_due_soundcloud_wikidata(now + CHANNEL_DETAILS_DEBOUNCE);
        assert!(
            matches!(requests.try_recv().unwrap(), ProviderRequest::Wikidata {
            external_id, ..
        } if external_id == "artist/third")
        );
    }

    #[test]
    fn soundcloud_wikidata_worker_failure_remains_optional_and_quiet() {
        for missing in [false, true] {
            let (mut controller, requests) = fixture();
            drop(requests);
            if missing {
                controller.provider_requests = None;
            }
            let now = Instant::now();
            controller.schedule_selected_soundcloud_wikidata(now);
            controller.request_due_soundcloud_wikidata(now + CHANNEL_DETAILS_DEBOUNCE);
            assert!(
                controller.view.error_popup.is_none(),
                "optional enrichment must not interrupt playback"
            );
            assert!(controller.soundcloud.pending_wikidata.is_none());
            assert_eq!(
                controller.view.details.as_ref().unwrap().wikidata,
                "provider worker unavailable"
            );
            // Even if a worker later becomes available, repainting the same
            // selected item must not turn an optional failure into retries.
            let (sender, recovered) = unbounded();
            controller.provider_requests = Some(sender);
            controller.schedule_selected_soundcloud_wikidata(now + Duration::from_secs(1));
            controller.request_due_soundcloud_wikidata(now + Duration::from_secs(10));
            assert!(recovered.try_recv().is_err());
        }
    }
}
