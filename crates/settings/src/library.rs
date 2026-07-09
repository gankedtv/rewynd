//! The clip library: every saved clip as a thumbnail card, and per clip a detail page with
//! play / show-in-folder / delete and an upload flow (title, destination, visibility) that
//! reuses the transport clients the tray uses.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use iced::widget::{
    Space, button, column, container, pick_list, row, scrollable, text, text_input,
};
use iced::{Background, Border, Element, Length, Task, Theme};

use rewynd_config::upload_history::{self, ClipKey, UploadRecord};
use rewynd_config::{ClipEntry, Config};
use rewynd_upload::youtube::{
    DEFAULT_CLIENT_ID, DEFAULT_CLIENT_SECRET, YouTubeClient, user_facing_youtube_error,
};
use rewynd_upload::{GankedClient, Visibility, default_title, titled, user_facing_upload_error};

use crate::anim::{ease, lerp_color};
use crate::player;
use crate::theme::{
    self, CONTENT_MAX_WIDTH, DISPLAY_BLACK, UI_BOLD, UI_SEMIBOLD, accent_button_style, accent_chip,
    card, field_label, hint, link_button, palette, primary_button, secondary_button, tinted,
    value_row,
};
use crate::thumbs;
use crate::trimbar;
use crate::video;

/// Cards per grid row (the body column is width-capped, so a fixed count stays balanced). Four
/// across suits the wider default window while staying readable if it is narrowed.
const GRID_COLUMNS: usize = 4;

/// Section label for clips saved outside a per-game subfolder (desktop / no game detected).
const ROOT_GROUP: &str = "Desktop";

/// Thumbnail decodes running at once. Each holds a full decoded frame briefly, so a big
/// library must stream through a small pool instead of decoding every stale clip at once.
const MAX_DECODES: usize = 4;

/// Keyframe thumbnails shown across the trim filmstrip.
const FILMSTRIP_FRAMES: usize = 12;

/// Decoded width of one filmstrip cell (~2x its ~60px logical size for hidpi sharpness).
const FILMSTRIP_CELL_WIDTH: u32 = 120;

/// Logical height of the detail page's preview pane.
const PREVIEW_HEIGHT: f32 = 240.0;

/// Buckets in the timeline's audio-peak lane.
const WAVEFORM_BUCKETS: usize = 300;

/// An upload destination the detail page can send a clip to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dest {
    Ganked,
    YouTube,
}

impl Dest {
    fn label(self) -> &'static str {
        match self {
            Dest::Ganked => "ganked.tv",
            Dest::YouTube => "YouTube",
        }
    }

    /// The stored history key for this destination.
    fn history_key(self) -> &'static str {
        match self {
            Dest::Ganked => upload_history::GANKED,
            Dest::YouTube => upload_history::YOUTUBE,
        }
    }
}

/// One filmstrip cell for the open clip: a decoded keyframe, still decoding, or undecodable.
enum StripCell {
    Loading,
    Ready(iced::widget::image::Handle),
    Failed,
}

/// One clip's thumbnail slot, keyed by path; `modified` is the freshness check.
enum Thumb {
    Loading {
        modified: SystemTime,
    },
    Ready {
        modified: SystemTime,
        handle: iced::widget::image::Handle,
        duration: Duration,
    },
    /// Corrupt or undecodable: the card gets a neutral placeholder, never a crash.
    Failed {
        modified: SystemTime,
    },
}

impl Thumb {
    fn modified(&self) -> SystemTime {
        match self {
            Thumb::Loading { modified }
            | Thumb::Ready { modified, .. }
            | Thumb::Failed { modified } => *modified,
        }
    }
}

/// Where the one in-flight (or just-finished) upload stands. One at a time: the whole view
/// keys off this, so a second upload can't start while one runs.
enum UploadState {
    Idle,
    Uploading {
        path: PathBuf,
        dest: Dest,
        abort: iced::task::Handle,
    },
    /// ganked.tv accepted the clip; polling its processing status until ready/failed.
    Processing {
        path: PathBuf,
        abort: iced::task::Handle,
    },
    /// Verifying whether an already-uploaded clip still exists remotely before re-uploading.
    Checking {
        path: PathBuf,
        dest: Dest,
    },
    /// The remote copy is still there, so a re-upload is blocked (with an "upload anyway" escape).
    Blocked {
        path: PathBuf,
        dest: Dest,
        link: Option<String>,
    },
    /// The remote copy could not be verified (offline, or YouTube's scope can't check); offer to
    /// upload anyway.
    Unverifiable {
        path: PathBuf,
        reason: String,
    },
    Done {
        path: PathBuf,
        dest: Dest,
        link: Option<String>,
        note: String,
    },
    Failed {
        path: PathBuf,
        error: String,
    },
}

/// A finished upload: the remote id (for history + later verification), the share/watch link
/// when the server issued one, and a note.
#[derive(Debug, Clone)]
pub struct Uploaded {
    remote_id: String,
    link: Option<String>,
    note: String,
}

/// The result of polling ganked.tv processing status.
#[derive(Debug, Clone)]
pub struct StatusOutcome {
    failed: bool,
    link: Option<String>,
    message: String,
}

/// How often to poll ganked.tv processing status, and for how many reads (≈5 minutes total).
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const POLL_MAX_READS: u32 = 60;

#[derive(Debug, Clone)]
pub enum Message {
    SearchEdited(String),
    GameFilterPicked(Option<String>),
    /// A directory rescan finished: the clips found plus the upload history read alongside
    /// (both come off one blocking task, so neither read stalls the UI thread).
    Scanned(Vec<ClipEntry>, Vec<UploadRecord>),
    /// The open clip's header was read: its duration in seconds (the trim range's ceiling).
    SummaryLoaded(PathBuf, f32),
    ThumbDone(PathBuf, SystemTime, Result<thumbs::Loaded, String>),
    Open(PathBuf),
    Back,
    Play,
    ShowInFolder,
    DeleteRequested,
    DeleteCancelled,
    DeleteConfirmed,
    Deleted(Result<PathBuf, String>),
    TrimStartChanged(f32),
    TrimEndChanged(f32),
    TrimSave(SaveMode),
    TrimSaved {
        src: PathBuf,
        mode: SaveMode,
        result: Result<PathBuf, String>,
    },
    TitleEdited(String),
    DestPicked(Dest),
    VisibilityPicked(Visibility),
    UploadPressed,
    UploadAgain,
    UploadAnyway,
    UploadCancelled,
    UploadDone(Result<Uploaded, String>),
    StatusPolled(PathBuf, Result<StatusOutcome, String>),
    Verified(PathBuf, Dest, Result<bool, String>),
    OpenLink(String),
    CopyLink(String),
    /// A frame tick while an accent fade is running (carries the frame instant).
    Tick(Instant),
    /// A filmstrip cell finished decoding: (clip path, cell index, decoded frame or error).
    StripDone(PathBuf, usize, Result<iced::widget::image::Handle, String>),
    /// A scrub decode finished: the frame at the trim handle the user dragged.
    ScrubDone(PathBuf, Result<iced::widget::image::Handle, String>),
    /// Start or pause the in-app preview playback of the kept range.
    PlayToggle,
    /// The next playback frame, with its position in the clip.
    PlayerFrame(crate::video::Frame, Duration),
    /// In-app playback has no decoder on this machine (ffmpeg missing).
    PlayerUnavailable,
    /// Playback reached the end of the kept range.
    PlayerEnded,
    /// The open clip's audio peaks arrived for the timeline lane (`None`: no audio/ffmpeg).
    WaveformDone(PathBuf, Option<Vec<f32>>),
    /// Grow the preview to fill the window, or shrink it back.
    FullscreenToggle,
    /// Leave the fullscreen preview (Escape).
    FullscreenExit,
    /// Move the playhead to this time (a press/drag between the trim edges).
    Seek(f32),
    /// A timeline drag was released; playback resumes here if seeking paused it.
    TrimDragEnd,
}

/// What a trim save does with the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveMode {
    /// Replace the original clip in place, so the open detail page (and the upload below it) act
    /// on the trimmed clip.
    Overwrite,
    /// Keep the original and write the trim to a new file.
    Copy,
}

/// Where the open clip's trim stands.
enum TrimState {
    Idle,
    Saving,
    Saved { overwrite: bool, path: PathBuf },
    Failed(String),
}

pub struct Library {
    entries: Vec<ClipEntry>,
    thumbs: HashMap<PathBuf, Thumb>,
    /// Thumbnail decodes not yet started; drained into `decoding` as slots free up.
    pending_thumbs: VecDeque<(PathBuf, SystemTime)>,
    /// The decodes currently on blocking tasks, keyed by path (the value is the mtime that
    /// decode was started for). Its size is the in-flight count, capped at [`MAX_DECODES`].
    decoding: HashMap<PathBuf, SystemTime>,
    scanning: bool,
    /// Fuzzy filter over game name / date / file name; empty shows everything.
    search: String,
    /// The game section the chip row has narrowed to, if any (`None` shows all).
    game_filter: Option<String>,
    /// The clip whose detail page is open, if any.
    open: Option<PathBuf>,
    /// The clip the filmstrip cells belong to (guards a late `StripDone` for a since-closed clip).
    strip_path: Option<PathBuf>,
    /// One slot per [`FILMSTRIP_FRAMES`] position for the open clip; empty when no clip is open.
    strip: Vec<StripCell>,
    /// Cells not yet decoding, drained into flight as slots free up: (cell index, position).
    strip_pending: VecDeque<(usize, f32)>,
    /// How many strip decodes are in flight (capped at [`MAX_DECODES`]).
    strip_decoding: usize,
    /// The frame under the trim handle last dragged, shown in the big preview.
    scrub_frame: Option<iced::widget::image::Handle>,
    /// Whether a scrub decode is in flight (one at a time; the latest request wins).
    scrub_busy: bool,
    /// The newest scrub time requested while a decode was in flight, in seconds.
    scrub_pending: Option<f32>,
    /// The `[start, end]` seconds the preview is playing, when playback runs (drives the
    /// playback subscription; `None` = stopped).
    play_range: Option<(f32, f32)>,
    /// The playback frame on screen.
    play_frame: Option<crate::video::Frame>,
    /// Where playback is (or paused), in seconds; resuming starts here.
    play_pos: Option<f32>,
    /// Why in-app playback can't run on this machine, when it can't (ffmpeg missing).
    play_error: Option<&'static str>,
    /// Whether seeking paused a running playback; releasing the drag resumes it.
    seek_resume: bool,
    /// Whether the preview fills the whole window.
    fullscreen: bool,
    /// The open clip's normalized audio peaks for the timeline lane.
    waveform: Option<Vec<f32>>,
    confirm_delete: bool,
    /// Trim in/out points for the open clip, in seconds; reset to the full span on open.
    trim_start: f32,
    trim_end: f32,
    /// The open clip's duration in seconds (read from its header on open), the trim range's ceiling.
    open_dur: f32,
    trim: TrimState,
    /// Play / show-in-folder / delete failure for the open clip.
    action_error: Option<String>,
    title: String,
    /// The suggested title, snapshotted when the detail page opens (recomputing it per
    /// `view()` would make the placeholder's minute stamp tick while the page sits open).
    title_hint: String,
    dest: Dest,
    /// An in-flight destination-accent fade, or `None` when the panel sits on a fixed brand.
    accent_fade: Option<AccentFade>,
    visibility: Visibility,
    upload: UploadState,
    /// Remembered successful uploads (per clip, per destination), for badges + the duplicate
    /// guard. Reloaded on each scan and after a record/forget.
    history: Vec<UploadRecord>,
}

