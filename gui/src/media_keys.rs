//! The keyboard's media keys, and the panel the desktop shows beside them.
//!
//! Every desktop has one place that answers Play, Pause, Next and Previous no
//! matter which application has focus, and shows what is playing while it does:
//! MPRIS on Linux, the System Media Transport Controls on Windows, Now Playing
//! on macOS. [`souvlaki`] is one interface over the three.
//!
//! This module is shaped like [`crate::desktop`], and for the same reason: the
//! decisions are pure functions over a published [`ViewModel`], and only the
//! last step touches a platform. [`announce`] says what the surface should be
//! told, [`command_for`] says what one of its buttons means — both are tested
//! here, on a machine where the platform half cannot be exercised at all.
//!
//! **Nothing is resolved against a snapshot.** A media key arrives from outside
//! the process, so it is queued and answered on the reducer thread against the
//! view the controller holds right now, exactly as a key press from the window
//! is. Play and Pause name a destination rather than a toggle, and answering
//! either against a stale snapshot would invert the state that was asked for.
//!
//! **No cover art.** [`souvlaki`] hands `cover_url` to the platform's own image
//! loader — `NSImage initWithContentsOfURL:` on macOS, a WinRT stream reference
//! on Windows, the desktop shell's fetcher through `mpris:artUrl` on Linux — so
//! a provider's thumbnail URL would be fetched by something other than Youta's
//! guarded agent, without its refusal to follow redirects, its pinning to public
//! addresses, or its size cap. The window renders artwork through the `youta://`
//! endpoint precisely so that no such fetch escapes; sending the same URL out
//! here would undo it for the sake of a thumbnail.
//!
//! On Linux this exposes Youta on the session bus, which is what MPRIS is: any
//! process the user is already running can then ask it to pause or to quit. That
//! is the bargain every MPRIS player makes, and it grants no reach that running
//! as the same user did not already grant.

use std::time::{Duration, Instant};

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};
use tauri::{AppHandle, Runtime};

use youta::domain::MediaId;
use youta::view::{UiAction, ViewModel};

use crate::desktop::bounded;
use crate::reducer::ReducerHandle;

/// Name Youta claims on the session bus, below `org.mpris.MediaPlayer2`.
const DBUS_NAME: &str = "youta";

/// Name the desktop shows for the player.
const DISPLAY_NAME: &str = "Youta";

/// Seconds a bare fast-forward or rewind moves.
///
/// The same five seconds the arrow keys move in either front-end. A surface
/// whose buttons step by a different amount is a third player.
const SEEK_STEP_SECONDS: i64 = 5;

/// Facts about the item, as the desktop panel should state them.
///
/// Absent facts are empty rather than missing: Windows' display updater writes
/// only the fields it is given, so leaving one out would keep the previous
/// track's artist beside the current track's title.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MediaFacts {
    /// Item title.
    pub title: String,
    /// Creator, where the provider names one.
    pub artist: String,
    /// Running time, once the backend knows it and it is not a live stream.
    pub duration: Option<Duration>,
}

/// Transport state, as the desktop panel should show it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaState {
    /// Nothing is loaded.
    Stopped,
    /// Something is loaded and held, at this position.
    Paused(Duration),
    /// Something is running, at this position.
    Playing(Duration),
}

impl MediaState {
    /// Whether two states differ in anything other than the clock.
    const fn same_kind(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::Stopped, Self::Stopped)
                | (Self::Paused(_), Self::Paused(_))
                | (Self::Playing(_), Self::Playing(_))
        )
    }

    /// The position this state carries, if it carries one.
    const fn position(self) -> Option<Duration> {
        match self {
            Self::Stopped => None,
            Self::Paused(position) | Self::Playing(position) => Some(position),
        }
    }
}

