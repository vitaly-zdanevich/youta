//! Independent SoundCloud navigation with instance-backed, canonical playback.

use super::*;

#[cfg(feature = "soundcloud")]
use crate::providers::soundcloak::{
    SoundcloakClient, SoundcloakSearchPage, SoundcloakSearchRequest, SoundcloakTrack,
};

/// Queries and selections remain independent even in builds without this provider.
#[derive(Default)]
pub(super) struct SoundCloudState {
    pub(super) query: String,
    pub(super) selected: usize,
    #[cfg(feature = "soundcloud")]
    items: Vec<SoundcloakTrack>,
    #[cfg(feature = "soundcloud")]
    submitted_query: String,
    #[cfg(feature = "soundcloud")]
    next_page: Option<u32>,
    #[cfg(feature = "soundcloud")]
    query_urn: Option<String>,
    #[cfg(feature = "soundcloud")]
    generation: u64,
    #[cfg(feature = "soundcloud")]
    pending: Option<SearchJob>,
    #[cfg(feature = "soundcloud")]
    worker: Option<SearchWorker>,
    #[cfg(all(test, feature = "soundcloud"))]
    client: Option<SoundcloakClient>,
}

impl SoundCloudState {
    /// Restores only credential-free navigation, never expiring stream URLs.
    pub(super) fn restored(saved: &SessionState) -> Self {
        Self {
            query: saved.soundcloud_search_text.clone(),
            selected: saved.soundcloud_selected_row.unwrap_or_default(),
            ..Self::default()
        }
    }
}

/// One current query owns its page and any later continuation token.
#[cfg(feature = "soundcloud")]
#[derive(Clone)]
struct SearchJob {
    generation: u64,
    request: SoundcloakSearchRequest,
}

/// Only one bounded HTTP request runs at a time; the queued replacement is latest-only.
#[cfg(feature = "soundcloud")]
struct SearchWorker {
    job: SearchJob,
    response: Receiver<Result<SoundcloakSearchPage, String>>,
    thread: JoinHandle<()>,
}

impl AppController {
    /// Configures metadata and playback with the same user-selected instance.
    #[cfg(feature = "soundcloud")]
    fn soundcloak_client(&self) -> Result<SoundcloakClient, String> {
        #[cfg(test)]
        if let Some(client) = &self.soundcloud.client {
            return Ok(client.clone());
        }
        match &self.config.providers.soundcloak_base_url {
            Some(url) => SoundcloakClient::new(url.clone()).map_err(|error| error.to_string()),
            None => Ok(SoundcloakClient::default()),
        }
    }

    /// Replaces an existing query without spawning unbounded obsolete workers.
    pub(super) fn submit_soundcloud_search(&mut self, query: String) {
        #[cfg(feature = "soundcloud")]
        {
            self.soundcloud.generation = self.soundcloud.generation.wrapping_add(1);
            self.soundcloud.query.clone_from(&query);
            self.soundcloud.submitted_query.clone_from(&query);
            self.soundcloud.selected = 0;
            self.soundcloud.items.clear();
            self.soundcloud.next_page = None;
            self.soundcloud.query_urn = None;
            self.view.selected = 0;
            self.refresh_soundcloud_rows();
            self.update_soundcloud_detail();
            self.refresh_selected_playlist_state();
            self.queue_soundcloud_page(1);
        }
        #[cfg(not(feature = "soundcloud"))]
        {
            let _ = query;
            self.view.status_line = "This build omits the `soundcloud` feature".to_owned();
        }
    }

    /// Schedules a provider-advertised page while bounding the retained result set.
    #[cfg(feature = "soundcloud")]
    fn queue_soundcloud_page(&mut self, page: u32) {
        self.soundcloud.pending = Some(SearchJob {
            generation: self.soundcloud.generation,
            request: SoundcloakSearchRequest {
                query: self.soundcloud.submitted_query.clone(),
                page,
                limit: 50,
                query_urn: self.soundcloud.query_urn.clone(),
            },
        });
        self.begin_search_activity(SearchActivity::SoundCloud);
        self.view.status_line = "Searching SoundCloud through Soundcloak…".to_owned();
        self.poll_soundcloud_worker();
    }