impl Library {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            thumbs: HashMap::new(),
            pending_thumbs: VecDeque::new(),
            decoding: HashMap::new(),
            scanning: false,
            search: String::new(),
            game_filter: None,
            open: None,
            strip_path: None,
            strip: Vec::new(),
            strip_pending: VecDeque::new(),
            strip_decoding: 0,
            scrub_frame: None,
            scrub_busy: false,
            scrub_pending: None,
            play_range: None,
            play_frame: None,
            play_pos: None,
            play_error: None,
            seek_resume: false,
            fullscreen: false,
            waveform: None,
            confirm_delete: false,
            trim_start: 0.0,
            trim_end: 0.0,
            open_dur: 0.0,
            trim: TrimState::Idle,
            action_error: None,
            title: String::new(),
            title_hint: String::new(),
            dest: Dest::Ganked,
            accent_fade: None,
            visibility: Visibility::default(),
            upload: UploadState::Idle,
            // Filled by the first scan (the boot task); reading it here would block the UI
            // thread during startup.
            history: Vec::new(),
        }
    }

    /// Leave any open clip's detail and return to the grid (the nav "Library" tab / brand logo).
    pub fn show_grid(&mut self) {
        self.open = None;
        self.confirm_delete = false;
        self.action_error = None;
        self.clear_strip();
        self.reset_preview();
    }

    /// Drop the scrub frame and stop any preview playback (leaving a clip, or its file changed).
    fn reset_preview(&mut self) {
        self.scrub_frame = None;
        self.scrub_busy = false;
        self.scrub_pending = None;
        self.play_range = None;
        self.play_frame = None;
        self.play_pos = None;
        self.play_error = None;
        self.seek_resume = false;
        self.fullscreen = false;
    }

    /// The upload record for `entry` at `dest`, if the clip was uploaded there.
    fn record_for(&self, entry: &ClipEntry, dest: Dest) -> Option<&UploadRecord> {
        let key = clip_key(entry)?;
        self.history.iter().find(|r| {
            key.file_name == r.file_name
                && key.size_bytes == r.size_bytes
                && key.modified_millis == r.modified_millis
                && r.destination == dest.history_key()
        })
    }

    /// The destinations `entry` has already been uploaded to (for the card badges).
    fn uploaded_dests(&self, entry: &ClipEntry) -> Vec<Dest> {
        [Dest::Ganked, Dest::YouTube]
            .into_iter()
            .filter(|d| self.record_for(entry, *d).is_some())
            .collect()
    }

    /// Rescan the clip directory (the same resolution the recorder saves through) on a
    /// blocking task. Called on view-enter and by the Refresh button.
    pub fn refresh(&mut self, config: &Config) -> Task<Message> {
        self.scanning = true;
        let dir = rewynd_config::clips_dir(config.output_dir().as_deref());
        // The upload history rides along so badges + the duplicate guard reflect uploads made
        // from the tray (or a prior run) without a restart — and without a UI-thread file read.
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    (rewynd_config::list_clips(&dir), upload_history::load())
                })
                .await
                .unwrap_or_default()
            },
            |(entries, history)| Message::Scanned(entries, history),
        )
    }

    pub fn update(&mut self, message: Message, config: &Config) -> Task<Message> {
        match message {
            Message::SearchEdited(q) => self.search = q,
            Message::GameFilterPicked(game) => self.game_filter = game,
            Message::Scanned(entries, history) => return self.scanned(entries, history),
            Message::ThumbDone(path, modified, result) => {
                // Free the decode slot, unless a newer decode for the same path superseded it.
                if self.decoding.get(&path) == Some(&modified) {
                    self.decoding.remove(&path);
                }
                // Drop a stale result: the file changed since this decode started.
                if self.thumbs.get(&path).map(Thumb::modified) == Some(modified) {
                    let thumb = match result {
                        Ok(loaded) => Thumb::Ready {
                            modified,
                            handle: loaded.handle,
                            duration: loaded.duration,
                        },
                        Err(e) => {
                            tracing::warn!(path = %path.display(), error = %e, "no thumbnail");
                            Thumb::Failed { modified }
                        }
                    };
                    self.thumbs.insert(path, thumb);
                }
                return self.start_pending_decodes();
            }
            Message::Open(path) => {
                // The trim span resets to the clip's full duration once [`Message::SummaryLoaded`]
                // delivers it (the header read is file I/O, kept off the UI thread).
                self.open_dur = 0.0;
                self.trim_start = 0.0;
                self.trim_end = 0.0;
                self.trim = TrimState::Idle;
                self.reset_preview();
                // The suggested title leads with the game when one was detected.
                self.title_hint = match self.entry(&path).and_then(|e| e.game.as_deref()) {
                    Some(game) => titled(game),
                    None => default_title(),
                };
                self.open = Some(path.clone());
                self.confirm_delete = false;
                self.action_error = None;
                self.upload = UploadState::Idle;
                self.accent_fade = None;
                self.title = self.title_hint.clone();
                let (ganked, youtube) = dest_statuses(config);
                self.dest = if !ganked.ready() && youtube.ready() {
                    Dest::YouTube
                } else {
                    Dest::Ganked
                };
                self.visibility = self.default_visibility(config);
                return self.load_open_media(path);
            }
            Message::Back => self.show_grid(),
            Message::SummaryLoaded(path, dur) => {
                // The summary landing completes the open: the trim resets to the full span.
                // Handles can't be moved meaningfully before this (the span was 0 until now).
                if self.open.as_deref() == Some(path.as_path()) {
                    self.open_dur = dur;
                    self.trim_start = 0.0;
                    self.trim_end = dur;
                }
            }
            Message::Play => {
                if let Some(path) = &self.open
                    && let Err(e) = open::that_detached(path)
                {
                    self.action_error = Some(format!("Could not open a video player: {e}"));
                }
            }
            Message::ShowInFolder => {
                if let Some(dir) = self.open.as_ref().and_then(|p| p.parent())
                    && let Err(e) = open::that_detached(dir)
                {
                    self.action_error = Some(format!("Could not open the folder: {e}"));
                }
            }
            Message::DeleteRequested => self.confirm_delete = true,
            Message::DeleteCancelled => self.confirm_delete = false,
            Message::DeleteConfirmed => {
                self.confirm_delete = false;
                let Some(path) = self.open.clone() else {
                    return Task::none();
                };
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            std::fs::remove_file(&path)
                                .map(|()| path)
                                .map_err(|e| e.to_string())
                        })
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()))
                    },
                    Message::Deleted,
                );
            }
            Message::Deleted(Ok(path)) => {
                // The clip is gone; its cached preview (a frame of it) must not outlive it.
                if let Some(entry) = self.entries.iter().find(|e| e.path == path) {
                    thumbs::remove_cached(&path, entry.modified);
                }
                self.entries.retain(|e| e.path != path);
                self.thumbs.remove(&path);
                self.pending_thumbs.retain(|(p, _)| p != &path);
                if self.open == Some(path) {
                    self.open = None;
                    self.clear_strip();
                    self.reset_preview();
                }
                self.action_error = None;
            }
            Message::Deleted(Err(e)) => {
                self.action_error = Some(format!("Could not delete the clip: {e}"));
            }
            Message::TrimStartChanged(v) => {
                // Keep at least a small gap below the end.
                self.trim_start = v.clamp(0.0, (self.trim_end - 0.2).max(0.0));
                self.trim = TrimState::Idle;
                // Editing the range pauses playback; the preview scrubs to the handle instead.
                self.play_range = None;
                self.play_frame = None;
                self.play_pos = None;
                self.seek_resume = false;
                return self.scrub(self.trim_start);
            }
            Message::TrimEndChanged(v) => {
                let hi = self.open_duration();
                self.trim_end = v.clamp(self.trim_start + 0.2, hi.max(0.2));
                self.trim = TrimState::Idle;
                self.play_range = None;
                self.play_frame = None;
                self.play_pos = None;
                self.seek_resume = false;
                return self.scrub(self.trim_end);
            }
            Message::TrimSave(mode) => {
                let Some(src) = self.open.clone() else {
                    return Task::none();
                };
                let start = Duration::from_secs_f32(self.trim_start);
                let end = Duration::from_secs_f32(self.trim_end);
                self.trim = TrimState::Saving;
                let work_src = src.clone();
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || save_trim(&work_src, mode, start, end))
                            .await
                            .unwrap_or_else(|e| Err(e.to_string()))
                    },
                    move |result| Message::TrimSaved {
                        src: src.clone(),
                        mode,
                        result,
                    },
                );
            }
            Message::TrimSaved { src, mode, result } => {
                // The trim may finish after the user opened another clip; only stamp the result on
                // its own detail page. The rescan still runs so the new clip shows up regardless.
                let current = self.open.as_deref() == Some(src.as_path());
                match result {
                    Ok(path) => {
                        let overwrite = matches!(mode, SaveMode::Overwrite);
                        if current {
                            self.trim = TrimState::Saved { overwrite, path };
                        }
                        let mut tasks = vec![self.refresh(config)];
                        // Overwrite replaced the open clip in place: reload its span + filmstrip so
                        // the panel (and the upload below) work on the now-trimmed content.
                        if current && overwrite {
                            tasks.push(self.refresh_open_after_trim());
                        }
                        return Task::batch(tasks);
                    }
                    Err(e) if current => {
                        self.trim =
                            TrimState::Failed(format!("Could not save the trimmed clip: {e}"));
                    }
                    Err(_) => {}
                }
            }
            Message::TitleEdited(s) => self.title = s,
            Message::DestPicked(dest) => {
                if self.dest != dest {
                    let from = self.current_accent();
                    self.dest = dest;
                    self.visibility = self.default_visibility(config);
                    self.accent_fade = Some(AccentFade {
                        from,
                        to: dest_accent(dest),
                        start: None,
                        progress: 0.0,
                    });
                }
            }
            Message::VisibilityPicked(v) => self.visibility = v,
            Message::UploadPressed | Message::UploadAnyway => return self.start_upload(config),
            Message::UploadAgain => return self.recheck_before_reupload(config),
            Message::UploadCancelled => {
                match std::mem::replace(&mut self.upload, UploadState::Idle) {
                    UploadState::Uploading { abort, .. }
                    | UploadState::Processing { abort, .. } => {
                        abort.abort();
                    }
                    _ => {}
                }
            }
            Message::UploadDone(result) => return self.upload_done(result, config),
            Message::StatusPolled(path, result) => return self.status_polled(&path, result),
            Message::Verified(path, dest, result) => {
                return self.verified(path, dest, result, config);
            }
            Message::Tick(now) => self.advance_fade(now),
            Message::StripDone(path, index, result) => {
                if self.strip_path.as_deref() == Some(path.as_path()) {
                    self.strip_decoding = self.strip_decoding.saturating_sub(1);
                    if let Some(cell) = self.strip.get_mut(index) {
                        *cell = match result {
                            Ok(handle) => StripCell::Ready(handle),
                            Err(e) => {
                                tracing::warn!(path = %path.display(), index, error = %e, "no filmstrip frame");
                                StripCell::Failed
                            }
                        };
                    }
                    return self.start_strip_decodes();
                }
            }
            Message::ScrubDone(path, result) => {
                if self.open.as_deref() == Some(path.as_path()) {
                    self.scrub_busy = false;
                    if let Ok(handle) = result {
                        self.scrub_frame = Some(handle);
                    }
                    if let Some(secs) = self.scrub_pending.take() {
                        return self.scrub(secs);
                    }
                }
            }
            Message::PlayToggle => {
                if self.play_range.is_some() {
                    // Pause: the position sticks, so the next press resumes there.
                    self.play_range = None;
                } else {
                    // Resume from where playback paused when that is still inside the kept
                    // range, else start the range over. The paused frame stays up until the
                    // first new frame lands, so resuming never flashes an older image.
                    let from = self
                        .play_pos
                        .filter(|p| (self.trim_start..self.trim_end - 0.1).contains(p))
                        .unwrap_or(self.trim_start);
                    self.play_error = None;
                    self.play_range = Some((from, self.trim_end));
                }
            }
            Message::PlayerFrame(handle, pts) => {
                if self.play_range.is_some() {
                    self.play_frame = Some(handle);
                    self.play_pos = Some(pts.as_secs_f32());
                }
            }
            Message::PlayerUnavailable => {
                self.play_range = None;
                self.play_pos = None;
                self.play_error =
                    Some("In-app preview needs ffmpeg installed; use Open in player.");
            }
            Message::PlayerEnded => {
                self.play_range = None;
                self.play_pos = None;
            }
            Message::WaveformDone(path, peaks) => {
                if self.strip_path.as_deref() == Some(path.as_path()) {
                    self.waveform = peaks;
                }
            }
            Message::FullscreenToggle => self.set_fullscreen(!self.fullscreen),
            Message::FullscreenExit => self.set_fullscreen(false),
            Message::Seek(secs) => {
                let secs = secs.clamp(self.trim_start, self.trim_end.max(self.trim_start));
                self.play_pos = Some(secs);
                // Seeking scrubs (one ffmpeg per pixel would be a process storm); a paused
                // playback resumes from here when the drag lets go.
                if self.play_range.is_some() {
                    self.seek_resume = true;
                    self.play_range = None;
                }
                self.play_frame = None;
                return self.scrub(secs);
            }
            Message::TrimDragEnd => {
                if self.seek_resume {
                    self.seek_resume = false;
                    let from = self
                        .play_pos
                        .unwrap_or(self.trim_start)
                        .clamp(self.trim_start, (self.trim_end - 0.05).max(self.trim_start));
                    self.play_range = Some((from, self.trim_end));
                }
            }
            Message::OpenLink(url) => {
                if let Err(e) = open::that_detached(&url) {
                    tracing::warn!(error = %e, url, "could not open the link");
                }
            }
            Message::CopyLink(url) => return iced::clipboard::write(url),
        }
        Task::none()
    }

    /// Store a fresh scan and queue thumbnail decodes for entries whose (path, mtime) slot is
    /// missing or stale. The queue is rebuilt from this scan; decodes already in flight keep
    /// their slot and are not restarted.
    fn scanned(&mut self, entries: Vec<ClipEntry>, history: Vec<UploadRecord>) -> Task<Message> {
        self.scanning = false;
        self.thumbs
            .retain(|path, _| entries.iter().any(|e| &e.path == path));
        self.pending_thumbs.clear();
        for entry in &entries {
            let decoded = self.thumbs.get(&entry.path).is_some_and(|t| {
                t.modified() == entry.modified && !matches!(t, Thumb::Loading { .. })
            });
            let in_flight = self.decoding.get(&entry.path) == Some(&entry.modified);
            if decoded || in_flight {
                continue;
            }
            let modified = entry.modified;
            self.thumbs
                .insert(entry.path.clone(), Thumb::Loading { modified });
            self.pending_thumbs
                .push_back((entry.path.clone(), modified));
        }
        self.entries = entries;
        self.history = history;
        // Drop a game filter whose section vanished (its last clip was deleted or moved).
        let stale = self
            .game_filter
            .as_ref()
            .is_some_and(|game| !self.entries.iter().any(|e| group_label(e) == game));
        if stale {
            self.game_filter = None;
        }
        self.start_pending_decodes()
    }

    /// Start queued decodes until [`MAX_DECODES`] are in flight, each on its own blocking
    /// task; [`Message::ThumbDone`] frees a slot and comes back here for the next one.
    fn start_pending_decodes(&mut self) -> Task<Message> {
        let mut tasks = Vec::new();
        while self.decoding.len() < MAX_DECODES {
            let Some((path, modified)) = self.pending_thumbs.pop_front() else {
                break;
            };
            self.decoding.insert(path.clone(), modified);
            tasks.push(Task::perform(
                async move {
                    let result = tokio::task::spawn_blocking({
                        let path = path.clone();
                        move || thumbs::load(&path, modified)
                    })
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()));
                    (path, modified, result)
                },
                |(path, modified, result)| Message::ThumbDone(path, modified, result),
            ));
        }
        Task::batch(tasks)
    }

    /// Reload the open clip's trim span and filmstrip after an in-place overwrite: its duration
    /// and frames changed, so reset the range to the new full span and re-decode the strip.
    fn refresh_open_after_trim(&mut self) -> Task<Message> {
        let Some(path) = self.open.clone() else {
            return Task::none();
        };
        self.open_dur = 0.0;
        self.trim_start = 0.0;
        self.trim_end = 0.0;
        self.reset_preview();
        self.load_open_media(path)
    }

    /// (Re)build the open clip's timeline media: the header summary (the trim span's ceiling),
    /// the filmstrip cells and the audio-peak lane — all off the UI thread.
    fn load_open_media(&mut self, path: PathBuf) -> Task<Message> {
        self.waveform = None;
        let sum_path = path.clone();
        let summary = Task::perform(
            async move {
                let dur = tokio::task::spawn_blocking({
                    let path = sum_path.clone();
                    move || {
                        rewynd_mux::read::clip_summary(&path)
                            .map_or(0.0, |s| s.duration.as_secs_f32())
                    }
                })
                .await
                .unwrap_or(0.0);
                (sum_path, dur)
            },
            |(path, dur)| Message::SummaryLoaded(path, dur),
        );
        let wave_path = path.clone();
        let wave = Task::perform(
            async move {
                let peaks = tokio::task::spawn_blocking({
                    let path = wave_path.clone();
                    move || player::waveform(&path, WAVEFORM_BUCKETS)
                })
                .await
                .unwrap_or(None);
                (wave_path, peaks)
            },
            |(path, peaks)| Message::WaveformDone(path, peaks),
        );
        Task::batch([summary, self.build_strip(path), wave])
    }

    /// Grow or shrink the preview; a running playback restarts from its current position so the
    /// decode width follows the pane size.
    fn set_fullscreen(&mut self, on: bool) {
        if self.fullscreen == on {
            return;
        }
        self.fullscreen = on;
        if let (Some((_, end)), Some(pos)) = (self.play_range, self.play_pos) {
            self.play_range = Some((pos.min(end), end));
        }
    }

    /// The decode width for scrub frames and playback, matching the pane the preview fills.
    fn preview_width(&self) -> u32 {
        if self.fullscreen {
            player::FULLSCREEN_WIDTH
        } else {
            player::PREVIEW_WIDTH
        }
    }

    /// Decode the frame at `secs` into the big preview (the trim handle being dragged). One
    /// decode at a time; while one runs the newest request waits in `scrub_pending`.
    fn scrub(&mut self, secs: f32) -> Task<Message> {
        let Some(path) = self.open.clone() else {
            return Task::none();
        };
        if self.scrub_busy {
            self.scrub_pending = Some(secs);
            return Task::none();
        }
        self.scrub_busy = true;
        let position = secs / self.open_dur.max(f32::EPSILON);
        let width = self.preview_width();
        Task::perform(
            async move {
                let result = tokio::task::spawn_blocking({
                    let path = path.clone();
                    move || thumbs::load_at(&path, position, width)
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
                (path, result)
            },
            |(path, result)| Message::ScrubDone(path, result),
        )
    }

    /// Frame ticks while the accent fades, plus the preview-playback stream while it runs. The
    /// playback subscription is keyed by (clip, range); dropping it cancels the decode thread.
    pub fn subscription(&self) -> iced::Subscription<Message> {
        let mut subs = Vec::new();
        if self.animating() {
            subs.push(iced::window::frames().map(Message::Tick));
        }
        if let (Some(path), Some((start, end))) = (&self.open, self.play_range) {
            let key = (
                path.clone(),
                Duration::from_secs_f32(start.max(0.0)),
                Duration::from_secs_f32(end.max(0.0)),
                self.preview_width(),
            );
            subs.push(
                iced::Subscription::run_with(
                    key,
                    |(path, start, end, width): &(PathBuf, Duration, Duration, u32)| {
                        player::stream(path.clone(), *start, *end, *width)
                    },
                )
                .map(|event| match event {
                    player::Event::Frame(handle, pts) => Message::PlayerFrame(handle, pts),
                    player::Event::Unavailable => Message::PlayerUnavailable,
                    player::Event::Ended => Message::PlayerEnded,
                }),
            );
        }
        iced::Subscription::batch(subs)
    }

    /// Drop the filmstrip and waveform for whatever clip was open (free the decoded frames).
    fn clear_strip(&mut self) {
        self.strip_path = None;
        self.strip.clear();
        self.strip_pending.clear();
        self.strip_decoding = 0;
        self.waveform = None;
    }

    /// Queue all filmstrip cells for `path` and kick off the first decodes.
    fn build_strip(&mut self, path: PathBuf) -> Task<Message> {
        self.strip_path = Some(path);
        self.strip = (0..FILMSTRIP_FRAMES).map(|_| StripCell::Loading).collect();
        self.strip_pending = filmstrip_positions(FILMSTRIP_FRAMES)
            .into_iter()
            .enumerate()
            .collect();
        self.strip_decoding = 0;
        self.start_strip_decodes()
    }

    /// Start queued strip decodes until [`MAX_DECODES`] are in flight; each is one blocking
    /// `thumbs::load_at`. [`Message::StripDone`] frees a slot and returns here for the next.
    fn start_strip_decodes(&mut self) -> Task<Message> {
        let Some(path) = self.strip_path.clone() else {
            return Task::none();
        };
        let mut tasks = Vec::new();
        while self.strip_decoding < MAX_DECODES {
            let Some((index, position)) = self.strip_pending.pop_front() else {
                break;
            };
            self.strip_decoding += 1;
            let path = path.clone();
            tasks.push(Task::perform(
                async move {
                    let result = tokio::task::spawn_blocking({
                        let path = path.clone();
                        move || thumbs::load_at(&path, position, FILMSTRIP_CELL_WIDTH)
                    })
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()));
                    (path, index, result)
                },
                |(path, index, result)| Message::StripDone(path, index, result),
            ));
        }
        Task::batch(tasks)
    }

    /// The config's default visibility for the chosen destination.
    fn default_visibility(&self, config: &Config) -> Visibility {
        match self.dest {
            Dest::Ganked => Visibility::parse(&config.upload().visibility),
            Dest::YouTube => Visibility::parse(&config.youtube().visibility),
        }
    }

    /// Kick off the upload for the open clip: the client is built from the config at click
    /// time, exactly as the tray does. Only the destination-specific upload future differs;
    /// the readiness check and task scaffolding are shared.
    fn start_upload(&mut self, config: &Config) -> Task<Message> {
        if matches!(self.upload, UploadState::Uploading { .. }) {
            return Task::none();
        }
        let Some(path) = self.open.clone() else {
            return Task::none();
        };
        let dest = self.dest;
        let (ganked, youtube) = dest_statuses(config);
        let status = match dest {
            Dest::Ganked => ganked,
            Dest::YouTube => youtube,
        };
        if let Some(reason) = status.blocker(dest) {
            self.upload = UploadState::Failed {
                path,
                error: reason,
            };
            return Task::none();
        }
        let title = {
            let t = self.title.trim();
            if t.is_empty() {
                self.title_hint.clone()
            } else {
                t.to_owned()
            }
        };
        let visibility = self.visibility;
        let upload: std::pin::Pin<Box<dyn Future<Output = Result<Uploaded, String>> + Send>> = {
            let clip = path.clone();
            match dest {
                Dest::Ganked => Box::pin(upload_ganked(config.upload(), clip, title, visibility)),
                Dest::YouTube => {
                    Box::pin(upload_youtube(config.youtube(), clip, title, visibility))
                }
            }
        };
        let (task, abort) = Task::perform(upload, Message::UploadDone).abortable();
        self.upload = UploadState::Uploading { path, dest, abort };
        task
    }

    /// Handle a finished upload: remember it (so badges + the guard persist), copy the link to the
    /// clipboard, then for ganked.tv start polling its processing status; YouTube has no further
    /// server step to watch.
    fn upload_done(&mut self, result: Result<Uploaded, String>, config: &Config) -> Task<Message> {
        let UploadState::Uploading { path, dest, .. } =
            std::mem::replace(&mut self.upload, UploadState::Idle)
        else {
            return Task::none();
        };
        let done = match result {
            Ok(done) => done,
            Err(error) => {
                self.upload = UploadState::Failed { path, error };
                return Task::none();
            }
        };

        self.remember_upload(&path, dest, &done);
        let mut tasks = Vec::new();
        if let Some(url) = &done.link {
            tasks.push(iced::clipboard::write(url.clone()));
        }

        match dest {
            Dest::Ganked => {
                let up = config.upload();
                let clip_id = done.remote_id.clone();
                let for_path = path.clone();
                let (task, abort) = Task::perform(
                    async move { poll_ganked(up, for_path, clip_id).await },
                    |(path, result)| Message::StatusPolled(path, result),
                )
                .abortable();
                self.upload = UploadState::Processing { path, abort };
                tasks.push(task);
            }
            Dest::YouTube => {
                self.upload = UploadState::Done {
                    path,
                    dest,
                    link: done.link,
                    note: done.note,
                };
            }
        }
        Task::batch(tasks)
    }

    /// Fold a ganked.tv status poll into a terminal state; ignore a poll for a clip the user has
    /// since navigated away from. A share code usually only appears once processing finishes, so
    /// this is where the link is copied to the clipboard for ganked.tv (it was still `processing`
    /// at upload time).
    fn status_polled(
        &mut self,
        path: &Path,
        result: Result<StatusOutcome, String>,
    ) -> Task<Message> {
        if !matches!(&self.upload, UploadState::Processing { path: p, .. } if p == path) {
            return Task::none();
        }
        let path = path.to_path_buf();
        let mut task = Task::none();
        self.upload = match result {
            Ok(outcome) if outcome.failed => UploadState::Failed {
                path,
                error: outcome.message,
            },
            Ok(outcome) => {
                if let Some(url) = &outcome.link {
                    // Keep the badge's link fresh and put the now-available link on the clipboard.
                    self.update_history_link(&path, Dest::Ganked, url);
                    task = iced::clipboard::write(url.clone());
                }
                UploadState::Done {
                    path,
                    dest: Dest::Ganked,
                    link: outcome.link,
                    note: outcome.message,
                }
            }
            Err(error) => UploadState::Failed { path, error },
        };
        task
    }

    /// "Upload again" on an already-uploaded clip: verify the remote copy still exists before
    /// allowing a second upload. ganked.tv can be checked (a 404 means it was deleted); YouTube's
    /// upload-only scope can't, so it goes straight to the "upload anyway" escape.
    fn recheck_before_reupload(&mut self, config: &Config) -> Task<Message> {
        let Some(path) = self.open.clone() else {
            return Task::none();
        };
        let dest = self.dest;
        let Some(record) = self
            .open_entry()
            .and_then(|e| self.record_for(e, dest))
            .cloned()
        else {
            return self.start_upload(config);
        };
        match dest {
            Dest::Ganked => {
                self.upload = UploadState::Checking {
                    path: path.clone(),
                    dest,
                };
                let up = config.upload();
                let remote_id = record.remote_id;
                Task::perform(
                    async move { verify_ganked(up, path.clone(), remote_id).await },
                    move |(path, result)| Message::Verified(path, dest, result),
                )
            }
            Dest::YouTube => {
                self.upload = UploadState::Unverifiable {
                    path,
                    reason: "YouTube can't confirm whether the video is still up.".to_owned(),
                };
                Task::none()
            }
        }
    }

    /// Resolve a remote-existence check: gone → forget the record and upload; still there → block;
    /// couldn't check → offer to upload anyway.
    fn verified(
        &mut self,
        path: PathBuf,
        dest: Dest,
        result: Result<bool, String>,
        config: &Config,
    ) -> Task<Message> {
        if !matches!(&self.upload, UploadState::Checking { path: p, .. } if *p == path) {
            return Task::none();
        }
        match result {
            Ok(false) => {
                // The remote copy is gone; drop the stale record and upload afresh.
                if let Some(key) = self.open_entry().and_then(clip_key) {
                    let _ = upload_history::forget(&key, dest.history_key());
                    self.history = upload_history::load();
                }
                self.start_upload(config)
            }
            Ok(true) => {
                let link = self
                    .open_entry()
                    .and_then(|e| self.record_for(e, dest))
                    .and_then(|r| r.url.clone());
                self.upload = UploadState::Blocked { path, dest, link };
                Task::none()
            }
            Err(reason) => {
                self.upload = UploadState::Unverifiable { path, reason };
                Task::none()
            }
        }
    }

    /// Persist a successful upload and refresh the in-memory history so the badge + guard update
    /// without a rescan.
    fn remember_upload(&mut self, path: &Path, dest: Dest, done: &Uploaded) {
        let Some(entry) = self.entries.iter().find(|e| e.path == path) else {
            return;
        };
        let Some(key) = clip_key(entry) else {
            return;
        };
        let record = UploadRecord {
            file_name: key.file_name,
            size_bytes: key.size_bytes,
            modified_millis: key.modified_millis,
            destination: dest.history_key().to_owned(),
            remote_id: done.remote_id.clone(),
            url: done.link.clone(),
            uploaded_millis: ClipKey::now_millis(),
        };
        if let Err(e) = upload_history::record(record) {
            tracing::warn!(error = %e, "could not record the upload");
        }
        self.history = upload_history::load();
    }

    /// Update a stored record's link (e.g. once ganked.tv issues the share code after processing).
    fn update_history_link(&mut self, path: &Path, dest: Dest, url: &str) {
        let Some(entry) = self.entries.iter().find(|e| e.path == path) else {
            return;
        };
        let Some(record) = self.record_for(entry, dest).cloned() else {
            return;
        };
        if record.url.as_deref() == Some(url) {
            return;
        }
        let updated = UploadRecord {
            url: Some(url.to_owned()),
            ..record
        };
        if upload_history::record(updated).is_ok() {
            self.history = upload_history::load();
        }
    }

    /// The `ClipEntry` for the open clip, if any.
    fn open_entry(&self) -> Option<&ClipEntry> {
        let path = self.open.as_ref()?;
        self.entries.iter().find(|e| &e.path == path)
    }

    /// The (fill, ink) accent the upload panel paints with right now: the destination brand,
    /// or the interpolated value while a switch is fading.
    fn current_accent(&self) -> (iced::Color, iced::Color) {
        self.accent_fade
            .as_ref()
            .map_or_else(|| dest_accent(self.dest), AccentFade::accent)
    }

    /// Whether an accent fade is running (drives the frame subscription in `main`).
    pub fn animating(&self) -> bool {
        self.accent_fade.is_some()
    }

    /// Advance any running accent fade to frame time `now`, dropping it once complete so the
    /// frame subscription in `main` stops (no idle redraw).
    fn advance_fade(&mut self, now: Instant) {
        if let Some(fade) = &mut self.accent_fade
            && fade.advance(now)
        {
            self.accent_fade = None;
        }
    }

    pub fn view(&self, config: &Config) -> Element<'_, Message> {
        let open = self.open.as_ref().and_then(|p| self.entry(p));
        // Fullscreen preview bypasses the page shell (no width cap, no scroll).
        if self.fullscreen
            && let Some(entry) = open
        {
            return self.fullscreen_view(entry);
        }
        let body: Element<Message> = match open {
            Some(entry) => self.detail(entry, dest_statuses(config)),
            None => self.grid(),
        };
        let content = container(
            column![body]
                .spacing(20)
                .padding(28)
                .max_width(CONTENT_MAX_WIDTH)
                .width(Length::Fill),
        )
        .center_x(Length::Fill);
        container(crate::scroll::smooth(scrollable(content)))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn entry(&self, path: &Path) -> Option<&ClipEntry> {
        self.entries.iter().find(|e| e.path == path)
    }

    /// The default page: header + the clip cards, grouped by game, newest first.
    fn grid(&self) -> Element<'_, Message> {
        let title = column![
            text("LIBRARY").size(32).font(DISPLAY_BLACK),
            text(self.library_stats())
                .size(12)
                .font(UI_SEMIBOLD)
                .style(tinted(palette::TEXT_SECONDARY)),
        ]
        .spacing(4);
        // No manual refresh and no refresh indicator: the directory watcher and window focus
        // rescan silently — a "Refreshing..." that flashed on every focus was more distracting
        // than useful.
        let header = row![title].spacing(12).align_y(iced::Alignment::Center);

        if self.entries.is_empty() {
            let empty = column![
                text("NO CLIPS YET").size(22).font(DISPLAY_BLACK),
                hint(if self.scanning {
                    "Looking for saved clips..."
                } else {
                    "Clips you save appear here. Press your hotkey while playing to save one."
                }),
            ]
            .spacing(8)
            .align_x(iced::Alignment::Center);
            return column![
                header,
                container(empty)
                    .center_x(Length::Fill)
                    .padding([80, 0])
                    .width(Length::Fill),
            ]
            .spacing(20)
            .into();
        }

        let groups = self.grouped();
        let mut sections = column![].spacing(24);
        if groups.is_empty() {
            // The empty result can come from the search box, the game chips, or both; word it
            // for whichever the user actually touched.
            let reason = if self.search.trim().is_empty() {
                "No clips in this section."
            } else {
                "No clips match your search."
            };
            sections = sections.push(
                container(hint(reason))
                    .center_x(Length::Fill)
                    .padding([48, 0])
                    .width(Length::Fill),
            );
        }
        for (label, clips) in &groups {
            sections = sections.push(self.section(label, clips));
        }
        column![header, self.controls(), sections]
            .spacing(20)
            .into()
    }

    /// The "N clips · X.X GB" line under the title, over the whole library (unfiltered).
    fn library_stats(&self) -> String {
        let count = self.entries.len();
        let bytes: u64 = self.entries.iter().map(|e| e.size_bytes).sum();
        let clips = if count == 1 { "clip" } else { "clips" };
        format!("{count} {clips} · {} on disk", disk_label(bytes))
    }

    /// Search field plus a chip per game section (and an "All" chip), shown only when there is
    /// more than one section to move between.
    fn controls(&self) -> Element<'_, Message> {
        let search = text_input("Search clips", &self.search)
            .on_input(Message::SearchEdited)
            .style(theme::arena_input)
            .width(Length::Fixed(260.0));

        let labels = self.game_labels();
        let mut chips = row![chip(
            "All",
            self.game_filter.is_none(),
            Message::GameFilterPicked(None)
        )]
        .spacing(8)
        .align_y(iced::Alignment::Center);
        if labels.len() > 1 {
            for label in labels {
                let active = self.game_filter.as_deref() == Some(label.as_str());
                chips = chips.push(chip(
                    &label,
                    active,
                    Message::GameFilterPicked(Some(label.clone())),
                ));
            }
        }

        row![search, Space::new().width(Length::Fill), chips]
            .spacing(12)
            .align_y(iced::Alignment::Center)
            .into()
    }

    /// One game section: a header (label + clip count) over that group's grid. `label`'s
    /// lifetime is independent of the returned element (its text is owned), so the caller's
    /// grouping scratch does not have to outlive the view.
    fn section<'a>(&'a self, label: &str, clips: &[&'a ClipEntry]) -> Element<'a, Message> {
        let head = row![
            text(label.to_uppercase()).size(15).font(UI_BOLD),
            text(format!("{}", clips.len()))
                .size(11)
                .font(UI_SEMIBOLD)
                .style(tinted(palette::MUTED)),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center);

        let mut rows = column![].spacing(14);
        for chunk in clips.chunks(GRID_COLUMNS) {
            let mut r = row![].spacing(14);
            for entry in chunk {
                r = r.push(self.clip_card(entry));
            }
            for _ in chunk.len()..GRID_COLUMNS {
                r = r.push(Space::new().width(Length::Fill));
            }
            rows = rows.push(r);
        }
        column![head, rows].spacing(12).into()
    }

    /// The filtered entries grouped by game, each group newest-first, groups ordered by most
    /// recent activity (entries arrive newest-first, so first sighting fixes group order).
    fn grouped(&self) -> Vec<(String, Vec<&ClipEntry>)> {
        let mut groups: Vec<(String, Vec<&ClipEntry>)> = Vec::new();
        for entry in self.entries.iter().filter(|e| self.matches(e)) {
            let label = group_label(entry);
            if let Some(group) = groups.iter_mut().find(|(l, _)| l == label) {
                group.1.push(entry);
            } else {
                groups.push((label.to_owned(), vec![entry]));
            }
        }
        groups
    }

    /// Distinct game section labels present, ordered by most recent activity (for the chip row).
    fn game_labels(&self) -> Vec<String> {
        let mut labels: Vec<String> = Vec::new();
        for entry in &self.entries {
            let label = group_label(entry);
            if !labels.iter().any(|l| l == label) {
                labels.push(label.to_owned());
            }
        }
        labels
    }

    /// Whether `entry` passes the active game filter and the search query.
    fn matches(&self, entry: &ClipEntry) -> bool {
        if let Some(filter) = &self.game_filter
            && group_label(entry) != filter
        {
            return false;
        }
        let query = self.search.trim();
        if query.is_empty() {
            return true;
        }
        let name = entry
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        fuzzy_match(query, group_label(entry))
            || fuzzy_match(query, &saved_at_label(entry.saved_at))
            || fuzzy_match(query, name)
    }

    fn clip_card<'a>(&'a self, entry: &'a ClipEntry) -> Element<'a, Message> {
        // Stack the chips over the "duration · size" line rather than inlining them: a narrow card
        // (four across) can't fit a game chip, an upload badge, and the readout on one row, and the
        // squeezed row wrapped unevenly. Stacking keeps every card the same height at any width.
        let mut info = column![
            text(saved_at_label(entry.saved_at))
                .size(12)
                .font(UI_SEMIBOLD)
                .style(tinted(palette::TEXT)),
        ]
        .spacing(7);
        let chips = self.meta_chips(entry);
        if !chips.is_empty() {
            let mut chip_row = row![].spacing(6).align_y(iced::Alignment::Center);
            for chip in chips {
                chip_row = chip_row.push(chip);
            }
            info = info.push(chip_row);
        }
        let info = info.push(
            text(self.meta_text(entry))
                .size(10)
                .style(tinted(palette::MUTED)),
        );

        button(
            column![
                container(
                    layered([self.thumbnail(entry, 148.0)])
                        .width(Length::Fill)
                        .height(Length::Fixed(148.0)),
                )
                .clip(true),
                container(info).padding([11, 12]),
            ]
            .spacing(0),
        )
        .on_press(Message::Open(entry.path.clone()))
        .style(clip_card_style)
        .padding(0)
        .width(Length::Fill)
        .into()
    }

    /// The clip's chips in order (per-game first, then one per uploaded destination). Empty when
    /// the clip has no detected game and hasn't been uploaded anywhere.
    fn meta_chips<'a>(&'a self, entry: &'a ClipEntry) -> Vec<Element<'a, Message>> {
        let mut chips: Vec<Element<'a, Message>> = Vec::new();
        if let Some(game) = &entry.game {
            chips.push(accent_chip(game.clone()));
        }
        for dest in self.uploaded_dests(entry) {
            chips.push(accent_chip(format!("On {}", dest.label())));
        }
        chips
    }

    /// The "duration · size" readout (duration only once the thumbnail decode has reported it).
    fn meta_text(&self, entry: &ClipEntry) -> String {
        let mut meta = size_label(entry.size_bytes);
        if let Some(Thumb::Ready { duration, .. }) = self.thumbs.get(&entry.path) {
            meta = format!("{} · {meta}", duration_label(*duration));
        }
        meta
    }

    /// The chips followed by the "duration · size" readout on a single row, for the detail page
    /// (which has the width for it; the cards stack the two so a narrow card never wraps).
    fn meta_row<'a>(
        &'a self,
        entry: &'a ClipEntry,
        size: f32,
        color: iced::Color,
    ) -> Element<'a, Message> {
        let mut line = row![].spacing(8).align_y(iced::Alignment::Center);
        for chip in self.meta_chips(entry) {
            line = line.push(chip);
        }
        line.push(text(self.meta_text(entry)).size(size).style(tinted(color)))
            .into()
    }

    /// The clip's thumbnail at the given height, or a neutral placeholder while it loads
    /// (or when the clip can't be decoded).
    fn thumbnail<'a>(&'a self, entry: &'a ClipEntry, height: f32) -> Element<'a, Message> {
        match self.thumbs.get(&entry.path) {
            Some(Thumb::Ready { handle, .. }) => frame_image(handle.clone(), height),
            Some(Thumb::Failed { .. }) => placeholder("No preview", height),
            _ => placeholder("Loading...", height),
        }
    }

    /// The moving preview frame as an element, when there is one: the live playback frame (a GPU
    /// video widget — updating one texture in place, so no per-frame image-atlas churn/flicker),
    /// else the frame under the trim handle last dragged (a one-off still, fine as an image).
    /// Both are letterboxed (`Contain`) to the box.
    fn moving_frame<'a>(&self) -> Option<Element<'a, Message>> {
        if let Some(frame) = &self.play_frame {
            Some(video::video(frame.clone()))
        } else {
            self.scrub_frame.clone().map(|handle| {
                iced::widget::image(handle)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .content_fit(iced::ContentFit::Contain)
                    .into()
            })
        }
    }

    /// The big detail preview: the moving frame (else the thumbnail), the brand mark as the
    /// centred play control, click-anywhere pause, and the fullscreen toggle.
    fn preview<'a>(&'a self, entry: &'a ClipEntry) -> Element<'a, Message> {
        let frame: Element<Message> = match self.moving_frame() {
            Some(element) => element,
            None => self.thumbnail(entry, PREVIEW_HEIGHT),
        };
        let playing = self.play_range.is_some();
        let mut layers: Vec<Element<Message>> = vec![
            iced::widget::mouse_area(frame)
                .on_press(Message::PlayToggle)
                .into(),
        ];
        if !playing {
            layers.push(
                container(logo_play_button(56.0))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(iced::Alignment::Center)
                    .align_y(iced::Alignment::Center)
                    .into(),
            );
        }
        layers.push(
            container(
                button(text("Fullscreen").size(12).font(UI_SEMIBOLD))
                    .on_press(Message::FullscreenToggle)
                    .style(theme::overlay_button)
                    .padding([7, 13]),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(iced::Alignment::End)
            .align_y(iced::Alignment::End)
            .padding(10)
            .into(),
        );
        // Layered above an empty base: iced only hard-clips layered stack children, so this
        // keeps the Cover-scaled frame inside the box (see `layered`).
        let stack = layered(layers)
            .width(Length::Fill)
            .height(Length::Fixed(PREVIEW_HEIGHT));
        let mut col = column![container(stack).clip(true).style(theme::card_style)].spacing(6);
        if let Some(reason) = self.play_error {
            col = col.push(hint(reason));
        }
        col.into()
    }

    /// The fullscreen preview: the video filling the window (letterboxed, never cropped), the
    /// timeline underneath so trimming keeps working, and an exit control (also Escape).
    fn fullscreen_view<'a>(&'a self, entry: &'a ClipEntry) -> Element<'a, Message> {
        let frame: Element<Message> = match self.moving_frame() {
            Some(element) => element,
            None => self.thumbnail(entry, 400.0),
        };
        let playing = self.play_range.is_some();
        let mut layers: Vec<Element<Message>> = vec![
            iced::widget::mouse_area(frame)
                .on_press(Message::PlayToggle)
                .into(),
        ];
        if !playing {
            layers.push(
                container(logo_play_button(96.0))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(iced::Alignment::Center)
                    .align_y(iced::Alignment::Center)
                    .into(),
            );
        }
        let video = layered(layers).width(Length::Fill).height(Length::Fill);

        let stat = |label: &'static str, secs: f32| {
            text(format!("{label} {}", secs_label(secs)))
                .size(12)
                .font(UI_SEMIBOLD)
                .style(tinted(palette::TEXT_SECONDARY))
        };
        let controls = row![
            stat("Start", self.trim_start),
            stat("End", self.trim_end),
            stat("Length", (self.trim_end - self.trim_start).max(0.0)),
            Space::new().width(Length::Fill),
            button(text("Exit fullscreen").size(11).font(UI_SEMIBOLD))
                .on_press(Message::FullscreenExit)
                .style(secondary_button)
                .padding([6, 12]),
        ]
        .spacing(16)
        .align_y(iced::Alignment::Center);

        column![
            container(video).width(Length::Fill).height(Length::Fill),
            self.filmstrip(),
            controls,
        ]
        .spacing(12)
        .padding(16)
        .height(Length::Fill)
        .into()
    }

    /// The detail page for one clip: preview, facts, actions, and the upload panel.
    fn detail<'a>(
        &'a self,
        entry: &'a ClipEntry,
        (ganked, youtube): (DestStatus, DestStatus),
    ) -> Element<'a, Message> {
        let back = button(text("← Back to library").size(12).font(UI_SEMIBOLD))
            .on_press(Message::Back)
            .style(link_button)
            .padding(0);

        // The heading leads with the game when one was detected.
        let heading = match &entry.game {
            Some(game) => format!("{game} · {}", saved_at_label(entry.saved_at)),
            None => saved_at_label(entry.saved_at),
        };
        let mut facts = column![
            text(heading.to_uppercase()).size(26).font(DISPLAY_BLACK),
            self.meta_row(entry, 11.0, palette::TEXT_SECONDARY),
            hint(entry.path.display().to_string()),
        ]
        .spacing(10);
        facts = facts.push(self.actions());
        if let Some(e) = &self.action_error {
            facts = facts.push(text(e.clone()).size(12).style(tinted(palette::DANGER)));
        }

        let top = row![
            container(self.preview(entry)).width(Length::FillPortion(5)),
            container(facts).width(Length::FillPortion(4)),
        ]
        .spacing(20);

        column![
            back,
            top,
            self.trim_panel(),
            self.upload_panel(entry, ganked, youtube),
        ]
        .spacing(20)
        .into()
    }

    /// The open clip's duration in seconds (read from its header on open), the trim ceiling.
    fn open_duration(&self) -> f32 {
        self.open_dur
    }

    /// A row of keyframe thumbnails across the whole clip, with the draggable [`TrimBar`] on top:
    /// the kept `[start, end]` band stays lit with mint handles, the rest is scrimmed.
    fn filmstrip(&self) -> Element<'_, Message> {
        if self.strip.is_empty() {
            return Space::new().into();
        }
        let mut cells = row![].width(Length::Fill).height(Length::Fill);
        for cell in &self.strip {
            let content: Element<Message> = match cell {
                StripCell::Ready(handle) => iced::widget::image(handle.clone())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .content_fit(iced::ContentFit::Cover)
                    .into(),
                _ => container(Space::new())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .style(|_: &Theme| container::Style {
                        background: Some(Background::Color(palette::HIGH)),
                        ..container::Style::default()
                    })
                    .into(),
            };
            cells = cells.push(
                container(content)
                    .width(Length::FillPortion(1))
                    .height(Length::Fill),
            );
        }

        let bar = trimbar::TrimBar::new(
            self.trim_start,
            self.trim_end,
            self.open_dur,
            Message::TrimStartChanged,
            Message::TrimEndChanged,
        )
        .playhead(self.play_pos)
        .waveform(self.waveform.as_deref())
        .on_seek(Message::Seek)
        .on_released(Message::TrimDragEnd);
        let stack = layered([cells.into(), bar.into()])
            .width(Length::Fill)
            .height(Length::Fill);
        container(stack)
            .width(Length::Fill)
            .height(Length::Fixed(64.0))
            .clip(true)
            .style(theme::card_style)
            .into()
    }

    /// The trim panel: a start/end range over the clip, and a lossless "save a trimmed copy" action.
    fn trim_panel(&self) -> Element<'_, Message> {
        let dur = self.open_dur.max(0.2);
        let start = self.trim_start.clamp(0.0, dur);
        let end = self.trim_end.clamp(0.0, dur);
        let length = (end - start).max(0.0);

        let panel = column![
            self.filmstrip(),
            value_row("Start", secs_label(start)),
            value_row("End", secs_label(end)),
            value_row("Trimmed length", secs_label(length)),
            hint(
                "Drag the handles on the timeline to set the range. The start snaps back to the \
                 nearest keyframe (about a second), so the cut is lossless. Save replaces this \
                 clip; Save as copy keeps the original.",
            ),
        ]
        .spacing(14);

        let busy = matches!(self.trim, TrimState::Saving);
        // Saving an untouched range would only copy the clip; wait until a handle moved.
        let changed = start > 0.05 || end < dur - 0.05;
        let ready = !busy && length >= 0.2 && changed;
        let mut save = button(text("Save").size(13).font(UI_BOLD))
            .style(primary_button)
            .padding([11, 24]);
        let mut save_copy = button(text("Save as copy").size(12).font(UI_SEMIBOLD))
            .style(secondary_button)
            .padding([11, 20]);
        if ready {
            save = save.on_press(Message::TrimSave(SaveMode::Overwrite));
            save_copy = save_copy.on_press(Message::TrimSave(SaveMode::Copy));
        }
        let panel = panel.push(
            container(
                row![save, save_copy]
                    .spacing(10)
                    .align_y(iced::Alignment::Center),
            )
            .center_x(Length::Fill),
        );

        let panel = match &self.trim {
            TrimState::Idle => panel,
            TrimState::Saving => panel.push(hint("Saving the trimmed clip...")),
            TrimState::Saved {
                overwrite: true, ..
            } => panel.push(
                text("Trimmed. Ready to upload below.")
                    .size(12)
                    .style(tinted(palette::ACCENT)),
            ),
            TrimState::Saved {
                overwrite: false,
                path,
            } => panel.push(
                column![
                    text("Saved a trimmed copy.")
                        .size(12)
                        .style(tinted(palette::ACCENT)),
                    hint(path.display().to_string()),
                ]
                .spacing(6),
            ),
            TrimState::Failed(e) => {
                panel.push(text(e.clone()).size(12).style(tinted(palette::DANGER)))
            }
        };
        card("TRIM", panel)
    }

    /// Play / show in folder / delete, with the delete confirm inline (no OS dialog).
    fn actions(&self) -> Element<'_, Message> {
        if self.confirm_delete {
            return row![
                text("Delete this clip from disk?")
                    .size(12)
                    .style(tinted(palette::TEXT)),
                button(text("Yes, delete").size(11).font(UI_SEMIBOLD))
                    .on_press(Message::DeleteConfirmed)
                    .style(theme::danger_button)
                    .padding([7, 14]),
                button(text("Keep it").size(11).font(UI_SEMIBOLD))
                    .on_press(Message::DeleteCancelled)
                    .style(link_button)
                    .padding([7, 4]),
            ]
            .spacing(10)
            .align_y(iced::Alignment::Center)
            .into();
        }
        row![
            button(text("Open in player").size(11).font(UI_SEMIBOLD))
                .on_press(Message::Play)
                .style(secondary_button)
                .padding([9, 14]),
            button(text("Show in folder").size(11).font(UI_SEMIBOLD))
                .on_press(Message::ShowInFolder)
                .style(secondary_button)
                .padding([9, 14]),
            button(text("Delete").size(11).font(UI_SEMIBOLD))
                .on_press(Message::DeleteRequested)
                .style(theme::danger_outline_button)
                .padding([9, 14]),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center)
        .into()
    }

    /// The upload panel: title, destination, visibility, and the upload state line.
    fn upload_panel<'a>(
        &'a self,
        entry: &'a ClipEntry,
        ganked: DestStatus,
        youtube: DestStatus,
    ) -> Element<'a, Message> {
        let busy = matches!(
            self.upload,
            UploadState::Uploading { .. }
                | UploadState::Processing { .. }
                | UploadState::Checking { .. }
        );

        let title = column![
            field_label("Title"),
            text_input(&self.title_hint, &self.title)
                .on_input(Message::TitleEdited)
                .style(theme::arena_input),
        ]
        .spacing(6);

        let seg = |label: &'static str, dest: Dest, ready: bool| {
            let active = self.dest == dest;
            let (accent, ink) = if active {
                self.current_accent()
            } else {
                dest_accent(dest)
            };
            let b = button(
                container(text(label).size(11).font(UI_SEMIBOLD))
                    .center_x(Length::Fill)
                    .padding([2, 0]),
            )
            .style(move |theme: &Theme, status| segment_style(theme, status, active, accent, ink))
            .width(Length::Fill)
            .padding([5, 15]);
            if ready && !busy {
                b.on_press(Message::DestPicked(dest))
            } else {
                b
            }
        };
        let dest_control = container(
            row![
                seg("ganked.tv", Dest::Ganked, ganked.ready()),
                seg("YouTube", Dest::YouTube, youtube.ready()),
            ]
            .spacing(2),
        )
        .padding(3)
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(palette::HIGH)),
            border: Border {
                color: palette::BORDER,
                width: 1.0,
                radius: 8.0.into(),
            },
            ..container::Style::default()
        });
        let mut destination = column![field_label("Destination"), dest_control].spacing(6);
        if let Some(reason) = ganked.blocker(Dest::Ganked) {
            destination = destination.push(hint(reason));
        }
        if let Some(reason) = youtube.blocker(Dest::YouTube) {
            destination = destination.push(hint(reason));
        }

        let mut visibility = column![
            field_label("Visibility"),
            pick_list(
                &Visibility::ALL[..],
                Some(self.visibility),
                Message::VisibilityPicked,
            )
            .style(theme::arena_pick)
            .width(Length::Fill),
        ]
        .spacing(6);
        if self.dest == Dest::Ganked && self.visibility == Visibility::Private {
            visibility = visibility.push(hint(
                "Private needs a ganked.tv server that supports it; \
                 the clip may come back unlisted.",
            ));
        }

        let dest_ready = match self.dest {
            Dest::Ganked => ganked.ready(),
            Dest::YouTube => youtube.ready(),
        };
        // Once a clip is on a destination, the primary action becomes "Upload again", which
        // verifies the remote copy still exists before it lets a duplicate through.
        let already_uploaded = self.record_for(entry, self.dest).is_some();
        // The action button takes the destination's brand accent: mint for ganked.tv, red for
        // YouTube, so the whole panel reads as "this clip is headed to YouTube".
        let (accent, ink) = self.current_accent();
        // Hover tracks the destination target (a hover mid-fade is rare and cosmetic).
        let accent_hover = match self.dest {
            Dest::Ganked => palette::ACCENT_HOVER,
            Dest::YouTube => palette::YOUTUBE_HOVER,
        };
        let mut send = button(
            text(if already_uploaded {
                "Upload again".to_owned()
            } else {
                format!("Upload to {}", self.dest.label())
            })
            .size(13)
            .font(UI_BOLD),
        )
        .style(move |_theme: &Theme, status| accent_button_style(status, accent, accent_hover, ink))
        .padding([11, 24]);
        if dest_ready && !busy {
            send = send.on_press(if already_uploaded {
                Message::UploadAgain
            } else {
                Message::UploadPressed
            });
        }

        let mut panel = column![title, destination, visibility].spacing(16);
        panel = panel.push(container(send).center_x(Length::Fill));
        panel = panel.push(self.upload_status(entry, accent));
        theme::card_accent("UPLOAD", accent, panel)
    }

    /// The status line under the upload button, for whatever upload concerns this clip. `accent`
    /// is the destination brand colour (mint/red, or the fading value), so success lines and
    /// share links match the rest of the panel.
    fn upload_status(&self, entry: &ClipEntry, accent: iced::Color) -> Element<'_, Message> {
        match &self.upload {
            UploadState::Uploading { path, dest, .. } if *path == entry.path => {
                cancellable(format!("Uploading to {}...", dest.label()))
            }
            UploadState::Processing { path, .. } if *path == entry.path => {
                cancellable("Processing on ganked.tv...".to_owned())
            }
            UploadState::Checking { path, dest, .. } if *path == entry.path => hint(format!(
                "Checking whether the clip is still on {}...",
                dest.label()
            )),
            UploadState::Blocked { path, dest, link } if *path == entry.path => {
                let mut col = column![
                    text(format!("It's still on {}.", dest.label()))
                        .size(12)
                        .style(tinted(palette::TEXT_SECONDARY))
                ]
                .spacing(8);
                if let Some(url) = link {
                    col = col.push(link_actions(url, accent));
                }
                col.push(anyway_button("Upload another copy")).into()
            }
            UploadState::Unverifiable { path, reason, .. } if *path == entry.path => column![
                text(reason.clone())
                    .size(12)
                    .style(tinted(palette::TEXT_SECONDARY)),
                anyway_button("Upload anyway"),
            ]
            .spacing(8)
            .into(),
            UploadState::Done {
                path,
                dest,
                link,
                note,
            } if *path == entry.path => {
                let mut line = row![
                    text(format!("Uploaded to {}.", dest.label()))
                        .size(12)
                        .style(tinted(accent))
                ]
                .spacing(12)
                .align_y(iced::Alignment::Center);
                line = match link {
                    Some(url) => line.push(link_actions(url, accent)),
                    None => line.push(
                        text(note.clone())
                            .size(12)
                            .style(tinted(palette::TEXT_SECONDARY)),
                    ),
                };
                line.into()
            }
            UploadState::Failed { path, error } if *path == entry.path => text(error.clone())
                .size(12)
                .style(tinted(palette::DANGER))
                .into(),
            UploadState::Uploading { .. }
            | UploadState::Processing { .. }
            | UploadState::Checking { .. } => {
                hint("Another clip is uploading; one upload runs at a time.")
            }
            // Idle for this clip: if it is already on the chosen destination, show where it landed.
            _ => match self.record_for(entry, self.dest) {
                Some(record) => {
                    let mut line = row![
                        text(format!("Already on {}.", self.dest.label()))
                            .size(12)
                            .style(tinted(accent))
                    ]
                    .spacing(12)
                    .align_y(iced::Alignment::Center);
                    if let Some(url) = &record.url {
                        line = line.push(link_actions(url, accent));
                    }
                    line.into()
                }
                None => Space::new().into(),
            },
        }
    }
}