/// How long a running transport may go without being told the position again.
///
/// This is the whole reason the position is not simply pushed on every tick.
/// macOS and Windows *extrapolate* the elapsed time from the last value they
/// were given and the rate, so they need it again only when it jumps; telling
/// them once a second is both unnecessary and, on macOS, expensive — souvlaki
/// rebuilds and re-copies the entire now-playing dictionary per call, from a
/// thread with no autorelease pool, at a measured 0.9 KiB that is never
/// returned. MPRIS is the opposite: it answers `Position` with exactly the
/// value it was last told, so a client's seek bar moves only this often.
#[cfg(all(unix, not(target_os = "macos")))]
const POSITION_RESYNC: Duration = Duration::from_secs(1);

/// How long a running transport may go without being told the position again.
#[cfg(not(all(unix, not(target_os = "macos"))))]
const POSITION_RESYNC: Duration = Duration::from_secs(30);

/// How far the position may sit from the extrapolated one before it is a seek.
///
/// Wide enough that ordinary scheduling jitter is not mistaken for a jump, and
/// far narrower than the smallest seek either front-end offers.
const POSITION_TOLERANCE: Duration = Duration::from_secs(2);

/// What the desktop's media surface should be told, if anything.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MediaUpdate {
    /// New facts, when they changed.
    pub facts: Option<MediaFacts>,
    /// New transport state, when it changed.
    pub state: Option<MediaState>,
}

/// What the media surface was last told.
#[derive(Debug, Default)]
pub struct Announced {
    /// Identity behind the facts, so two items sharing a title still differ.
    media: Option<MediaId>,
    /// The facts already stated.
    facts: Option<MediaFacts>,
    /// The transport state already shown.
    state: Option<MediaState>,
    /// The position last published, and the moment it was published.
    ///
    /// Together with the rate this says where the surface believes playback is
    /// right now, which is what makes a seek distinguishable from time passing.
    clock: Option<(Duration, Instant)>,
    /// The volume already published, which only MPRIS carries a property for.
    #[cfg(all(unix, not(target_os = "macos")))]
    volume: Option<u8>,
}

/// Rounds a duration down to whole seconds, or drops it.
///
/// The panel displays a clock, so sub-second precision buys nothing and costs a
/// great deal: republishing four times a second would put four D-Bus signals
/// behind every tick of it, and mpv's reported duration wobbles in the
/// milliseconds even while the file does not change.
fn whole_seconds(duration: Duration) -> Duration {
    Duration::from_secs(duration.as_secs())
}

/// Returns what the media surface should be told, or `None` when it knows.
///
/// `now` is passed rather than read so the decision stays a pure function of
/// its inputs, which is the only way the position rules above are testable.
pub fn announce(view: &ViewModel, now: Instant, state: &mut Announced) -> Option<MediaUpdate> {
    let media = view
        .now_playing
        .as_ref()
        .map(|playing| playing.media_id.clone());
    let facts = match view.now_playing.as_ref() {
        Some(playing) => MediaFacts {
            title: bounded(&playing.title),
            artist: bounded(&playing.subtitle),
            // A live stream reports a growing, apparently finite duration, so
            // publishing it would redraw a progress bar that means nothing and
            // resend the metadata on every tick.
            duration: (!view.playback.live)
                .then_some(view.playback.duration)
                .flatten()
                .map(whole_seconds),
        },
        None => MediaFacts::default(),
    };

    let position = whole_seconds(view.playback.position);
    // An idle backend holds no media, whatever the queue still names, and it
    // reports itself paused because a backend with nothing loaded is not
    // running. Calling that "paused" would offer a Play button that resumes
    // nothing.
    let running = view.now_playing.is_some() && !view.playback.idle;
    let transport = match (running, view.playback.paused) {
        (false, _) => MediaState::Stopped,
        (true, true) => MediaState::Paused(position),
        (true, false) => MediaState::Playing(position),
    };

    let facts = (state.media != media || state.facts.as_ref() != Some(&facts)).then_some(facts);
    let transport = match state.state {
        // Nothing has been said yet, or the transport itself changed.
        None => true,
        Some(shown) if !shown.same_kind(transport) => true,
        // A new item starts a new clock whatever the numbers happen to be —
        // two tracks a second in are one second in each, and leaving the
        // surface extrapolating from the first would drift it through the
        // second.
        Some(_) if facts.is_some() => true,
        // Said, and still exactly true. A stopped player left alone is the
        // common case here, and it must not restate itself on every snapshot
        // the user's browsing produces.
        Some(shown) if shown == transport => false,
        Some(_) => position_is_due(transport, view.playback.speed, now, state),
    }
    .then_some(transport);
    if facts.is_none() && transport.is_none() {
        return None;
    }
    state.media = media;
    if let Some(facts) = facts.clone() {
        state.facts = Some(facts);
    }
    if let Some(transport) = transport {
        state.state = Some(transport);
        state.clock = transport.position().map(|position| (position, now));
    }
    Some(MediaUpdate {
        facts,
        state: transport,
    })
}