    /// Polls only finished threads; network activity never blocks UI navigation.
    pub(super) fn poll_soundcloud_worker(&mut self) {
        #[cfg(feature = "soundcloud")]
        {
            if self
                .soundcloud
                .worker
                .as_ref()
                .is_some_and(|worker| worker.thread.is_finished())
            {
                let worker = self
                    .soundcloud
                    .worker
                    .take()
                    .expect("finished worker exists");
                let result = worker.response.try_recv().unwrap_or_else(|_| {
                    Err("The Soundcloak search worker stopped without a result".to_owned())
                });
                let _ = worker.thread.join();
                self.apply_soundcloud_page(worker.job, result);
            }
            if self.soundcloud.worker.is_some() {
                return;
            }
            let Some(job) = self.soundcloud.pending.take() else {
                return;
            };
            let client = match self.soundcloak_client() {
                Ok(client) => client,
                Err(error) => {
                    self.apply_soundcloud_page(job, Err(error));
                    return;
                }
            };
            let request = job.request.clone();
            let (sender, response) = bounded(1);
            match thread::Builder::new()
                .name("youta-soundcloak".to_owned())
                .spawn(move || {
                    let result = client.search(&request).map_err(|error| error.to_string());
                    let _ = sender.send(result);
                }) {
                Ok(thread) => {
                    self.soundcloud.worker = Some(SearchWorker {
                        job,
                        response,
                        thread,
                    })
                }
                Err(error) => self.apply_soundcloud_page(job, Err(error.to_string())),
            }
        }
    }

    /// Applies only current results and never paints them over a different tab.
    #[cfg(feature = "soundcloud")]
    fn apply_soundcloud_page(
        &mut self,
        job: SearchJob,
        result: Result<SoundcloakSearchPage, String>,
    ) {
        if job.generation != self.soundcloud.generation {
            return;
        }
        self.finish_search_activity(SearchActivity::SoundCloud);
        match result {
            Ok(page) => {
                let previous_len = self.soundcloud.items.len();
                for track in page.items {
                    if self.soundcloud.items.len() >= 1_000 {
                        break;
                    }
                    if !self
                        .soundcloud
                        .items
                        .iter()
                        .any(|old| old.webpage_url == track.webpage_url)
                    {
                        self.soundcloud.items.push(track);
                    }
                }
                // Empty/duplicate continuations must not trigger an automatic request loop.
                self.soundcloud.next_page = (self.soundcloud.items.len() > previous_len
                    && self.soundcloud.items.len() < 1_000)
                    .then_some(page.next_page)
                    .flatten();
                self.soundcloud.query_urn = page.query_urn;
                if self.view.screen == Screen::SoundCloud {
                    self.populate_soundcloud();
                    self.refresh_selected_playlist_state();
                }
            }
            Err(error) => {
                if self.view.screen == Screen::SoundCloud {
                    self.view.status_line = format!(
                        "Soundcloak: {error}; instance: {} (providers.soundcloak_base_url)",
                        self.soundcloak_instance_label()
                    );
                }
            }
        }
    }

    /// Returns the active endpoint for visible error messages and source attribution.
    #[cfg(feature = "soundcloud")]
    fn soundcloak_instance_label(&self) -> &str {
        self.config.providers.soundcloak_base_url.as_ref().map_or(
            crate::providers::soundcloak::DEFAULT_SOUNDCLOAK_INSTANCE,
            url::Url::as_str,
        )
    }