/// A share/watch link with Open + Copy-link buttons, in the destination's brand accent.
fn link_actions(url: &str, accent: iced::Color) -> Element<'static, Message> {
    row![
        text(url.to_owned())
            .size(12)
            .font(UI_SEMIBOLD)
            .style(tinted(accent)),
        button(text("Open").size(11).font(UI_SEMIBOLD))
            .on_press(Message::OpenLink(url.to_owned()))
            .style(secondary_button)
            .padding([6, 12]),
        button(text("Copy link").size(11).font(UI_SEMIBOLD))
            .on_press(Message::CopyLink(url.to_owned()))
            .style(secondary_button)
            .padding([6, 12]),
    ]
    .spacing(12)
    .align_y(iced::Alignment::Center)
    .into()
}

/// A status line with a Cancel button, for the in-flight upload/processing states.
fn cancellable(message: String) -> Element<'static, Message> {
    row![
        text(message)
            .size(12)
            .style(tinted(palette::TEXT_SECONDARY)),
        button(text("Cancel").size(11).font(UI_SEMIBOLD))
            .on_press(Message::UploadCancelled)
            .style(secondary_button)
            .padding([6, 14]),
    ]
    .spacing(12)
    .align_y(iced::Alignment::Center)
    .into()
}

/// The "upload anyway / another copy" escape for the duplicate guard.
fn anyway_button(label: &'static str) -> Element<'static, Message> {
    button(text(label).size(11).font(UI_SEMIBOLD))
        .on_press(Message::UploadAnyway)
        .style(secondary_button)
        .padding([6, 14])
        .into()
}

/// Whether one upload destination can actually receive a clip. Being logged in (an API key /
/// refresh token is present) is the whole bar now: login turns uploads on by itself, and clips
/// are sent per-clip from here, so there is no separate enable switch to satisfy.
#[derive(Debug, Clone, Copy)]
struct DestStatus {
    logged_in: bool,
}

impl DestStatus {
    fn ready(self) -> bool {
        self.logged_in
    }

    /// Why `dest` can't be uploaded to right now, in words the user can act on.
    fn blocker(self, dest: Dest) -> Option<String> {
        if self.logged_in {
            return None;
        }
        Some(match dest {
            Dest::Ganked => "Log in to ganked.tv under Settings first.".to_owned(),
            Dest::YouTube => "Log in with YouTube under Settings first.".to_owned(),
        })
    }
}

/// Both destinations' status, from the same validated config the uploads read.
fn dest_statuses(config: &Config) -> (DestStatus, DestStatus) {
    let up = config.upload();
    let yt = config.youtube();
    (
        DestStatus {
            logged_in: !up.api_key.is_empty(),
        },
        DestStatus {
            logged_in: !yt.refresh_token.is_empty(),
        },
    )
}

/// Upload to ganked.tv with the same flow (and outcome wording) as the tray.
async fn upload_ganked(
    up: rewynd_config::UploadSettings,
    path: PathBuf,
    title: String,
    visibility: Visibility,
) -> Result<Uploaded, String> {
    let client = GankedClient::new(&up.api_url, &up.api_key).map_err(|e| e.to_string())?;
    let clip = client
        .upload(&path, &title, visibility)
        .await
        .map_err(|e| {
            // The user-facing copy is shared with the tray; the full error goes to the log.
            tracing::error!(error = %e, "upload failed");
            user_facing_upload_error(&e)
        })?;
    if clip.failed() {
        return Err(
            "ganked.tv could not process the clip (check its length and format).".to_owned(),
        );
    }
    let link = clip.share_url(&up.share_url);
    Ok(Uploaded {
        remote_id: clip.id,
        link,
        note: "Processing on ganked.tv.".to_owned(),
    })
}

/// Poll ganked.tv processing status until it is ready or failed (or the budget runs out), so the
/// user learns of a server-side failure that only surfaces after "processing".
async fn poll_ganked(
    up: rewynd_config::UploadSettings,
    path: PathBuf,
    clip_id: String,
) -> (PathBuf, Result<StatusOutcome, String>) {
    let outcome = async {
        let client = GankedClient::new(&up.api_url, &up.api_key).map_err(|e| e.to_string())?;
        let report = client
            .poll_status(&clip_id, POLL_INTERVAL, POLL_MAX_READS)
            .await
            .map_err(|e| user_facing_upload_error(&e))?;
        let message = if report.failed() {
            report.failure_message()
        } else if report.is_ready() {
            "Live on ganked.tv.".to_owned()
        } else {
            "Still processing on ganked.tv.".to_owned()
        };
        Ok(StatusOutcome {
            failed: report.failed(),
            link: report.share_url(&up.share_url),
            message,
        })
    }
    .await;
    (path, outcome)
}

/// Whether a ganked.tv clip still exists (for the duplicate guard). A 404 → gone (re-upload
/// allowed); any other error is a verification failure the caller surfaces as "upload anyway".
async fn verify_ganked(
    up: rewynd_config::UploadSettings,
    path: PathBuf,
    clip_id: String,
) -> (PathBuf, Result<bool, String>) {
    let exists = async {
        let client = GankedClient::new(&up.api_url, &up.api_key).map_err(|e| e.to_string())?;
        client
            .clip_exists(&clip_id)
            .await
            .map_err(|e| user_facing_upload_error(&e))
    }
    .await;
    (path, exists)
}

/// Upload to YouTube with the same flow (and outcome wording) as the tray.
async fn upload_youtube(
    yt: rewynd_config::YouTubeSettings,
    path: PathBuf,
    title: String,
    visibility: Visibility,
) -> Result<Uploaded, String> {
    let client = YouTubeClient::new(
        rewynd_config::non_empty_or(&yt.client_id, DEFAULT_CLIENT_ID),
        rewynd_config::non_empty_or(&yt.client_secret, DEFAULT_CLIENT_SECRET),
        &yt.refresh_token,
    )
    .map_err(|e| e.to_string())?;
    let video = client
        .upload(&path, &title, visibility)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "YouTube upload failed");
            user_facing_youtube_error(&e)
        })?;
    Ok(Uploaded {
        remote_id: video.id.clone(),
        link: video.watch_url(),
        note: "Uploaded to YouTube.".to_owned(),
    })
}