/// Whether the surface's own idea of the position has stopped being right.
///
/// True when playback jumped — a seek, a new item, a chapter — and, while the
/// transport is running, once every [`POSITION_RESYNC`] besides.
fn position_is_due(transport: MediaState, speed: f64, now: Instant, state: &Announced) -> bool {
    let (Some((published, published_at)), Some(position)) = (state.clock, transport.position())
    else {
        return true;
    };
    let elapsed = now.saturating_duration_since(published_at);
    if matches!(transport, MediaState::Playing(_)) && elapsed >= POSITION_RESYNC {
        return true;
    }
    // Only a running transport moves on its own, and it moves at the rate the
    // player was told to use. A held one is expected exactly where it was left.
    let expected = match transport {
        // The clamp keeps a nonsensical rate out of `mul_f64`, which panics on
        // one, rather than trusting a float that reached here from a backend.
        MediaState::Playing(_) => published.saturating_add(elapsed.mul_f64(speed.clamp(0.0, 8.0))),
        MediaState::Paused(_) | MediaState::Stopped => published,
    };
    expected.abs_diff(position) >= POSITION_TOLERANCE
}

/// What one press on the desktop's media surface asks Youta to do.
#[derive(Clone, Debug, PartialEq)]
pub enum MediaCommand {
    /// Something the reducer has a word for.
    Act(UiAction),
    /// Bring the window forward, which the reducer does not and should not.
    Show,
}

/// Turns one signed step into a seek in the requested direction.
fn seek(direction: SeekDirection, seconds: i64) -> UiAction {
    UiAction::SeekRelative(match direction {
        SeekDirection::Forward => seconds,
        SeekDirection::Backward => -seconds,
    })
}

/// Returns what one media-surface event means for this exact view.
///
/// `None` is the ordinary answer for a button asking for something that has
/// already happened — Play while playing, a position within a stream whose
/// length nobody knows.
pub fn command_for(event: &MediaControlEvent, view: &ViewModel) -> Option<MediaCommand> {
    // Only a backend holding media can be paused or resumed. Without this, a
    // Play arriving at an idle backend would toggle the pause flag underneath
    // it and the next real Play would pause instead.
    let holding = view.now_playing.is_some() && !view.playback.idle;
    let action = match event {
        // Play and Pause name a destination; Toggle names a change. Answering
        // the first two with a toggle would invert the requested state whenever
        // the surface and the player had drifted apart, which is exactly when
        // the button gets pressed.
        MediaControlEvent::Play if holding && view.playback.paused => UiAction::TogglePause,
        // Youta has no stop, and a stop button that does nothing is worse than
        // one that holds the item where it is.
        MediaControlEvent::Pause | MediaControlEvent::Stop if holding && !view.playback.paused => {
            UiAction::TogglePause
        }
        MediaControlEvent::Play | MediaControlEvent::Pause | MediaControlEvent::Stop => {
            return None;
        }
        MediaControlEvent::Toggle => UiAction::TogglePause,
        MediaControlEvent::Next => UiAction::PlayQueueNeighbour(1),
        // Previous is one entry back, with no "restart if past three seconds"
        // rule: that convention is invisible, and it turns one button into two
        // depending on a threshold nothing on screen shows.
        MediaControlEvent::Previous => UiAction::PlayQueueNeighbour(-1),
        MediaControlEvent::Seek(direction) => seek(*direction, SEEK_STEP_SECONDS),
        MediaControlEvent::SeekBy(direction, by) => {
            seek(*direction, i64::try_from(by.as_secs()).unwrap_or(i64::MAX))
        }
        MediaControlEvent::SetPosition(MediaPosition(position)) => {
            // The reducer seeks by percentage, so a length nobody knows yet
            // makes the request unanswerable rather than approximate.
            let duration = view.playback.duration?.as_secs_f64();
            if duration <= 0.0 {
                return None;
            }
            UiAction::SeekPercent((position.as_secs_f64() / duration * 100.0).clamp(0.0, 100.0))
        }
        MediaControlEvent::SetVolume(level) => {
            let target = (level.clamp(0.0, 1.0) * 100.0).round();
            let step = target - f64::from(view.playback.volume);
            // Both ends are already bounded to a hundred, so the conversion is
            // in range; the fallback is here because a saturating cast is not
            // something to leave a reader to infer.
            let step =
                i8::try_from(step as i64).unwrap_or(if step < 0.0 { i8::MIN } else { i8::MAX });
            if step == 0 {
                return None;
            }
            UiAction::ChangeVolume(step)
        }
        MediaControlEvent::Raise => return Some(MediaCommand::Show),
        MediaControlEvent::Quit => UiAction::Quit,
        // Youta plays what its providers resolved. A URI arriving from the
        // session bus is not one of those, and treating it as one would make
        // the media surface a way to feed the player arbitrary input.
        MediaControlEvent::OpenUri(_) => return None,
    };
    Some(MediaCommand::Act(action))
}

