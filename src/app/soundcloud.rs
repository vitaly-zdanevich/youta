//! Independent SoundCloud navigation with instance-backed, canonical playback.

use super::*;

#[cfg(all(feature = "soundcloud", feature = "wikidata"))]
mod wikidata;

#[cfg(feature = "soundcloud")]
mod playback;

#[cfg(feature = "soundcloud")]
mod comments;

#[cfg(feature = "soundcloud")]
use crate::providers::soundcloak::{
    SoundcloakClient, SoundcloakPlayback, SoundcloakSearchPage, SoundcloakSearchRequest,
    SoundcloakTrack,
};

/// Queries and selections remain independent even in builds without this provider.
#[derive(Default)]
pub(super) struct SoundCloudState {
    #[cfg(feature = "soundcloud")]
    playback: playback::SoundCloudPlaybackState,
    #[cfg(feature = "soundcloud")]
    comments: comments::SoundCloudCommentsState,
    pub(super) query: String,
    pub(super) selected: usize,
    #[cfg(all(feature = "soundcloud", feature = "wikidata"))]
    scheduled_wikidata: Option<wikidata::ScheduledSoundCloudWikidata>,
    #[cfg(all(feature = "soundcloud", feature = "wikidata"))]
    pending_wikidata: Option<u64>,
    #[cfg(feature = "soundcloud")]
    items: Vec<SoundcloakTrack>,
    #[cfg(feature = "soundcloud")]
    submitted_query: String,
    /// Latest frontend result capacity, used only for a new query.
    #[cfg(feature = "soundcloud")]
    search_page_capacity: Option<usize>,
    /// Fixed offset stride for the current result set, unaffected by resizing.
    #[cfg(feature = "soundcloud")]
    page_limit: Option<usize>,
    /// Explicit continuation owner; changed selections and tabs cancel the turn.
    #[cfg(feature = "soundcloud")]
    pub(super) page_turn: Option<(u64, usize)>,
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
    /// Reports future search capacity without fetching or changing existing offsets.
    #[cfg(feature = "soundcloud")]
    pub(super) fn update_soundcloud_search_page_capacity(&mut self, rows: usize) {
        self.soundcloud.search_page_capacity = Some(rows.clamp(1, 100));
    }

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
            self.soundcloud.page_limit = Some(self.soundcloud.search_page_capacity.unwrap_or(50));
            self.soundcloud.page_turn = None;
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
                limit: self.soundcloud.page_limit.unwrap_or(50),
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
                if let Some((generation, boundary)) = self.soundcloud.page_turn.take()
                    && generation == job.generation
                    && self.view.screen == Screen::SoundCloud
                    && self.soundcloud.selected == boundary
                    && self.soundcloud.items.len() > previous_len
                {
                    self.soundcloud.selected = if self.soundcloud.next_page.is_some() {
                        self.soundcloud.items.len()
                    } else {
                        self.soundcloud.items.len().saturating_sub(1)
                    };
                }
                if self.view.screen == Screen::SoundCloud {
                    self.populate_soundcloud();
                    self.refresh_selected_playlist_state();
                }
            }
            Err(error) => {
                self.soundcloud.page_turn = None;
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
                    self.soundcloud.page_turn =
                        Some((self.soundcloud.generation, self.view.selected));
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
                    subtitle: match track.playback {
                        SoundcloakPlayback::Full if track.streamable => track.artist.clone(),
                        SoundcloakPlayback::Preview if track.streamable => format!(
                            "{} · {} public preview",
                            track.artist,
                            track
                                .duration_seconds
                                .map_or_else(String::new, format_seconds),
                        ),
                        _ => format!("{} · playback unavailable", track.artist),
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
            #[cfg(feature = "wikidata")]
            let previous = self.view.details.take();
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
                if let Some(genre) = track.genre.as_ref()
                    && let Ok(url) = self.soundcloak_client().and_then(|client| {
                        client.genre_url(genre).map_err(|error| error.to_string())
                    })
                {
                    links.push(DetailLinkView {
                        prefix: "Genre: ".to_owned(),
                        label: genre.clone(),
                        url: url.to_string(),
                        ..DetailLinkView::default()
                    });
                }
                let preview = track.playback == SoundcloakPlayback::Preview;
                DetailView {
                    media_id: Some(soundcloud_media_id(track)),
                    title: track.title.clone(),
                    channel_name: track.artist.clone(),
                    source: "SoundCloud".to_owned(),
                    length: soundcloud_duration_label(track),
                    likes: track.likes_count.map_or_else(String::new, format_count),
                    comments: track.comment_count.map_or_else(String::new, format_count),
                    license: track.license.clone().unwrap_or_default(),
                    soundcloud: Some(crate::view::SoundCloudDetailsView {
                        plays: track.playback_count,
                        reposts: track.reposts_count,
                        created: soundcloud_date(track.created_at.as_deref()),
                        modified: soundcloud_date(track.last_modified.as_deref()),
                        tags: track.tags.clone(),
                        preview_duration_seconds: preview
                            .then_some(track.duration_seconds)
                            .flatten(),
                    }),
                    description: track.description.clone().unwrap_or_default(),
                    webpage_url: Some(track.webpage_url.clone()),
                    thumbnail_url: track.artwork_url.clone(),
                    expanded_thumbnail_url: track.expanded_artwork_url.clone(),
                    links,
                    ..DetailView::default()
                }
            });
            #[cfg(feature = "wikidata")]
            {
                if let Some(details) = self.view.details.as_mut() {
                    if let Some(previous) = previous.as_ref().filter(|previous| {
                        previous.media_id.is_some() && previous.media_id == details.media_id
                    }) {
                        details.wikidata.clone_from(&previous.wikidata);
                        details.links.extend(
                            previous
                                .links
                                .iter()
                                .filter(|link| link.wikidata_item_id.is_some())
                                .cloned(),
                        );
                    }
                    preserve_same_media_wikidata_state(previous.as_ref(), details);
                }
                self.schedule_selected_soundcloud_wikidata(Instant::now());
            }
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
                .ok_or_else(|| {
                    "Select a SoundCloud track with full playback or a public preview".to_owned()
                })
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
}

/// Formats provider dates with the same local-calendar convention as other sources.
#[cfg(feature = "soundcloud")]
fn soundcloud_date(value: Option<&str>) -> String {
    value
        .and_then(|value| format_rfc3339_local_datetime_relative(value, Local::now().date_naive()))
        .unwrap_or_default()
}

/// Labels the playable preview duration separately from the full work's duration.
#[cfg(feature = "soundcloud")]
fn soundcloud_duration_label(track: &SoundcloakTrack) -> String {
    let duration = track
        .duration_seconds
        .map_or_else(String::new, format_seconds);
    if track.playback != SoundcloakPlayback::Preview {
        return duration;
    }
    track.full_duration_seconds.map_or_else(
        || format!("{duration} preview"),
        |full| format!("{duration} preview (full track {})", format_seconds(full)),
    )
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