/// The saved-at instant in local time, for card titles.
fn saved_at_label(t: SystemTime) -> String {
    jiff::Timestamp::try_from(t).map_or_else(
        |_| "Unknown time".to_owned(),
        |ts| {
            ts.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        },
    )
}

fn size_label(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

/// A whole-library size in whichever of MB / GB reads cleanest.
fn disk_label(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.0} MB", bytes as f64 / 1e6)
    }
}

fn duration_label(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

/// The section label for a clip: its game subfolder, or [`ROOT_GROUP`] for a root clip.
fn group_label(entry: &ClipEntry) -> &str {
    entry.game.as_deref().unwrap_or(ROOT_GROUP)
}

/// The upload-history identity key for a clip entry.
fn clip_key(entry: &ClipEntry) -> Option<ClipKey> {
    ClipKey::new(&entry.path, entry.size_bytes, entry.modified)
}

/// Case-insensitive subsequence match: every character of `needle` appears in `haystack` in
/// order (so "eldn" matches "Elden Ring"). An empty needle matches everything.
fn fuzzy_match(needle: &str, haystack: &str) -> bool {
    let mut hay = haystack.chars().flat_map(char::to_lowercase);
    'needle: for nc in needle.chars().flat_map(char::to_lowercase) {
        for hc in hay.by_ref() {
            if hc == nc {
                continue 'needle;
            }
        }
        return false;
    }
    true
}