/// The platform's media surface, owned by the reducer thread.
pub struct Surface {
    controls: MediaControls,
    announced: Announced,
}

impl Surface {
    /// Registers Youta with the desktop's media surface.
    ///
    /// Returns `None`, having said why, when the platform declines: a Linux
    /// session with no D-Bus and a Windows window whose handle cannot be read
    /// are both ordinary, and neither is a reason to refuse to start.
    ///
    /// **Call this only once the reducer has reported itself ready.** Reading a
    /// window handle asks the main thread for it, and the main thread is inside
    /// `setup` waiting for that report.
    pub fn install<R: Runtime>(app: &AppHandle<R>) -> Option<Self> {
        let config = PlatformConfig {
            display_name: DISPLAY_NAME,
            dbus_name: DBUS_NAME,
            hwnd: window_handle(app)?,
        };
        let mut controls = match MediaControls::new(config) {
            Ok(controls) => controls,
            Err(error) => {
                eprintln!("the desktop offers Youta no media controls: {error:?}");
                return None;
            }
        };
        let app = app.clone();
        let attached = controls.attach(move |event| {
            // The reducer is reached the same way the menu reaches it, so the
            // event is resolved on its thread against the live view and this
            // callback holds nothing that could outlive the window.
            if let Some(reducer) = tauri::Manager::try_state::<ReducerHandle>(&app)
                && let Err(error) = reducer.media(event)
            {
                eprintln!("a media key could not reach the Youta reducer: {error}");
            }
        });
        if let Err(error) = attached {
            eprintln!("the desktop refused Youta's media controls: {error:?}");
            return None;
        }
        Some(Self {
            controls,
            announced: Announced::default(),
        })
    }

    /// Tells the surface whatever this snapshot changed.
    pub fn publish(&mut self, view: &ViewModel) {
        // MPRIS carries a volume property, and it is the one control whose
        // value the player must write back: souvlaki delivers the request as an
        // event and leaves the property alone until it is told the answer. The
        // other two platforms have no such property, so there is nothing to say.
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let volume = view.playback.volume;
            if self.announced.volume != Some(volume) {
                self.announced.volume = Some(volume);
                if let Err(error) = self.controls.set_volume(f64::from(volume) / 100.0) {
                    eprintln!("the Youta media surface refused a volume: {error:?}");
                }
            }
        }