    /// Loads another bounded page only when the advertised continuation is selected.
    pub(super) fn activate_soundcloud_selection(&mut self) {
        #[cfg(feature = "soundcloud")]
        {
            if self.view.selected == self.soundcloud.items.len() {
                if let Some(page) = self.soundcloud.next_page
                    && self.soundcloud.pending.is_none()
                    && self.soundcloud.worker.is_none()
                {
                    self.queue_soundcloud_page(page);
                }
                return;
            }
            match self.selected_soundcloud_queue_item() {
                Ok(item) => self.play_queue_item(item, false),
                Err(error) => self.view.status_line = error,
            }
        }
        #[cfg(not(feature = "soundcloud"))]
        self.populate_soundcloud();
    }

    /// Reconstructs the tab from canonical cached metadata without network access.
    pub(super) fn populate_soundcloud(&mut self) {
        #[cfg(feature = "soundcloud")]
        {
            self.refresh_soundcloud_rows();
            self.update_soundcloud_detail();
            self.view.status_line = format!(
                "SoundCloud via {} · {} tracks · / Search · Enter Play",
                self.soundcloak_instance_label(),
                self.soundcloud.items.len()
            );
            if self.soundcloud.worker.is_some() || self.soundcloud.pending.is_some() {
                self.begin_search_activity(SearchActivity::SoundCloud);
            }
        }
        #[cfg(not(feature = "soundcloud"))]
        {
            self.view.rows.clear();
            self.view.details = None;
            self.view.status_line = "This build omits the `soundcloud` feature".to_owned();
        }
    }

    /// Projects bounded track summaries into the shared compact list layout.
    #[cfg(feature = "soundcloud")]
    fn refresh_soundcloud_rows(&mut self) {
        self.view.rows = self
            .soundcloud
            .items
            .iter()
            .map(|track| {
                let id = soundcloud_media_id(track);
                let progress = self.store.progress(&id).ok().flatten();
                let playback = PlaybackRowState::from_progress(progress.as_ref());
                RowView {
                    media_id: Some(id),
                    title: track.title.clone(),
                    subtitle: if track.streamable {
                        track.artist.clone()
                    } else {
                        format!("{} · unavailable for full playback", track.artist)
                    },
                    source: "SoundCloud".to_owned(),
                    thumbnail_url: track.artwork_url.clone(),
                    watched_percent: playback.watched_percent,
                    playback_started: playback.playback_started,
                    compact: true,
                    ..RowView::default()
                }
            })
            .collect();
        if self.soundcloud.next_page.is_some() {
            self.view.rows.push(RowView {
                title: "Load more tracks…".to_owned(),
                compact: true,
                ..RowView::default()
            });
        }
        self.view.selected = self
            .soundcloud
            .selected
            .min(self.view.rows.len().saturating_sub(1));
    }

    /// Keeps track details, selection, and canonical page links in agreement.
    pub(super) fn update_soundcloud_detail(&mut self) {
        self.soundcloud.selected = self.view.selected;
        #[cfg(feature = "soundcloud")]
        {
            self.view.details = self.soundcloud.items.get(self.view.selected).map(|track| {
                let mut links = vec![DetailLinkView {
                    label: "SoundCloud original".to_owned(),
                    url: track.webpage_url.to_string(),
                    ..DetailLinkView::default()
                }];
                if let Ok(page) = self.soundcloak_client().and_then(|client| {
                    client
                        .page_url(&track.webpage_url)
                        .map_err(|error| error.to_string())
                }) {
                    links.push(DetailLinkView {
                        label: "Soundcloak page".to_owned(),
                        url: page.to_string(),
                        ..DetailLinkView::default()
                    });
                }
                DetailView {
                    media_id: Some(soundcloud_media_id(track)),
                    title: track.title.clone(),
                    channel_name: track.artist.clone(),
                    source: "SoundCloud".to_owned(),
                    length: track
                        .duration_seconds
                        .map_or_else(String::new, format_seconds),
                    description: track.description.clone().unwrap_or_default(),
                    webpage_url: Some(track.webpage_url.clone()),
                    thumbnail_url: track.artwork_url.clone(),
                    links,
                    ..DetailView::default()
                }
            });
        }
        #[cfg(not(feature = "soundcloud"))]
        {
            self.view.details = None;
        }
    }