/// A filter chip: mint fill when active, a quiet outline otherwise. The label text is owned, so
/// the chip does not borrow the caller's string.
fn chip(label: &str, active: bool, msg: Message) -> Element<'static, Message> {
    button(text(label.to_uppercase()).size(10).font(UI_BOLD))
        .on_press(msg)
        .style(move |_: &Theme, status| chip_style(status, active))
        .padding([5, 11])
        .into()
}

fn chip_style(status: iced::widget::button::Status, active: bool) -> iced::widget::button::Style {
    use iced::widget::button::{Status, Style};
    let (background, text_color, border_color) = if active {
        (
            Some(Background::Color(palette::ACCENT)),
            palette::INK_ON_ACCENT,
            palette::ACCENT,
        )
    } else {
        match status {
            Status::Hovered | Status::Pressed => (None, palette::ACCENT, palette::ACCENT_BORDER),
            _ => (None, palette::TEXT_SECONDARY, palette::BORDER),
        }
    };
    Style {
        background,
        text_color,
        border: Border {
            color: border_color,
            width: 1.0,
            radius: 12.0.into(),
        },
        ..Style::default()
    }
}

/// The clip card shell: a raised panel that is also a button (hover lifts the border to the
/// accent tint, the design's one sanctioned hover cue).
fn clip_card_style(
    _theme: &Theme,
    status: iced::widget::button::Status,
) -> iced::widget::button::Style {
    use iced::widget::button::{Status, Style};
    let border_color = match status {
        Status::Hovered | Status::Pressed => palette::ACCENT_BORDER,
        _ => palette::BORDER,
    };
    Style {
        background: Some(Background::Color(palette::PANEL)),
        text_color: palette::TEXT,
        border: Border {
            color: border_color,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..Style::default()
    }
}

/// One segment of the destination control: mint fill + ink when active, quiet otherwise.
/// A `m:ss` label for a number of seconds.
fn secs_label(seconds: f32) -> String {
    let s = seconds.max(0.0).round() as u32;
    format!("{}:{:02}", s / 60, s % 60)
}

/// A fresh sibling path for a trimmed copy of `src` (`<stem>-trim.<ext>`, bumping a counter when
/// taken), so re-trimming never overwrites an earlier trim and the copy lands beside the original
/// (same per-game folder).
fn unique_trim_path(src: &Path) -> PathBuf {
    let parent = src.parent().unwrap_or_else(|| Path::new("."));
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("clip");
    let ext = src.extension().and_then(|s| s.to_str()).unwrap_or("mp4");
    let mut candidate = parent.join(format!("{stem}-trim.{ext}"));
    let mut n = 2;
    while candidate.exists() {
        candidate = parent.join(format!("{stem}-trim-{n}.{ext}"));
        n += 1;
    }
    candidate
}

/// A sibling temp path in `src`'s directory (so the replace is a same-filesystem, atomic rename).
fn overwrite_temp_path(src: &Path) -> PathBuf {
    let name = src.file_name().and_then(|n| n.to_str()).unwrap_or("clip");
    src.with_file_name(format!(".{name}.trimming.{}.tmp", std::process::id()))
}

/// Trim `src` to `[start, end]`. [`SaveMode::Copy`] writes a fresh sibling file and returns it;
/// [`SaveMode::Overwrite`] writes a temp then atomically renames it over `src` (leaving the
/// original untouched if anything fails), returning `src`. Blocking.
fn save_trim(
    src: &Path,
    mode: SaveMode,
    start: Duration,
    end: Duration,
) -> Result<PathBuf, String> {
    match mode {
        SaveMode::Copy => {
            let dst = unique_trim_path(src);
            rewynd_mux::read::trim_clip(src, &dst, start, end)
                .map(|_| dst)
                .map_err(|e| e.to_string())
        }
        SaveMode::Overwrite => {
            let temp = overwrite_temp_path(src);
            let result = rewynd_mux::read::trim_clip(src, &temp, start, end)
                .map_err(|e| e.to_string())
                .and_then(|_| std::fs::rename(&temp, src).map_err(|e| e.to_string()))
                .map(|()| src.to_path_buf());
            if result.is_err() {
                let _ = std::fs::remove_file(&temp);
            }
            result
        }
    }
}

/// `n` evenly spaced, centred sample positions in `0.0..1.0` (cell i samples its own midpoint),
/// so the strip skips the very first and last frames.
fn filmstrip_positions(n: usize) -> Vec<f32> {
    (0..n).map(|i| (i as f32 + 0.5) / n as f32).collect()
}

/// How long the upload-panel accent takes to fade when the destination switches.
const ACCENT_FADE: Duration = Duration::from_millis(220);

/// An in-flight accent fade between two (fill, ink) brand pairs. `start` is anchored on the
/// first tick, so all time comes from the frame subscription rather than `Instant::now()` in
/// update.
struct AccentFade {
    from: (iced::Color, iced::Color),
    to: (iced::Color, iced::Color),
    start: Option<Instant>,
    progress: f32,
}

impl AccentFade {
    /// The interpolated (fill, ink) at the current progress.
    fn accent(&self) -> (iced::Color, iced::Color) {
        (
            lerp_color(self.from.0, self.to.0, self.progress),
            lerp_color(self.from.1, self.to.1, self.progress),
        )
    }

    /// Advance to frame time `now`, anchoring the clock on the first call. Returns `true` once
    /// the fade has reached its end (the caller then drops it).
    fn advance(&mut self, now: Instant) -> bool {
        let start = *self.start.get_or_insert(now);
        let linear = now.duration_since(start).as_secs_f32() / ACCENT_FADE.as_secs_f32();
        self.progress = ease(linear);
        linear >= 1.0
    }
}

/// The brand accent and its ink for an upload destination: mint for ganked.tv, red for YouTube.
fn dest_accent(dest: Dest) -> (iced::Color, iced::Color) {
    match dest {
        Dest::Ganked => (palette::ACCENT, palette::INK_ON_ACCENT),
        Dest::YouTube => (palette::YOUTUBE, palette::INK_ON_YOUTUBE),
    }
}

fn segment_style(
    _theme: &Theme,
    status: iced::widget::button::Status,
    active: bool,
    accent: iced::Color,
    ink: iced::Color,
) -> iced::widget::button::Style {
    use iced::widget::button::{Status, Style};
    let (background, text_color) = if active {
        (Some(Background::Color(accent)), ink)
    } else {
        match status {
            Status::Hovered | Status::Pressed => (None, accent),
            Status::Disabled => (None, palette::MUTED),
            _ => (None, palette::TEXT_SECONDARY),
        }
    };
    Style {
        background,
        text_color,
        border: Border {
            radius: 6.0.into(),
            ..Border::default()
        },
        ..Style::default()
    }
}

/// A stack whose real content sits above an empty base layer. iced hard-clips only *layered*
/// stack children (the base draws unlayered, and `container.clip` merely narrows a viewport
/// hint that image drawing ignores), so this is what actually keeps Cover-scaled frames inside
/// the box of the nearest `clip(true)` ancestor.
fn layered<'a>(
    content: impl IntoIterator<Item = Element<'a, Message>>,
) -> iced::widget::Stack<'a, Message> {
    content.into_iter().fold(
        iced::widget::Stack::new().push(Space::new()),
        |stack, layer| stack.push(layer),
    )
}