        let Some(update) = announce(view, Instant::now(), &mut self.announced) else {
            return;
        };
        if let Some(facts) = update.facts.as_ref()
            && let Err(error) = self.controls.set_metadata(MediaMetadata {
                title: Some(&facts.title),
                artist: Some(&facts.artist),
                album: None,
                cover_url: None,
                duration: facts.duration,
            })
        {
            eprintln!("the Youta media surface refused the track details: {error:?}");
        }
        if let Some(state) = update.state
            && let Err(error) = self.controls.set_playback(match state {
                MediaState::Stopped => MediaPlayback::Stopped,
                MediaState::Paused(position) => MediaPlayback::Paused {
                    progress: Some(MediaPosition(position)),
                },
                MediaState::Playing(position) => MediaPlayback::Playing {
                    progress: Some(MediaPosition(position)),
                },
            })
        {
            eprintln!("the Youta media surface refused the transport state: {error:?}");
        }
    }
}

/// Returns the native window handle the platform's media surface requires.
///
/// Only Windows asks for one, and it refuses outright without it, so the outer
/// `Option` is the answer to "should Youta register at all".
#[cfg(target_os = "windows")]
fn window_handle<R: Runtime>(app: &AppHandle<R>) -> Option<Option<*mut std::ffi::c_void>> {
    use tauri::Manager as _;

    let window = app.get_webview_window(crate::desktop::MAIN_WINDOW)?;
    match window.hwnd() {
        Ok(handle) => Some(Some(handle.0)),
        Err(error) => {
            eprintln!("the Youta window has no handle to register media keys with: {error}");
            None
        }
    }
}