    /// Produces replay-safe queue metadata, rejecting unavailable and continuation rows.
    pub(super) fn selected_soundcloud_queue_item(&self) -> Result<QueueItem, String> {
        #[cfg(feature = "soundcloud")]
        {
            self.soundcloud
                .items
                .get(self.view.selected)
                .filter(|track| track.streamable)
                .map(queue_item_from_soundcloud)
                .ok_or_else(|| "Select a SoundCloud track available for full playback".to_owned())
        }
        #[cfg(not(feature = "soundcloud"))]
        {
            Err("This build omits the `soundcloud` feature".to_owned())
        }
    }

    /// Captures only playable tracks so later searches cannot alter autoplay ownership.
    #[cfg(feature = "soundcloud")]
    pub(super) fn soundcloud_autoplay_origin(&self, media_id: &MediaId) -> Option<AutoplayOrigin> {
        let items = self
            .soundcloud
            .items
            .iter()
            .filter(|track| track.streamable)
            .map(queue_item_from_soundcloud)
            .collect::<Vec<_>>();
        let index = items.iter().position(|item| item.media.id == *media_id)?;
        Some(AutoplayOrigin::SoundCloud {
            items: items.into(),
            index,
        })
    }

    /// Resolves canonical replay locations locally; mpv retrieves proxied HLS off-thread.
    #[cfg(feature = "soundcloud")]
    pub(super) fn soundcloud_playback_input(
        &self,
        item: &QueueItem,
    ) -> Result<PlaybackInput, String> {
        let mut canonical = item.media.webpage_url.clone();
        // Old direct links may use www/http or carry sharing parameters. Normalize
        // public track pages only; the provider still rejects accounts, sets,
        // private tokens, encoded separators, credentials, and foreign origins.
        if matches!(canonical.scheme(), "http" | "https")
            && matches!(
                canonical.host_str(),
                Some("soundcloud.com" | "www.soundcloud.com")
            )
            && canonical.port().is_none()
            && !canonical
                .query_pairs()
                .any(|(key, _)| key == "secret_token")
        {
            canonical
                .set_scheme("https")
                .map_err(|()| "Invalid SoundCloud scheme".to_owned())?;
            canonical
                .set_host(Some("soundcloud.com"))
                .map_err(|error| error.to_string())?;
            canonical.set_query(None);
            canonical.set_fragment(None);
            let path = canonical.path().trim_end_matches('/').to_owned();
            canonical.set_path(&path);
        }
        let stream = self
            .soundcloak_client()?
            .stream_url(&canonical)
            .map_err(|error| error.to_string())?;
        let mut input = PlaybackInput::new(stream.to_string());
        input.bypass_ytdl = true;
        Ok(input)
    }
}

/// Uses the existing direct-URL identity convention for History and playlists.
#[cfg(feature = "soundcloud")]
fn soundcloud_media_id(track: &SoundcloakTrack) -> MediaId {
    MediaId::new(SourceKind::SoundCloud, track.webpage_url.as_str())
}

/// Stores a stable public page; the selected Soundcloak instance is a runtime concern.
#[cfg(feature = "soundcloud")]
fn queue_item_from_soundcloud(track: &SoundcloakTrack) -> QueueItem {
    QueueItem {
        media: MediaItem {
            id: soundcloud_media_id(track),
            kind: MediaKind::Audio,
            title: track.title.clone(),
            creator: Some(track.artist.clone()),
            description: track.description.clone(),
            webpage_url: track.webpage_url.clone(),
            thumbnail_url: track.artwork_url.clone(),
            duration_seconds: track.duration_seconds,
            published_at: None,
            statistics: MediaStatistics::default(),
            license: MediaLicense::Unknown,
            chapters: Vec::new(),
            captions: Vec::new(),
        },
        playback_location: track.webpage_url.to_string(),
        start_at_seconds: None,
        added_at: unix_time(),
    }
}

#[cfg(all(test, feature = "soundcloud"))]
mod tests;