/// The brand mark as a bare play control (it is a play button), centred over the preview.
fn logo_play_button<'a>(size: f32) -> Element<'a, Message> {
    button(theme::logo(size))
        .on_press(Message::PlayToggle)
        .style(|_: &Theme, _| iced::widget::button::Style::default())
        .padding(0)
        .into()
}

/// A decoded frame styled like every clip image: full width, covering `height`. Callers wrap it
/// in [`layered`] + a `clip(true)` container (Cover overflows the box on mismatched aspects).
fn frame_image<'a>(handle: iced::widget::image::Handle, height: f32) -> Element<'a, Message> {
    iced::widget::image(handle)
        .width(Length::Fill)
        .height(height)
        .content_fit(iced::ContentFit::Cover)
        .into()
}

/// A neutral thumbnail placeholder: a surface-high well with a muted caption.
fn placeholder<'a>(label: &'a str, height: f32) -> Element<'a, Message> {
    container(text(label).size(11).style(tinted(palette::MUTED)))
        .width(Length::Fill)
        .height(height)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(palette::HIGH)),
            ..container::Style::default()
        })
        .into()
}

#[cfg(test)]
mod filmstrip_tests {
    use super::*;

    #[test]
    fn positions_are_centred_and_ordered() {
        let p = filmstrip_positions(4);
        assert_eq!(p.len(), 4);
        assert!((p[0] - 0.125).abs() < 1e-6);
        assert!((p[3] - 0.875).abs() < 1e-6);
        assert!(p.windows(2).all(|w| w[0] < w[1]), "strictly increasing");
        assert!(p.iter().all(|&x| x > 0.0 && x < 1.0), "inside (0,1)");
        assert!(filmstrip_positions(0).is_empty());
    }
}

