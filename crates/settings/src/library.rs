//! The clip library: every saved clip as a thumbnail card, and per clip a detail page with
//! play / show-in-folder / delete and an upload flow (title, destination, visibility) that
//! reuses the transport clients the tray uses.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use iced::widget::{
    Space, button, column, container, pick_list, row, scrollable, text, text_input,
};
use iced::{Background, Border, Element, Length, Task, Theme};

use rewynd_config::{ClipEntry, Config};
use rewynd_upload::youtube::{
    DEFAULT_CLIENT_ID, DEFAULT_CLIENT_SECRET, YouTubeClient, user_facing_youtube_error,
};
use rewynd_upload::{GankedClient, Visibility, default_title, user_facing_upload_error};

use crate::theme::{
    self, DISPLAY_BLACK, UI_BOLD, UI_SEMIBOLD, accent_chip, card, field_label, hint, link_button,
    palette, primary_button, secondary_button, tinted,
};
use crate::thumbs;

/// Cards per grid row (the body column is width-capped, so a fixed count stays balanced).
const GRID_COLUMNS: usize = 3;

/// Thumbnail decodes running at once. Each holds a full decoded frame briefly, so a big
/// library must stream through a small pool instead of decoding every stale clip at once.
const MAX_DECODES: usize = 4;

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
    Done {
        path: PathBuf,
        link: Option<String>,
        note: String,
    },
    Failed {
        path: PathBuf,
        error: String,
    },
}

/// A finished upload: the share/watch link when the server issued one, else a note.
#[derive(Debug, Clone)]
pub struct Uploaded {
    link: Option<String>,
    note: String,
}

#[derive(Debug, Clone)]
pub enum Message {
    Refresh,
    Scanned(Vec<ClipEntry>),
    ThumbDone(PathBuf, SystemTime, Result<thumbs::Loaded, String>),
    Open(PathBuf),
    Back,
    Play,
    ShowInFolder,
    DeleteRequested,
    DeleteCancelled,
    DeleteConfirmed,
    Deleted(Result<PathBuf, String>),
    TitleEdited(String),
    DestPicked(Dest),
    VisibilityPicked(Visibility),
    UploadPressed,
    UploadCancelled,
    UploadDone(Result<Uploaded, String>),
    OpenLink(String),
    CopyLink(String),
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
    /// The clip whose detail page is open, if any.
    open: Option<PathBuf>,
    confirm_delete: bool,
    /// Play / show-in-folder / delete failure for the open clip.
    action_error: Option<String>,
    title: String,
    /// The suggested title, snapshotted when the detail page opens (recomputing it per
    /// `view()` would make the placeholder's minute stamp tick while the page sits open).
    title_hint: String,
    dest: Dest,
    visibility: Visibility,
    upload: UploadState,
}