/// Returns the native window handle the platform's media surface requires.
#[cfg(not(target_os = "windows"))]
fn window_handle<R: Runtime>(_app: &AppHandle<R>) -> Option<Option<*mut std::ffi::c_void>> {
    Some(None)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use souvlaki::{MediaControlEvent, MediaPosition, SeekDirection};
    use youta::domain::{MediaId, SourceKind};
    use youta::view::{NowPlayingView, UiAction, ViewModel};

    use super::{
        Announced, MediaCommand, MediaFacts, MediaState, POSITION_RESYNC, announce, command_for,
    };

    /// Builds a snapshot of one identified item, held by a live backend.
    fn playing(id: &str, title: &str, artist: &str) -> ViewModel {
        let mut view = ViewModel {
            now_playing: Some(NowPlayingView {
                media_id: MediaId::new(SourceKind::YouTube, id),
                title: title.to_owned(),
                subtitle: artist.to_owned(),
            }),
            ..ViewModel::default()
        };
        view.playback.idle = false;
        view.playback.paused = false;
        view
    }

    /// The surface is told each thing once, and nothing while it is right.
    #[test]
    fn a_surface_that_is_already_right_is_told_nothing() {
        let mut state = Announced::default();
        let start = Instant::now();
        let mut view = playing("a", "First", "Creator");
        // A fraction of a second in, so the tick below lands on a whole second
        // the surface was never handed and it is the extrapolation that answers
        // rather than the two numbers happening to match.
        view.playback.position = Duration::from_millis(700);

        let first =
            announce(&view, start, &mut state).expect("the first snapshot states everything");
        assert_eq!(
            first.facts,
            Some(MediaFacts {
                title: "First".to_owned(),
                artist: "Creator".to_owned(),
                duration: None,
            })
        );
        assert_eq!(first.state, Some(MediaState::Playing(Duration::ZERO)));
        assert!(announce(&view, start, &mut state).is_none());

        // Time passing is not news for as long as the surface extrapolates it,
        // which is one resync window — a window that is two orders of magnitude
        // apart between MPRIS and the rest, so the tick is taken from it rather
        // than written down. Past it the interval speaks whatever the position
        // says, which `a_running_transport_is_resynchronised_on_its_own_interval`
        // covers.
        let tick = POSITION_RESYNC / 2;
        let mut moved = view.clone();
        moved.playback.position = view.playback.position + tick;
        assert!(
            announce(&moved, start + tick, &mut state).is_none(),
            "a position exactly where the surface expects it says nothing"
        );

        // Pausing is, and it carries the clock with it.
        let mut held = moved.clone();
        held.playback.paused = true;
        held.playback.position = Duration::from_millis(1_400);
        let update = announce(&held, start + tick, &mut state).expect("the transport changed");
        assert_eq!(update.facts, None);
        assert_eq!(
            update.state,
            Some(MediaState::Paused(Duration::from_secs(1)))
        );

        // And a held transport that nobody moved stays quiet however long it is
        // left, because a paused clock does not drift.
        assert!(announce(&held, start + Duration::from_secs(600), &mut state).is_none());
    }

    /// A jump is published at once; time passing is not.
    ///
    /// This is what separates a seek from a tick without asking the reducer to
    /// say which happened: the surface's own idea of where playback is comes
    /// from the value it was last given and the rate it was told.
    #[test]
    fn a_seek_is_told_immediately_and_a_tick_is_not() {
        let mut state = Announced::default();
        let start = Instant::now();
        let mut view = playing("a", "First", "");
        view.playback.duration = Some(Duration::from_secs(600));
        view.playback.position = Duration::from_millis(700);
        announce(&view, start, &mut state).expect("the first snapshot");

        // Half a resync window later, half a window in: exactly where it was
        // expected. Half, because a whole one is resent on the interval alone
        // and this is the extrapolation being tested, not the interval.
        let tick = POSITION_RESYNC / 2;
        view.playback.position += tick;
        assert!(announce(&view, start + tick, &mut state).is_none());

        // That same moment, a minute in: somebody sought.
        view.playback.position = Duration::from_secs(60);
        let update = announce(&view, start + tick, &mut state)
            .expect("a seek must reach the surface at once");
        assert_eq!(
            update.state,
            Some(MediaState::Playing(Duration::from_secs(60)))
        );

        // A doubled rate moves twice as far in the same time, and that is not a
        // seek either.
        view.playback.speed = 2.0;
        view.playback.position = Duration::from_secs(60) + tick * 2;
        assert!(announce(&view, start + tick * 2, &mut state).is_none());
    }

    /// A stopped player left alone must not restate itself.
    ///
    /// Browsing changes the snapshot constantly while nothing plays, and each of
    /// those would otherwise be another call into the platform to repeat what it
    /// already shows.
    #[test]
    fn a_player_that_is_still_stopped_says_nothing() {
        let mut state = Announced::default();
        let start = Instant::now();
        announce(&ViewModel::default(), start, &mut state).expect("the first snapshot");
        for second in 1..60 {
            assert!(
                announce(
                    &ViewModel::default(),
                    start + Duration::from_secs(second),
                    &mut state
                )
                .is_none(),
                "a stopped player restated itself after {second} seconds"
            );
        }
    }

    /// A new item restarts the clock even where the numbers happen to agree.
    #[test]
    fn a_new_item_carries_its_own_position() {
        let mut state = Announced::default();
        let start = Instant::now();
        let mut first = playing("a", "First", "");
        first.playback.position = Duration::from_secs(1);
        announce(&first, start, &mut state).expect("the first item");

        // A second item one second in, one second later: the position alone
        // would look like ordinary progress.
        let mut second = playing("b", "Second", "");
        second.playback.position = Duration::from_secs(1);
        let update = announce(&second, start + Duration::from_secs(1), &mut state)
            .expect("a new item is announced");
        assert!(update.facts.is_some());
        assert_eq!(
            update.state,
            Some(MediaState::Playing(Duration::from_secs(1))),
            "the surface must be re-anchored, not left extrapolating the old item"
        );
    }

    /// A running transport is resynchronised even when nothing jumped.
    ///
    /// MPRIS answers `Position` with the last value it was told, so this is the
    /// interval a client's seek bar actually moves at.
    #[test]
    fn a_running_transport_is_resynchronised_on_its_own_interval() {
        let mut state = Announced::default();
        let start = Instant::now();
        let mut view = playing("a", "First", "");
        announce(&view, start, &mut state).expect("the first snapshot");

        let resync = POSITION_RESYNC;
        view.playback.position = resync - Duration::from_millis(1);
        assert!(
            announce(&view, start + resync - Duration::from_millis(1), &mut state).is_none(),
            "before the interval there is nothing to resend"
        );
        view.playback.position = resync;
        assert!(
            announce(&view, start + resync, &mut state).is_some(),
            "the interval is what keeps a stored position from going stale"
        );
    }

    /// Two items can share a title, so identity decides when facts are restated.
    #[test]
    fn a_different_item_with_the_same_words_is_still_a_different_item() {
        let mut state = Announced::default();
        announce(&playing("a", "Untitled", ""), Instant::now(), &mut state)
            .expect("the first item");
        let update = announce(&playing("b", "Untitled", ""), Instant::now(), &mut state)
            .expect("a second item with the same words");
        assert!(
            update.facts.is_some(),
            "the panel must be restated for a track it cannot tell apart by name"
        );
    }

    /// A live stream has no length to draw a progress bar from.
    ///
    /// The backend reports a growing, apparently finite duration for one, so
    /// publishing it would both lie about the bar and restate the facts on
    /// every tick.
    #[test]
    fn a_live_stream_publishes_no_running_time() {
        let mut state = Announced::default();
        let mut view = playing("a", "A station", "");
        view.playback.live = true;
        view.playback.duration = Some(Duration::from_secs(3_600));
        let first = announce(&view, Instant::now(), &mut state).expect("the station is announced");
        assert_eq!(first.facts.expect("facts").duration, None);

        view.playback.duration = Some(Duration::from_secs(3_601));
        assert!(
            announce(&view, Instant::now(), &mut state).is_none(),
            "a growing live duration must not restate anything"
        );
    }

    /// A queue entry that nothing has started is stopped, not paused.
    #[test]
    fn a_queued_entry_no_backend_holds_is_stopped() {
        let mut state = Announced::default();
        let mut view = playing("a", "First", "");
        view.playback.idle = true;
        view.playback.paused = true;
        let update =
            announce(&view, Instant::now(), &mut state).expect("the queued entry is announced");
        assert_eq!(update.state, Some(MediaState::Stopped));

        // And the panel is emptied when the queue empties, rather than keeping
        // the last track beside a stopped transport.
        let mut state = Announced::default();
        announce(
            &playing("a", "First", "Creator"),
            Instant::now(),
            &mut state,
        )
        .expect("something plays");
        let stopped =
            announce(&ViewModel::default(), Instant::now(), &mut state).expect("it stops");
        assert_eq!(stopped.facts, Some(MediaFacts::default()));
        assert_eq!(stopped.state, Some(MediaState::Stopped));
    }

    /// Provider text reaches an operating system here too, so it is bounded.
    #[test]
    fn an_oversized_title_is_bounded_before_it_leaves_the_process() {
        let mut state = Announced::default();
        let facts = announce(
            &playing("a", &"é".repeat(4096), &"ß".repeat(4096)),
            Instant::now(),
            &mut state,
        )
        .expect("an update")
        .facts
        .expect("facts");
        assert_eq!(facts.title.chars().count(), 120);
        assert_eq!(facts.artist.chars().count(), 120);
    }

    /// Play and Pause name a destination, so neither may act as a toggle.
    ///
    /// This is the whole reason media events are resolved on the reducer thread
    /// rather than in the callback: a Play answered against a snapshot that had
    /// already resumed would pause.
    #[test]
    fn play_and_pause_ask_for_a_state_rather_than_a_change() {
        let mut playing_now = playing("a", "First", "");
        assert_eq!(command_for(&MediaControlEvent::Play, &playing_now), None);
        assert_eq!(
            command_for(&MediaControlEvent::Pause, &playing_now),
            Some(MediaCommand::Act(UiAction::TogglePause))
        );
        assert_eq!(
            command_for(&MediaControlEvent::Stop, &playing_now),
            Some(MediaCommand::Act(UiAction::TogglePause))
        );

        playing_now.playback.paused = true;
        assert_eq!(
            command_for(&MediaControlEvent::Play, &playing_now),
            Some(MediaCommand::Act(UiAction::TogglePause))
        );
        assert_eq!(command_for(&MediaControlEvent::Pause, &playing_now), None);
        assert_eq!(command_for(&MediaControlEvent::Stop, &playing_now), None);

        // Toggle is the one that names a change, so it always asks for one.
        assert_eq!(
            command_for(&MediaControlEvent::Toggle, &ViewModel::default()),
            Some(MediaCommand::Act(UiAction::TogglePause))
        );
        // An idle backend holds nothing to hold or release.
        assert_eq!(
            command_for(&MediaControlEvent::Play, &ViewModel::default()),
            None
        );
    }

    /// Skipping is a queue move, not a change to the list on screen.
    #[test]
    fn the_skip_buttons_move_through_the_queue() {
        let view = playing("a", "First", "");
        assert_eq!(
            command_for(&MediaControlEvent::Next, &view),
            Some(MediaCommand::Act(UiAction::PlayQueueNeighbour(1)))
        );
        assert_eq!(
            command_for(&MediaControlEvent::Previous, &view),
            Some(MediaCommand::Act(UiAction::PlayQueueNeighbour(-1)))
        );
    }

    /// Seeking uses the same five seconds both front-ends already use.
    #[test]
    fn seeking_matches_the_step_the_arrow_keys_take() {
        let view = playing("a", "First", "");
        assert_eq!(
            command_for(&MediaControlEvent::Seek(SeekDirection::Backward), &view),
            Some(MediaCommand::Act(UiAction::SeekRelative(-5)))
        );
        assert_eq!(
            command_for(
                &MediaControlEvent::SeekBy(SeekDirection::Forward, Duration::from_secs(30)),
                &view
            ),
            Some(MediaCommand::Act(UiAction::SeekRelative(30)))
        );
    }

    /// A dragged position is unanswerable until the length is known.
    #[test]
    fn a_position_is_refused_rather_than_guessed_without_a_length() {
        let mut view = playing("a", "First", "");
        let drag = MediaControlEvent::SetPosition(MediaPosition(Duration::from_secs(30)));
        assert_eq!(command_for(&drag, &view), None);

        view.playback.duration = Some(Duration::from_secs(120));
        assert_eq!(
            command_for(&drag, &view),
            Some(MediaCommand::Act(UiAction::SeekPercent(25.0)))
        );

        // Past the end is clamped rather than sent on as a percentage nothing
        // downstream expects.
        let beyond = MediaControlEvent::SetPosition(MediaPosition(Duration::from_secs(600)));
        assert_eq!(
            command_for(&beyond, &view),
            Some(MediaCommand::Act(UiAction::SeekPercent(100.0)))
        );
    }

    /// A desktop volume slider is absolute; the reducer's volume is a step.
    #[test]
    fn a_requested_volume_becomes_the_step_that_reaches_it() {
        let mut view = playing("a", "First", "");
        view.playback.volume = 80;
        assert_eq!(
            command_for(&MediaControlEvent::SetVolume(0.3), &view),
            Some(MediaCommand::Act(UiAction::ChangeVolume(-50)))
        );
        assert_eq!(command_for(&MediaControlEvent::SetVolume(0.8), &view), None);
        // MPRIS states that other values are accepted, so they are bounded here
        // rather than trusted.
        assert_eq!(
            command_for(&MediaControlEvent::SetVolume(9.0), &view),
            Some(MediaCommand::Act(UiAction::ChangeVolume(20)))
        );
    }

    /// The two events that are not about playback at all.
    #[test]
    fn raising_shows_the_window_and_a_uri_is_refused() {
        let view = ViewModel::default();
        assert_eq!(
            command_for(&MediaControlEvent::Raise, &view),
            Some(MediaCommand::Show)
        );
        assert_eq!(
            command_for(&MediaControlEvent::Quit, &view),
            Some(MediaCommand::Act(UiAction::Quit))
        );
        assert_eq!(
            command_for(
                &MediaControlEvent::OpenUri("file:///etc/passwd".to_owned()),
                &view
            ),
            None,
            "the media surface must not be a way to feed the player input"
        );
    }
}