#[cfg(test)]
mod accent_tests {
    use super::*;
    use iced::Color;
    use std::time::{Duration, Instant};

    fn approx(a: Color, b: Color) {
        for (x, y) in [(a.r, b.r), (a.g, b.g), (a.b, b.b), (a.a, b.a)] {
            assert!((x - y).abs() < 1e-4, "{x} vs {y}");
        }
    }

    #[test]
    fn lerp_color_hits_both_ends() {
        let a = Color::from_rgb(0.0, 0.0, 0.0);
        let b = Color::from_rgb(1.0, 0.5, 0.25);
        approx(lerp_color(a, b, 0.0), a);
        approx(lerp_color(a, b, 1.0), b);
        approx(lerp_color(a, b, 0.5), Color::from_rgb(0.5, 0.25, 0.125));
    }

    #[test]
    fn ease_is_clamped_and_smooth() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        assert_eq!(ease(-1.0), 0.0);
        assert_eq!(ease(2.0), 1.0);
        assert!((ease(0.5) - 0.5).abs() < 1e-6, "symmetric midpoint");
    }

    #[test]
    fn fade_runs_from_source_to_target_then_ends() {
        let from = (palette::ACCENT, palette::INK_ON_ACCENT);
        let to = (palette::YOUTUBE, palette::INK_ON_YOUTUBE);
        let mut fade = AccentFade {
            from,
            to,
            start: None,
            progress: 0.0,
        };
        let t0 = Instant::now();

        // First tick anchors the clock; still fully at `from`.
        assert!(!fade.advance(t0));
        approx(fade.accent().0, from.0);

        // Partway through: strictly between the endpoints.
        assert!(!fade.advance(t0 + Duration::from_millis(90)));
        let mid = fade.accent().0;
        assert!(mid.r > from.0.r && mid.r < to.0.r + 1e-3);

        // At/after the duration: reports done and sits on the target.
        assert!(fade.advance(t0 + ACCENT_FADE));
        approx(fade.accent().0, to.0);
    }

    #[test]
    fn advance_fade_clears_when_complete() {
        let mut fade = AccentFade {
            from: (palette::ACCENT, palette::INK_ON_ACCENT),
            to: (palette::YOUTUBE, palette::INK_ON_YOUTUBE),
            start: None,
            progress: 0.0,
        };
        let t0 = Instant::now();
        assert!(!fade.advance(t0));
        assert!(fade.advance(t0 + ACCENT_FADE + Duration::from_millis(1)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_and_duration_labels() {
        assert_eq!(size_label(123_456_789), "123.5 MB");
        assert_eq!(size_label(900_000), "0.9 MB");
        assert_eq!(duration_label(Duration::from_secs(30)), "0:30");
        assert_eq!(duration_label(Duration::from_secs(65)), "1:05");
        assert_eq!(duration_label(Duration::from_secs(600)), "10:00");
    }

    #[test]
    fn dest_status_gates_on_login() {
        let ready = DestStatus { logged_in: true };
        assert!(ready.ready());
        assert_eq!(ready.blocker(Dest::Ganked), None);
        let logged_out = DestStatus { logged_in: false };
        assert!(!logged_out.ready());
        let blocker = logged_out.blocker(Dest::YouTube).expect("blocked");
        assert!(blocker.contains("Log in"), "{blocker}");
    }

    #[test]
    fn fuzzy_match_is_a_case_insensitive_subsequence() {
        assert!(fuzzy_match("eldn", "Elden Ring"));
        assert!(fuzzy_match("OW", "Overwatch"));
        assert!(fuzzy_match("", "anything"));
        assert!(fuzzy_match("desktop", "Desktop"));
        assert!(!fuzzy_match("zzz", "Overwatch"));
        assert!(
            !fuzzy_match("ringx", "Elden Ring"),
            "extra char past the end"
        );
    }

    #[test]
    fn disk_label_scales_mb_to_gb() {
        assert_eq!(disk_label(500_000_000), "500 MB");
        assert_eq!(disk_label(0), "0 MB");
        assert_eq!(disk_label(1_500_000_000), "1.5 GB");
        assert_eq!(disk_label(12_300_000_000), "12.3 GB");
    }

    #[test]
    fn group_label_falls_back_to_the_root_section() {
        let with_game = ClipEntry {
            path: PathBuf::from("/c/Elden Ring/rewynd-1-0.mp4"),
            game: Some("Elden Ring".to_owned()),
            saved_at: SystemTime::UNIX_EPOCH,
            modified: SystemTime::UNIX_EPOCH,
            size_bytes: 1,
        };
        let rootless = ClipEntry {
            game: None,
            ..with_game.clone()
        };
        assert_eq!(group_label(&with_game), "Elden Ring");
        assert_eq!(group_label(&rootless), ROOT_GROUP);
    }

    #[test]
    fn saved_at_label_formats_local_time() {
        let label = saved_at_label(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        assert_eq!(label.len(), 16, "{label}");
        assert!(label.starts_with("2023-11-1"), "{label}");
    }
}