impl Library {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            thumbs: HashMap::new(),
            pending_thumbs: VecDeque::new(),
            decoding: HashMap::new(),
            scanning: false,
            open: None,
            confirm_delete: false,
            action_error: None,
            title: String::new(),
            title_hint: String::new(),
            dest: Dest::Ganked,
            visibility: Visibility::default(),
            upload: UploadState::Idle,
        }
    }

    /// Rescan the clip directory (the same resolution the recorder saves through) on a
    /// blocking task. Called on view-enter and by the Refresh button.
    pub fn refresh(&mut self, config: &Config) -> Task<Message> {
        self.scanning = true;
        let dir = rewynd_config::clips_dir(config.output_dir().as_deref());
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || rewynd_config::list_clips(&dir))
                    .await
                    .unwrap_or_default()
            },
            Message::Scanned,
        )
    }

    pub fn update(&mut self, message: Message, config: &Config) -> Task<Message> {
        match message {
            Message::Refresh => return self.refresh(config),
            Message::Scanned(entries) => return self.scanned(entries),
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
                self.open = Some(path);
                self.confirm_delete = false;
                self.action_error = None;
                self.title_hint = default_title();
                self.title = self.title_hint.clone();
                let (ganked, youtube) = dest_statuses(config);
                self.dest = if !ganked.ready() && youtube.ready() {
                    Dest::YouTube
                } else {
                    Dest::Ganked
                };
                self.visibility = self.default_visibility(config);
            }
            Message::Back => {
                self.open = None;
                self.confirm_delete = false;
                self.action_error = None;
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
                }
                self.action_error = None;
            }
            Message::Deleted(Err(e)) => {
                self.action_error = Some(format!("Could not delete the clip: {e}"));
            }
            Message::TitleEdited(s) => self.title = s,
            Message::DestPicked(dest) => {
                if self.dest != dest {
                    self.dest = dest;
                    self.visibility = self.default_visibility(config);
                }
            }
            Message::VisibilityPicked(v) => self.visibility = v,
            Message::UploadPressed => return self.start_upload(config),
            Message::UploadCancelled => {
                if let UploadState::Uploading { abort, .. } =
                    std::mem::replace(&mut self.upload, UploadState::Idle)
                {
                    abort.abort();
                }
            }
            Message::UploadDone(result) => {
                let UploadState::Uploading { path, .. } =
                    std::mem::replace(&mut self.upload, UploadState::Idle)
                else {
                    return Task::none();
                };
                self.upload = match result {
                    Ok(done) => UploadState::Done {
                        path,
                        link: done.link,
                        note: done.note,
                    },
                    Err(error) => UploadState::Failed { path, error },
                };
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
    fn scanned(&mut self, entries: Vec<ClipEntry>) -> Task<Message> {
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

    pub fn view(&self, config: &Config) -> Element<'_, Message> {
        let body: Element<Message> = match self.open.as_ref().and_then(|p| self.entry(p)) {
            Some(entry) => self.detail(entry, dest_statuses(config)),
            None => self.grid(),
        };
        let content = container(
            column![body]
                .spacing(20)
                .padding(28)
                .max_width(880)
                .width(Length::Fill),
        )
        .center_x(Length::Fill);
        container(scrollable(content))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn entry(&self, path: &Path) -> Option<&ClipEntry> {
        self.entries.iter().find(|e| e.path == path)
    }

    /// The default page: header + the clip cards, newest first.
    fn grid(&self) -> Element<'_, Message> {
        let header = row![
            text("LIBRARY").size(32).font(DISPLAY_BLACK),
            Space::new().width(Length::Fill),
            if self.scanning {
                hint("Refreshing...")
            } else {
                Space::new().into()
            },
            button(text("Refresh").size(12).font(UI_SEMIBOLD))
                .on_press(Message::Refresh)
                .style(secondary_button)
                .padding([8, 16]),
        ]
        .spacing(12)
        .align_y(iced::Alignment::Center);

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

        let mut rows = column![].spacing(14);
        for chunk in self.entries.chunks(GRID_COLUMNS) {
            let mut r = row![].spacing(14);
            for entry in chunk {
                r = r.push(self.clip_card(entry));
            }
            // Pad the last row so its cards keep the grid's card width.
            for _ in chunk.len()..GRID_COLUMNS {
                r = r.push(Space::new().width(Length::Fill));
            }
            rows = rows.push(r);
        }
        column![header, rows].spacing(20).into()
    }

    fn clip_card<'a>(&'a self, entry: &'a ClipEntry) -> Element<'a, Message> {
        let info = column![
            text(saved_at_label(entry.saved_at))
                .size(12)
                .font(UI_SEMIBOLD)
                .style(tinted(palette::TEXT)),
            self.meta_row(entry, 10.0, palette::MUTED),
        ]
        .spacing(7);

        button(
            column![
                self.thumbnail(entry, 148.0),
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

    /// The "duration · size" line with the per-game chip, shared by the cards and the
    /// detail page (which differ only in type size and tint).
    fn meta_row<'a>(
        &'a self,
        entry: &'a ClipEntry,
        size: f32,
        color: iced::Color,
    ) -> Element<'a, Message> {
        let mut meta = size_label(entry.size_bytes);
        if let Some(Thumb::Ready { duration, .. }) = self.thumbs.get(&entry.path) {
            meta = format!("{} · {meta}", duration_label(*duration));
        }
        let mut line = row![].spacing(8).align_y(iced::Alignment::Center);
        if let Some(game) = &entry.game {
            line = line.push(accent_chip(game.clone()));
        }
        line.push(text(meta).size(size).style(tinted(color))).into()
    }

    /// The clip's thumbnail at the given height, or a neutral placeholder while it loads
    /// (or when the clip can't be decoded).
    fn thumbnail<'a>(&'a self, entry: &'a ClipEntry, height: f32) -> Element<'a, Message> {
        match self.thumbs.get(&entry.path) {
            Some(Thumb::Ready { handle, .. }) => iced::widget::image(handle.clone())
                .width(Length::Fill)
                .height(height)
                .content_fit(iced::ContentFit::Cover)
                .into(),
            Some(Thumb::Failed { .. }) => placeholder("No preview", height),
            _ => placeholder("Loading...", height),
        }
    }

    /// The detail page for one clip: preview, facts, actions, and the upload panel.
    fn detail<'a>(
        &'a self,
        entry: &'a ClipEntry,
        (ganked, youtube): (DestStatus, DestStatus),
    ) -> Element<'a, Message> {
        let back = button(text("Back to library").size(12).font(UI_SEMIBOLD))
            .on_press(Message::Back)
            .style(link_button)
            .padding(0);

        let mut facts = column![
            text(saved_at_label(entry.saved_at).to_uppercase())
                .size(26)
                .font(DISPLAY_BLACK),
            self.meta_row(entry, 11.0, palette::TEXT_SECONDARY),
            hint(entry.path.display().to_string()),
        ]
        .spacing(10);
        facts = facts.push(self.actions());
        if let Some(e) = &self.action_error {
            facts = facts.push(text(e.clone()).size(12).style(tinted(palette::DANGER)));
        }

        let preview = container(self.thumbnail(entry, 240.0)).style(theme::card_style);
        let top = row![
            container(preview).width(Length::FillPortion(5)),
            container(facts).width(Length::FillPortion(4)),
        ]
        .spacing(20);

        column![back, top, self.upload_panel(entry, ganked, youtube),]
            .spacing(20)
            .into()
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
                    .style(secondary_button)
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
            button(text("Play").size(12).font(UI_BOLD))
                .on_press(Message::Play)
                .style(primary_button)
                .padding([9, 22]),
            button(text("Show in folder").size(11).font(UI_SEMIBOLD))
                .on_press(Message::ShowInFolder)
                .style(secondary_button)
                .padding([9, 14]),
            button(text("Delete").size(11).font(UI_SEMIBOLD))
                .on_press(Message::DeleteRequested)
                .style(secondary_button)
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
        let uploading = matches!(self.upload, UploadState::Uploading { .. });

        let title = column![
            field_label("Title"),
            text_input(&self.title_hint, &self.title)
                .on_input(Message::TitleEdited)
                .style(theme::arena_input),
        ]
        .spacing(6);

        let seg = |label: &'static str, dest: Dest, ready: bool| {
            let active = self.dest == dest;
            let b = button(
                container(text(label).size(11).font(UI_SEMIBOLD))
                    .center_x(Length::Fill)
                    .padding([2, 0]),
            )
            .style(move |theme: &Theme, status| segment_style(theme, status, active))
            .width(Length::Fill)
            .padding([5, 15]);
            if ready && !uploading {
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
        let mut send = button(
            text(format!("Upload to {}", self.dest.label()))
                .size(13)
                .font(UI_BOLD),
        )
        .style(primary_button)
        .padding([11, 24]);
        if dest_ready && !uploading {
            send = send.on_press(Message::UploadPressed);
        }

        let mut panel = column![title, destination, visibility].spacing(16);
        panel = panel.push(row![send].spacing(10).align_y(iced::Alignment::Center));
        panel = panel.push(self.upload_status(entry));
        card("UPLOAD", panel)
    }

    /// The status line under the upload button, for whatever upload concerns this clip.
    fn upload_status(&self, entry: &ClipEntry) -> Element<'_, Message> {
        match &self.upload {
            UploadState::Uploading { path, dest, .. } if *path == entry.path => row![
                text(format!("Uploading to {}...", dest.label()))
                    .size(12)
                    .style(tinted(palette::TEXT_SECONDARY)),
                button(text("Cancel").size(11).font(UI_SEMIBOLD))
                    .on_press(Message::UploadCancelled)
                    .style(secondary_button)
                    .padding([6, 14]),
            ]
            .spacing(12)
            .align_y(iced::Alignment::Center)
            .into(),
            UploadState::Uploading { .. } => {
                hint("Another clip is uploading; one upload runs at a time.")
            }
            UploadState::Done { path, link, note } if *path == entry.path => {
                let mut line = row![text("Uploaded.").size(12).style(tinted(palette::ACCENT)),]
                    .spacing(12)
                    .align_y(iced::Alignment::Center);
                if let Some(url) = link {
                    line = line
                        .push(
                            text(url.clone())
                                .size(12)
                                .font(UI_SEMIBOLD)
                                .style(tinted(palette::ACCENT)),
                        )
                        .push(
                            button(text("Open").size(11).font(UI_SEMIBOLD))
                                .on_press(Message::OpenLink(url.clone()))
                                .style(secondary_button)
                                .padding([6, 12]),
                        )
                        .push(
                            button(text("Copy link").size(11).font(UI_SEMIBOLD))
                                .on_press(Message::CopyLink(url.clone()))
                                .style(secondary_button)
                                .padding([6, 12]),
                        );
                } else {
                    line = line.push(
                        text(note.clone())
                            .size(12)
                            .style(tinted(palette::TEXT_SECONDARY)),
                    );
                }
                line.into()
            }
            UploadState::Failed { path, error } if *path == entry.path => text(error.clone())
                .size(12)
                .style(tinted(palette::DANGER))
                .into(),
            _ => Space::new().into(),
        }
    }
}

/// Whether one upload destination can actually receive a clip: logged in (key / refresh
/// token present) AND switched on in the config — the same bar the tray applies before it
/// builds an uploader.
#[derive(Debug, Clone, Copy)]
struct DestStatus {
    logged_in: bool,
    enabled: bool,
}

impl DestStatus {
    fn ready(self) -> bool {
        self.logged_in && self.enabled
    }

    /// Why `dest` can't be uploaded to right now, in words the user can act on.
    fn blocker(self, dest: Dest) -> Option<String> {
        if !self.logged_in {
            Some(match dest {
                Dest::Ganked => "Log in to ganked.tv under Settings first.".to_owned(),
                Dest::YouTube => "Log in with YouTube under Settings first.".to_owned(),
            })
        } else if !self.enabled {
            Some(format!(
                "{} uploads are switched off in Settings; enable them there first.",
                dest.label()
            ))
        } else {
            None
        }
    }
}

/// Both destinations' status, from the same validated config the tray uploads read.
fn dest_statuses(config: &Config) -> (DestStatus, DestStatus) {
    let up = config.upload();
    let yt = config.youtube();
    (
        DestStatus {
            logged_in: !up.api_key.is_empty(),
            enabled: up.enabled,
        },
        DestStatus {
            logged_in: !yt.refresh_token.is_empty(),
            enabled: yt.enabled,
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
    Ok(Uploaded {
        link: clip.share_url(&up.share_url),
        note: "Processing on ganked.tv.".to_owned(),
    })
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
        link: video.watch_url(),
        note: "Processing on YouTube.".to_owned(),
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

fn duration_label(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
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
fn segment_style(
    _theme: &Theme,
    status: iced::widget::button::Status,
    active: bool,
) -> iced::widget::button::Style {
    use iced::widget::button::{Status, Style};
    let (background, text_color) = if active {
        (
            Some(Background::Color(palette::ACCENT)),
            palette::INK_ON_ACCENT,
        )
    } else {
        match status {
            Status::Hovered | Status::Pressed => (None, palette::ACCENT),
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
    fn dest_status_gates_on_login_and_enabled() {
        let ready = DestStatus {
            logged_in: true,
            enabled: true,
        };
        assert!(ready.ready());
        assert_eq!(ready.blocker(Dest::Ganked), None);
        let logged_out = DestStatus {
            logged_in: false,
            enabled: false,
        };
        assert!(!logged_out.ready());
        let blocker = logged_out.blocker(Dest::YouTube).expect("blocked");
        assert!(blocker.contains("Log in"), "{blocker}");
        let disabled = DestStatus {
            logged_in: true,
            enabled: false,
        };
        assert!(!disabled.ready());
        let blocker = disabled.blocker(Dest::Ganked).expect("blocked");
        assert!(blocker.contains("switched off"), "{blocker}");
    }

    #[test]
    fn saved_at_label_formats_local_time() {
        let label = saved_at_label(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        assert_eq!(label.len(), 16, "{label}");
        assert!(label.starts_with("2023-11-1"), "{label}");
    }
}
