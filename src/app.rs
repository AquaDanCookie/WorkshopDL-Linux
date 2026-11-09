use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use egui::{
    Align, Align2, Button, Color32, ComboBox, CornerRadius, FontData, FontDefinitions, FontFamily,
    FontId, Frame, Label, Layout, Margin, ProgressBar, RichText, ScrollArea, Stroke, TextEdit,
    TextureHandle, TextureOptions, Ui, Vec2,
};
use image::AnimationDecoder;
use serde_json::Value;

use crate::desktop;
use crate::games;
use crate::ggntw::GgNetworkManager;
use crate::jobs::{Auth, Bridge, DownloadItem, Events, LoginResult, RunResult};
use crate::settings::Settings;
use crate::steamcmd::SteamCmdManager;
use crate::util;
use crate::workshop_api;

const BG: Color32 = Color32::from_rgb(0x1b, 0x28, 0x38);
const HEADER_BG: Color32 = Color32::from_rgb(0x17, 0x1a, 0x21);
const CARD_BG: Color32 = Color32::from_rgb(0x16, 0x20, 0x2d);
const CARD_ALT: Color32 = Color32::from_rgb(0x1f, 0x2d, 0x3d);
const INPUT_BG: Color32 = Color32::from_rgb(0x0e, 0x16, 0x20);
const BORDER: Color32 = Color32::from_rgb(0x2a, 0x47, 0x5e);
const TEXT: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
const MUTED: Color32 = Color32::from_rgb(0xa9, 0xb8, 0xc6);
const DIM: Color32 = Color32::from_rgb(0x74, 0x82, 0x91);
const ACCENT: Color32 = Color32::from_rgb(0xa4, 0xd0, 0x07);
const ACCENT_DARK: Color32 = Color32::from_rgb(0x12, 0x1b, 0x04);
const GREEN: Color32 = Color32::from_rgb(0xa4, 0xd0, 0x07);
const RED: Color32 = Color32::from_rgb(0xe0, 0x5a, 0x52);
const BLUE: Color32 = Color32::from_rgb(0x66, 0xc0, 0xf4);
const YELLOW: Color32 = Color32::from_rgb(0xd9, 0xb4, 0x5a);
const LOG_TEXT: Color32 = Color32::from_rgb(0xa9, 0xb8, 0xc6);
const PREVIEW_MAX: u32 = 320;
const MAX_LOG_LINES: usize = 4000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Single,
    Batch,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RunKind {
    Single,
    Batch,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResolvePurpose {
    Single,
    Queue,
}

enum ResolveJob {
    Item {
        purpose: ResolvePurpose,
        fileid: u64,
        appid: Option<u64>,
    },
    Titles {
        appid: Option<u64>,
        fileids: Vec<u64>,
    },
}

struct PreviewJob {
    fileid: u64,
    url: String,
}

struct Resolved {
    purpose: ResolvePurpose,
    fileid: u64,
    appid: Option<u64>,
    title: Option<String>,
    description: String,
    preview_url: String,
    is_collection: bool,
    children: Vec<u64>,
    resolve_error: Option<String>,
    detail_error: Option<String>,
}

struct ItemMeta {
    fileid: u64,
    appid: Option<u64>,
    title: Option<String>,
    description: String,
    preview_url: String,
}

enum BridgeReply {
    Login(Option<(String, String)>),
    Guard(Option<String>),
}

enum AppEvent {
    Log(String),
    Status {
        id: u64,
        status: String,
        detail: String,
        progress: Option<f32>,
    },
    Resolved(Resolved),
    Titles(Vec<ItemMeta>),
    Preview {
        fileid: u64,
        bytes: Vec<u8>,
    },
    PreviewFailed {
        fileid: u64,
    },
    LoginPrompt {
        suggested: String,
        reply: Sender<BridgeReply>,
    },
    GuardPrompt {
        reply: Sender<BridgeReply>,
    },
    LoginDone(LoginResult),
    Finished {
        kind: RunKind,
        result: RunResult,
    },
}

struct ChannelEvents {
    tx: Sender<AppEvent>,
    ctx: egui::Context,
}

impl Events for ChannelEvents {
    fn log(&self, text: String) {
        let _ = self.tx.send(AppEvent::Log(text));
        self.ctx.request_repaint();
    }

    fn status(&self, item_id: u64, status: &str, detail: String, progress: Option<f32>) {
        let _ = self.tx.send(AppEvent::Status {
            id: item_id,
            status: status.to_string(),
            detail,
            progress,
        });
        self.ctx.request_repaint();
    }
}

struct UiBridge {
    tx: Sender<AppEvent>,
    ctx: egui::Context,
}

impl Bridge for UiBridge {
    fn ask_login(&self, suggested: &str, cancel: &AtomicBool) -> Option<(String, String)> {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self
            .tx
            .send(AppEvent::LoginPrompt {
                suggested: suggested.to_string(),
                reply: reply_tx,
            })
            .is_err()
        {
            return None;
        }
        self.ctx.request_repaint();
        loop {
            if cancel.load(Ordering::SeqCst) {
                return None;
            }
            match reply_rx.recv_timeout(Duration::from_millis(150)) {
                Ok(BridgeReply::Login(value)) => return value,
                Ok(BridgeReply::Guard(_)) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    fn ask_guard(&self, cancel: &AtomicBool) -> Option<String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self
            .tx
            .send(AppEvent::GuardPrompt { reply: reply_tx })
            .is_err()
        {
            return None;
        }
        self.ctx.request_repaint();
        loop {
            if cancel.load(Ordering::SeqCst) {
                return None;
            }
            match reply_rx.recv_timeout(Duration::from_millis(150)) {
                Ok(BridgeReply::Guard(value)) => return value,
                Ok(BridgeReply::Login(_)) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }
}

struct QueueItem {
    id: u64,
    appid: u64,
    fileid: u64,
    title: String,
    description: String,
    preview_url: String,
    status: String,
    detail: String,
    progress: Option<f32>,
    cancel: Arc<AtomicBool>,
    kill: Arc<AtomicBool>,
}

impl QueueItem {
    fn download(&self) -> DownloadItem {
        DownloadItem {
            id: self.id,
            appid: self.appid,
            fileid: self.fileid,
            title: self.title.clone(),
            cancel: self.cancel.clone(),
            kill: self.kill.clone(),
        }
    }
}

struct LoginPromptState {
    suggested: String,
    username: String,
    password: String,
    error: Option<String>,
    reply: Option<Sender<BridgeReply>>,
    manual: bool,
}

struct GuardPromptState {
    code: String,
    error: Option<String>,
    reply: Sender<BridgeReply>,
}

struct AccountDraft {
    username: String,
    output_dir: String,
    anonymous: bool,
    auto_open: bool,
    provider: String,
}

impl AccountDraft {
    fn from_settings(settings: &Settings) -> Self {
        Self {
            username: settings.username.clone(),
            output_dir: settings.output_dir.clone(),
            anonymous: settings.anonymous,
            auto_open: settings.auto_open,
            provider: settings.provider.clone(),
        }
    }
}

enum PreviewData {
    Image(TextureHandle),
    Gif {
        frames: Vec<TextureHandle>,
        delays: Vec<Duration>,
        started: Instant,
    },
    None,
}

enum DecodedPreview {
    Image(egui::ColorImage),
    Gif {
        frames: Vec<egui::ColorImage>,
        delays: Vec<Duration>,
    },
}

enum RowAction {
    Cancel(u64),
    Remove(u64),
    MoveUp(u64),
    MoveDown(u64),
}

struct ActiveRun {
    running: bool,
    stop: Arc<AtomicBool>,
}

pub struct WorkshopApp {
    project: PathBuf,
    settings: Settings,
    settings_dirty: bool,
    settings_saved_at: Instant,
    ctx: egui::Context,
    tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,
    resolve_tx: Sender<ResolveJob>,
    preview_tx: Sender<PreviewJob>,
    items: Vec<QueueItem>,
    next_id: u64,
    single_item: Option<QueueItem>,
    single_run: ActiveRun,
    single_resolving: bool,
    single_progress: Option<f32>,
    single_message: String,
    single_status: String,
    queue_run: ActiveRun,
    login_running: bool,
    login_cancel: Arc<AtomicBool>,
    login_dialog: Option<LoginPromptState>,
    guard_dialog: Option<GuardPromptState>,
    settings_open: bool,
    settings_draft: AccountDraft,
    game_picker_open: bool,
    game_search: String,
    item_input: String,
    game_input: String,
    focus_item: bool,
    previews: HashMap<u64, Arc<PreviewData>>,
    preview_requested: HashSet<u64>,
    log: Vec<String>,
    tab: Tab,
    notice: Option<(String, Instant)>,
    clipboard: Option<arboard::Clipboard>,
    last_clip: String,
    last_clip_poll: Instant,
    logo: Option<TextureHandle>,
    closing: Option<Instant>,
    show_log: bool,
}

impl WorkshopApp {
    pub fn new(cc: &eframe::CreationContext<'_>, project: PathBuf, settings: Settings) -> Self {
        setup_fonts(&cc.egui_ctx);
        setup_style(&cc.egui_ctx);

        let ctx = cc.egui_ctx.clone();
        let (tx, rx) = mpsc::channel();
        let (resolve_tx, resolve_rx) = mpsc::channel::<ResolveJob>();
        let (preview_tx, preview_rx) = mpsc::channel::<PreviewJob>();
        let resolve_rx = Arc::new(Mutex::new(resolve_rx));
        let preview_rx = Arc::new(Mutex::new(preview_rx));

        for index in 0..3 {
            spawn_resolve_worker(
                tx.clone(),
                ctx.clone(),
                resolve_rx.clone(),
                format!("resolve-{index}"),
            );
        }
        for index in 0..2 {
            spawn_preview_worker(
                tx.clone(),
                ctx.clone(),
                preview_rx.clone(),
                format!("preview-{index}"),
            );
        }

        let logo = load_logo(&ctx);
        let mut clipboard = arboard::Clipboard::new().ok();
        let last_clip = clipboard
            .as_mut()
            .and_then(|clipboard| clipboard.get_text().ok())
            .unwrap_or_default();

        let startup = vec![
            format!("[app] WorkshopDL {} ready", env!("CARGO_PKG_VERSION")),
            format!("[app] data folder: {}", project.display()),
            format!("[app] provider: {}", provider_label(&settings.provider)),
        ];

        Self {
            project,
            settings,
            settings_dirty: false,
            settings_saved_at: Instant::now(),
            ctx,
            tx,
            rx,
            resolve_tx,
            preview_tx,
            items: Vec::new(),
            next_id: 0,
            single_item: None,
            single_run: ActiveRun {
                running: false,
                stop: Arc::new(AtomicBool::new(false)),
            },
            single_resolving: false,
            single_progress: None,
            single_message: "Paste a workshop link or file ID to begin.".to_string(),
            single_status: "idle".to_string(),
            queue_run: ActiveRun {
                running: false,
                stop: Arc::new(AtomicBool::new(false)),
            },
            login_running: false,
            login_cancel: Arc::new(AtomicBool::new(false)),
            login_dialog: None,
            guard_dialog: None,
            settings_open: false,
            settings_draft: AccountDraft::from_settings(&Settings::default()),
            game_picker_open: false,
            game_search: String::new(),
            item_input: String::new(),
            game_input: String::new(),
            focus_item: false,
            previews: HashMap::new(),
            preview_requested: HashSet::new(),
            log: startup,
            tab: Tab::Single,
            notice: None,
            clipboard,
            last_clip,
            last_clip_poll: Instant::now(),
            logo,
            closing: None,
            show_log: true,
        }
    }

    fn auth(&self) -> Auth {
        Auth {
            anonymous: self.settings.anonymous,
            username: self.settings.username.clone(),
        }
    }

    fn selected_appid(&self) -> Option<u64> {
        games::resolve_input(&self.game_input).0
    }

    fn flash(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }

    fn push_log(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim_end();
            if line.is_empty() || is_progress_line(line) {
                continue;
            }
            self.log.push(line.to_string());
        }
        if self.log.len() > MAX_LOG_LINES {
            let excess = self.log.len() - MAX_LOG_LINES;
            self.log.drain(0..excess);
        }
    }

    fn save_settings(&mut self) {
        self.settings.save(&self.project);
        self.settings_dirty = false;
        self.settings_saved_at = Instant::now();
    }

    fn save_window_size(&mut self, ctx: &egui::Context) {
        if let Some(rect) = ctx.input(|input| input.viewport().inner_rect) {
            let width = rect.width().round();
            let height = rect.height().round();
            if width >= 800.0 {
                self.settings.window_width = width as u32;
            }
            if height >= 560.0 {
                self.settings.window_height = height as u32;
            }
        }
    }

    fn cancel_all(&mut self) {
        self.queue_run.stop.store(true, Ordering::SeqCst);
        self.single_run.stop.store(true, Ordering::SeqCst);
        self.login_cancel.store(true, Ordering::SeqCst);
        for item in &mut self.items {
            if is_active(&item.status) {
                item.cancel.store(true, Ordering::SeqCst);
                item.kill.store(true, Ordering::SeqCst);
            }
        }
        if let Some(item) = &mut self.single_item {
            if is_active(&item.status) {
                item.cancel.store(true, Ordering::SeqCst);
                item.kill.store(true, Ordering::SeqCst);
            }
        }
        if let Some(dialog) = self.login_dialog.take() {
            if let Some(reply) = dialog.reply {
                let _ = reply.send(BridgeReply::Login(None));
            }
        }
        if let Some(dialog) = self.guard_dialog.take() {
            let _ = dialog.reply.send(BridgeReply::Guard(None));
        }
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                AppEvent::Log(text) => self.push_log(&text),
                AppEvent::Status {
                    id,
                    status,
                    detail,
                    progress,
                } => self.apply_status(id, status, detail, progress),
                AppEvent::Resolved(resolved) => self.handle_resolved(resolved),
                AppEvent::Titles(metas) => self.handle_titles(metas),
                AppEvent::Preview { fileid, bytes } => self.handle_preview(ctx, fileid, bytes),
                AppEvent::PreviewFailed { fileid } => {
                    self.preview_requested.remove(&fileid);
                    self.previews.insert(fileid, Arc::new(PreviewData::None));
                }
                AppEvent::LoginPrompt { suggested, reply } => {
                    self.login_dialog = Some(LoginPromptState {
                        username: if self.settings.username.trim().is_empty() {
                            suggested.clone()
                        } else {
                            self.settings.username.clone()
                        },
                        suggested,
                        password: String::new(),
                        error: None,
                        reply: Some(reply),
                        manual: false,
                    });
                    ctx.request_repaint();
                }
                AppEvent::GuardPrompt { reply } => {
                    self.guard_dialog = Some(GuardPromptState {
                        code: String::new(),
                        error: None,
                        reply,
                    });
                    ctx.request_repaint();
                }
                AppEvent::LoginDone(result) => {
                    self.login_running = false;
                    if result.ok {
                        self.settings.username = result.username.clone();
                        self.settings_dirty = true;
                        self.push_log(&format!(
                            "[auth] logged in as {} (token cached by steamcmd)",
                            result.username
                        ));
                        self.flash(format!("Signed in as {}.", result.username));
                    } else {
                        self.push_log(&format!("[auth] login failed: {}", result.detail));
                        self.flash(format!("Login failed: {}", result.detail));
                    }
                }
                AppEvent::Finished { kind, result } => self.handle_finished(kind, result),
            }
        }
    }

    fn apply_status(&mut self, id: u64, status: String, detail: String, progress: Option<f32>) {
        let mut single_hit = false;
        if let Some(item) = self.single_item.as_mut() {
            if item.id == id {
                item.status = status.clone();
                item.detail = detail.clone();
                item.progress = progress;
                single_hit = true;
            }
        }
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            item.status = status.clone();
            item.detail = detail.clone();
            item.progress = progress;
        }
        if single_hit {
            self.single_status = status.clone();
            self.single_progress = progress;
            self.single_message = if detail.trim().is_empty() {
                nice(&status)
            } else {
                format!("{} — {}", nice(&status), detail)
            };
        }
    }

    fn handle_resolved(&mut self, resolved: Resolved) {
        let selected = self.selected_appid();
        match resolved.purpose {
            ResolvePurpose::Single => {
                self.single_resolving = false;
                if resolved.is_collection {
                    let appid = resolved.appid.or(selected);
                    let children = resolved.children.clone();
                    self.expand_collection(appid, children);
                    self.single_status = "idle".to_string();
                    self.single_progress = None;
                    self.single_message =
                        format!("Collection {} added to the Batch tab.", resolved.fileid);
                    self.tab = Tab::Batch;
                    return;
                }

                if let Some(error) = &resolved.resolve_error {
                    self.push_log(&format!(
                        "[item] {}: could not verify details ({error})",
                        resolved.fileid
                    ));
                }
                if let Some(message) = &resolved.detail_error {
                    self.push_log(&format!(
                        "[item] {}: {message} — attempting the download anyway",
                        resolved.fileid
                    ));
                }

                let Some(appid) = resolved.appid.or(selected) else {
                    self.single_status = "failed".to_string();
                    self.single_progress = Some(0.0);
                    self.single_message = format!(
                        "Could not detect the app id for item {}. Pick the game above or paste a full workshop URL.",
                        resolved.fileid
                    );
                    self.flash(format!(
                        "Could not detect the app id for item {}.",
                        resolved.fileid
                    ));
                    self.push_log(&format!(
                        "[item] app id detection failed for {}",
                        resolved.fileid
                    ));
                    return;
                };

                let title = resolved
                    .title
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| format!("Item {}", resolved.fileid));
                let item = self.make_item(
                    appid,
                    resolved.fileid,
                    title,
                    resolved.description,
                    resolved.preview_url,
                );
                self.request_preview(item.fileid, &item.preview_url);
                self.single_item = Some(item);
                self.single_status = "queued".to_string();
                self.single_progress = None;
                self.single_message = "Item resolved. Starting the download ...".to_string();
                self.start_single_download();
            }
            ResolvePurpose::Queue => {
                if resolved.is_collection {
                    let appid = resolved.appid.or(selected);
                    self.expand_collection(appid, resolved.children);
                    return;
                }
                if let Some(message) = resolved.detail_error {
                    self.push_log(&format!(
                        "[queue] item {} skipped: {message}",
                        resolved.fileid
                    ));
                    self.flash(format!("Item {} skipped: {message}", resolved.fileid));
                    return;
                }
                let Some(appid) = resolved.appid.or(selected) else {
                    self.push_log(&format!(
                        "[queue] item {}: could not detect the app id",
                        resolved.fileid
                    ));
                    self.flash(format!(
                        "Could not detect the app id for item {}.",
                        resolved.fileid
                    ));
                    return;
                };
                if let Some(error) = &resolved.resolve_error {
                    self.push_log(&format!(
                        "[queue] {}: could not verify ({error}); adding anyway",
                        resolved.fileid
                    ));
                }
                let title = resolved
                    .title
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| format!("Item {}", resolved.fileid));
                self.enqueue(
                    appid,
                    resolved.fileid,
                    title,
                    resolved.description,
                    resolved.preview_url,
                );
            }
        }
    }

    fn handle_titles(&mut self, metas: Vec<ItemMeta>) {
        let mut previews = Vec::new();
        for meta in metas {
            for item in self
                .items
                .iter_mut()
                .filter(|item| item.fileid == meta.fileid)
            {
                if let Some(title) = &meta.title {
                    if !title.trim().is_empty() {
                        item.title = title.clone();
                    }
                }
                if let Some(appid) = meta.appid {
                    if appid > 0 {
                        item.appid = appid;
                    }
                }
                if !meta.description.is_empty() {
                    item.description = meta.description.clone();
                }
                if !meta.preview_url.is_empty() {
                    item.preview_url = meta.preview_url.clone();
                }
            }
            if let Some(item) = self.single_item.as_mut() {
                if item.fileid == meta.fileid {
                    if let Some(title) = &meta.title {
                        if !title.trim().is_empty() {
                            item.title = title.clone();
                        }
                    }
                    if !meta.description.is_empty() {
                        item.description = meta.description.clone();
                    }
                    if !meta.preview_url.is_empty() {
                        item.preview_url = meta.preview_url.clone();
                    }
                }
            }
            if !meta.preview_url.is_empty() {
                previews.push((meta.fileid, meta.preview_url));
            }
        }
        for (fileid, url) in previews {
            self.request_preview(fileid, &url);
        }
    }

    fn handle_preview(&mut self, ctx: &egui::Context, fileid: u64, bytes: Vec<u8>) {
        self.preview_requested.remove(&fileid);
        let Some(decoded) = decode_preview(&bytes) else {
            self.previews.insert(fileid, Arc::new(PreviewData::None));
            return;
        };
        let data = match decoded {
            DecodedPreview::Image(image) => PreviewData::Image(ctx.load_texture(
                format!("preview-{fileid}"),
                image,
                TextureOptions::LINEAR,
            )),
            DecodedPreview::Gif { frames, delays } => {
                let textures = frames
                    .into_iter()
                    .enumerate()
                    .map(|(index, image)| {
                        ctx.load_texture(
                            format!("preview-{fileid}-{index}"),
                            image,
                            TextureOptions::LINEAR,
                        )
                    })
                    .collect();
                PreviewData::Gif {
                    frames: textures,
                    delays,
                    started: Instant::now(),
                }
            }
        };
        self.previews.insert(fileid, Arc::new(data));
        ctx.request_repaint();
    }

    fn request_preview(&mut self, fileid: u64, url: &str) {
        if fileid == 0 || url.trim().is_empty() {
            return;
        }
        if self.previews.contains_key(&fileid) || !self.preview_requested.insert(fileid) {
            return;
        }
        if self
            .preview_tx
            .send(PreviewJob {
                fileid,
                url: url.to_string(),
            })
            .is_err()
        {
            self.preview_requested.remove(&fileid);
        }
    }

    fn queue_resolve(&self, purpose: ResolvePurpose, fileid: u64, appid: Option<u64>) {
        let _ = self.resolve_tx.send(ResolveJob::Item {
            purpose,
            fileid,
            appid,
        });
    }

    fn request_titles(&self, appid: Option<u64>, fileids: Vec<u64>) {
        if fileids.is_empty() {
            return;
        }
        let _ = self.resolve_tx.send(ResolveJob::Titles { appid, fileids });
    }

    fn make_item(
        &mut self,
        appid: u64,
        fileid: u64,
        title: String,
        description: String,
        preview_url: String,
    ) -> QueueItem {
        self.next_id += 1;
        QueueItem {
            id: self.next_id,
            appid,
            fileid,
            title,
            description,
            preview_url,
            status: "queued".to_string(),
            detail: String::new(),
            progress: None,
            cancel: Arc::new(AtomicBool::new(false)),
            kill: Arc::new(AtomicBool::new(false)),
        }
    }

    fn enqueue(
        &mut self,
        appid: u64,
        fileid: u64,
        title: String,
        description: String,
        preview_url: String,
    ) {
        if self
            .items
            .iter()
            .any(|item| item.fileid == fileid && item.appid == appid)
        {
            self.push_log(&format!("[queue] already queued {appid}:{fileid}"));
            return;
        }
        let item = self.make_item(appid, fileid, title, description, preview_url);
        self.request_preview(item.fileid, &item.preview_url);
        self.push_log(&format!("[queue] added {fileid} (app {appid})"));
        self.items.push(item);
    }

    fn expand_collection(&mut self, appid: Option<u64>, children: Vec<u64>) {
        if children.is_empty() {
            self.push_log("[queue] collection is empty");
            self.flash("That collection is empty.");
            return;
        }
        self.push_log(&format!(
            "[queue] collection expands to {} item(s)",
            children.len()
        ));
        if let Some(appid) = appid {
            for &child in &children {
                self.enqueue(
                    appid,
                    child,
                    format!("Item {child}"),
                    String::new(),
                    String::new(),
                );
            }
            self.request_titles(Some(appid), children);
            self.flash(format!("{} collection item(s) added.", self.items.len()));
        } else {
            for child in children {
                self.queue_resolve(ResolvePurpose::Queue, child, None);
            }
            self.flash("Looking up collection items ...");
        }
        self.tab = Tab::Batch;
    }

    fn parse_jobs(&self, text: &str) -> Vec<(u64, Option<u64>)> {
        let selected = self.selected_appid();
        let mut seen = HashSet::new();
        let mut jobs = Vec::new();
        for token in text.split(|character: char| {
            character.is_whitespace() || character == ',' || character == ';'
        }) {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            let Some(fileid) = workshop_api::extract_workshop_id(token) else {
                continue;
            };
            if !seen.insert(fileid) {
                continue;
            }
            jobs.push((fileid, appid_from_url(token).or(selected)));
        }
        jobs
    }

    fn begin_single(&mut self) {
        if self.single_run.running || self.queue_run.running {
            self.flash("A download is already running; wait or stop it first.");
            return;
        }
        if self.login_running {
            self.flash("A Steam login is in progress; try again in a moment.");
            return;
        }
        let jobs = self.parse_jobs(&self.item_input);
        if jobs.is_empty() {
            self.flash("Paste a workshop item URL or file id first.");
            return;
        }
        if jobs.len() > 1 {
            let count = jobs.len();
            for (fileid, appid) in jobs {
                self.queue_resolve(ResolvePurpose::Queue, fileid, appid);
            }
            self.item_input.clear();
            self.tab = Tab::Batch;
            self.single_status = "idle".to_string();
            self.single_message = format!("{count} items added to the Batch tab.");
            self.flash(format!("{count} items added to the Batch queue."));
            self.push_log(&format!("[item] {count} ids found — adding them to Batch"));
            return;
        }

        let (fileid, appid) = jobs[0];
        self.single_item = None;
        self.single_resolving = true;
        self.single_status = "resolving".to_string();
        self.single_progress = None;
        self.single_message = format!("Resolving item {fileid} ...");
        self.push_log(&format!("[item] resolving {fileid} ..."));
        self.queue_resolve(ResolvePurpose::Single, fileid, appid);
    }

    fn add_input_to_queue(&mut self) {
        let jobs = self.parse_jobs(&self.item_input);
        if jobs.is_empty() {
            self.flash("Enter workshop URLs or file ids in the Item field first.");
            return;
        }
        let count = jobs.len();
        for (fileid, appid) in jobs {
            self.queue_resolve(ResolvePurpose::Queue, fileid, appid);
        }
        self.item_input.clear();
        self.tab = Tab::Batch;
        self.push_log(&format!("[queue] resolving {count} item(s) ..."));
        self.flash(format!("{count} item(s) queued for lookup."));
    }

    fn start_single_download(&mut self) {
        if self.single_run.running {
            return;
        }
        if self.queue_run.running {
            self.flash("A batch download is running; finish or stop it first.");
            return;
        }
        let Some(item) = self.single_item.as_ref() else {
            return;
        };
        let output = PathBuf::from(self.settings.output_dir.trim());
        if output.as_os_str().is_empty() {
            self.flash("Choose an output directory first.");
            return;
        }
        if let Err(error) = fs::create_dir_all(&output) {
            self.flash(format!("Could not create the output folder: {error}"));
            return;
        }

        let download = item.download();
        let title = item.title.clone();
        let output_text = output.display().to_string();
        self.single_progress = None;
        self.single_status = "downloading".to_string();
        self.single_message = format!("Starting {title} ...");
        self.single_run = ActiveRun {
            running: true,
            stop: Arc::new(AtomicBool::new(false)),
        };
        self.push_log(&format!(
            "--- single item download (provider: {}) ---",
            provider_label(&self.settings.provider)
        ));
        self.push_log(&format!("[item] output: {output_text}"));
        let stop = self.single_run.stop.clone();
        self.spawn_download(RunKind::Single, vec![download], stop, output);
    }

    fn start_queue(&mut self) {
        if self.queue_run.running || self.single_run.running || self.login_running {
            return;
        }
        if self.items.is_empty() {
            self.flash("Batch queue is empty. Add items first.");
            return;
        }
        let output = PathBuf::from(self.settings.output_dir.trim());
        if output.as_os_str().is_empty() {
            self.flash("Choose an output directory.");
            return;
        }
        if let Err(error) = fs::create_dir_all(&output) {
            self.flash(format!("Could not create the output folder: {error}"));
            return;
        }

        let mut downloads = Vec::new();
        for item in &mut self.items {
            if item.status == "done" || item.cancel.load(Ordering::SeqCst) {
                continue;
            }
            if item.appid == 0 {
                continue;
            }
            item.status = "queued".to_string();
            item.detail.clear();
            item.progress = None;
            downloads.push(item.download());
        }
        if downloads.is_empty() {
            self.flash("Nothing to download. Clear finished rows or add new items.");
            return;
        }
        self.queue_run = ActiveRun {
            running: true,
            stop: Arc::new(AtomicBool::new(false)),
        };
        self.push_log(&format!(
            "--- batch download: {} item(s) (provider: {}) ---",
            downloads.len(),
            provider_label(&self.settings.provider)
        ));
        let stop = self.queue_run.stop.clone();
        self.spawn_download(RunKind::Batch, downloads, stop, output);
    }

    fn stop_single(&mut self) {
        if !self.single_run.running && !self.single_resolving {
            return;
        }
        self.single_run.stop.store(true, Ordering::SeqCst);
        if let Some(item) = &mut self.single_item {
            item.cancel.store(true, Ordering::SeqCst);
            item.kill.store(true, Ordering::SeqCst);
        }
        self.single_resolving = false;
        self.single_status = "cancelled".to_string();
        self.single_message = "Stop requested.".to_string();
        self.push_log("[item] stop requested");
    }

    fn stop_queue(&mut self) {
        if !self.queue_run.running {
            return;
        }
        self.queue_run.stop.store(true, Ordering::SeqCst);
        for item in &mut self.items {
            if matches!(item.status.as_str(), "downloading" | "verifying") {
                item.cancel.store(true, Ordering::SeqCst);
                item.kill.store(true, Ordering::SeqCst);
            }
        }
        self.push_log("[queue] stop requested");
    }

    fn spawn_download(
        &mut self,
        kind: RunKind,
        items: Vec<DownloadItem>,
        stop: Arc<AtomicBool>,
        output: PathBuf,
    ) {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let project = self.project.clone();
        let provider = self.settings.provider.clone();
        let auth = self.auth();
        let spawned = thread::Builder::new()
            .name("download".to_string())
            .spawn(move || {
                let events = Arc::new(ChannelEvents {
                    tx: tx.clone(),
                    ctx: ctx.clone(),
                }) as Arc<dyn Events>;
                let bridge = Arc::new(UiBridge {
                    tx: tx.clone(),
                    ctx: ctx.clone(),
                }) as Arc<dyn Bridge>;
                let result = if provider == "ggntw" {
                    GgNetworkManager::new(events).run_queue(&items, &output, &stop)
                } else {
                    SteamCmdManager::new(project.join("steamcmd"), events, bridge)
                        .run_queue(&items, &auth, &output, &stop)
                };
                let _ = tx.send(AppEvent::Finished { kind, result });
                ctx.request_repaint();
            });
        if spawned.is_err() {
            self.queue_run.running = false;
            self.single_run.running = false;
            self.flash("Could not start the download worker.");
        }
    }

    fn handle_finished(&mut self, kind: RunKind, result: RunResult) {
        match kind {
            RunKind::Single => self.finish_single(result),
            RunKind::Batch => self.finish_batch(result),
        }
    }

    fn finish_single(&mut self, result: RunResult) {
        self.single_run.running = false;
        let output = PathBuf::from(self.settings.output_dir.trim());
        let mut saved_path = None;
        if let Some(item) = &self.single_item {
            if item.status != "done" {
                saved_path = item_on_disk(&self.project, &output, item.appid, item.fileid);
                if let Some(path) = &saved_path {
                    if let Some(item) = self.single_item.as_mut() {
                        item.status = "done".to_string();
                        item.detail = format!("Saved at {}", path.display());
                        item.progress = Some(1.0);
                    }
                }
            }
        }

        let (status, detail) = match self.single_item.as_ref() {
            Some(item) => (item.status.clone(), item.detail.clone()),
            None => ("failed".to_string(), "No item was loaded".to_string()),
        };

        if !result.ok {
            self.push_log(&format!("[item] download stopped: {}", result.detail));
            self.flash(format!("Download failed: {}", result.detail));
        }
        if let Some(path) = saved_path {
            self.push_log(&format!("[done] item found on disk at {}", path.display()));
        }

        match status.as_str() {
            "done" => {
                self.single_status = "done".to_string();
                self.single_progress = Some(1.0);
                self.single_message = if detail.trim().is_empty() {
                    "Done.".to_string()
                } else {
                    format!("Done — {detail}")
                };
                self.push_log(&format!(
                    "[done] item saved to {}",
                    self.settings.output_dir
                ));
                self.play_finish();
                self.maybe_open_output();
            }
            "cancelled" => {
                self.single_status = "cancelled".to_string();
                self.single_progress = Some(0.0);
                self.single_message = "Cancelled.".to_string();
            }
            _ => {
                self.single_status = "failed".to_string();
                self.single_progress = Some(0.0);
                self.single_message = if detail.trim().is_empty() {
                    "Download failed.".to_string()
                } else {
                    format!("Failed — {detail}")
                };
                self.flash("Download failed.");
            }
        }
    }

    fn finish_batch(&mut self, result: RunResult) {
        self.queue_run.running = false;
        let output = PathBuf::from(self.settings.output_dir.trim());

        let mut found = Vec::new();
        for entry in &result.results {
            if entry.status != "failed" {
                continue;
            }
            let Some(index) = self.items.iter().position(|item| item.id == entry.id) else {
                continue;
            };
            let (appid, fileid, title) = {
                let item = &self.items[index];
                (item.appid, item.fileid, item.title.clone())
            };
            if let Some(path) = item_on_disk(&self.project, &output, appid, fileid) {
                self.items[index].status = "done".to_string();
                self.items[index].detail = format!("Saved at {}", path.display());
                self.items[index].progress = Some(1.0);
                found.push(format!(
                    "[done] {title} found on disk at {}",
                    path.display()
                ));
            }
        }
        for line in found {
            self.push_log(&line);
        }

        let mut done = 0usize;
        let mut failed = 0usize;
        let mut cancelled = 0usize;
        for entry in &result.results {
            let status = self
                .items
                .iter()
                .find(|item| item.id == entry.id)
                .map(|item| item.status.clone())
                .unwrap_or_else(|| entry.status.clone());
            match status.as_str() {
                "done" => done += 1,
                "cancelled" => cancelled += 1,
                _ => {
                    failed += 1;
                    if !entry.detail.trim().is_empty() {
                        self.push_log(&format!("[failed] {}: {}", entry.fileid, entry.detail));
                    }
                }
            }
        }

        if !result.ok {
            self.push_log(&format!("[queue] stopped: {}", result.detail));
            self.flash(format!("Download stopped: {}", result.detail));
        }
        if done > 0 {
            self.push_log(&format!("[done] {done} item(s) finished, {failed} failed"));
            self.play_finish();
            self.maybe_open_output();
        } else if failed > 0 {
            self.push_log(&format!("[done] all items failed ({failed})"));
        } else if cancelled > 0 {
            self.push_log("[done] queue stopped");
        } else {
            self.push_log("[done] queue finished (nothing downloaded)");
        }
    }

    fn play_finish(&mut self) {
        util::play_finish_sound(&self.project);
        self.push_log("[sounds] finish sound");
    }

    fn maybe_open_output(&mut self) {
        if self.settings.auto_open {
            self.open_output();
        } else {
            self.push_log("[open] download finished, auto-open is disabled");
        }
    }

    fn open_output(&mut self) {
        let path = PathBuf::from(self.settings.output_dir.trim());
        if path.as_os_str().is_empty() {
            self.flash("No output folder is set.");
            return;
        }
        if let Err(error) = fs::create_dir_all(&path) {
            self.flash(format!("Could not open the output folder: {error}"));
            return;
        }
        util::open_path(&path);
    }

    fn pick_output(&mut self) {
        let mut dialog = rfd::FileDialog::new().set_title("Choose the download folder");
        if !self.settings.output_dir.trim().is_empty() {
            dialog = dialog.set_directory(self.settings.output_dir.trim());
        }
        if let Some(path) = dialog.pick_folder() {
            self.settings.output_dir = path.to_string_lossy().into_owned();
            self.settings_dirty = true;
        }
    }

    fn import_list(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Text files", &["txt"])
            .pick_file()
        else {
            return;
        };
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                self.flash(format!("Import failed: {error}"));
                return;
            }
        };
        let selected = self.selected_appid();
        let mut count = 0usize;
        let mut errors = 0usize;
        for (line_number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("URL:") {
                continue;
            }
            if let Some((appid, fileid)) = parse_appid_fileid(line) {
                self.queue_resolve(ResolvePurpose::Queue, fileid, Some(appid));
                count += 1;
                continue;
            }
            if let Some(fileid) = workshop_api::extract_workshop_id(line) {
                let appid = appid_from_url(line).or(selected);
                self.queue_resolve(ResolvePurpose::Queue, fileid, appid);
                count += 1;
                continue;
            }
            errors += 1;
            if errors <= 20 {
                self.push_log(&format!(
                    "[import] line {}: {}",
                    line_number + 1,
                    line.chars().take(70).collect::<String>()
                ));
            }
        }
        self.tab = Tab::Batch;
        self.push_log(&format!(
            "[import] loaded {count} item line(s); {errors} skipped"
        ));
        self.flash(format!("Imported {count} item(s); {errors} skipped."));
    }

    fn export_queue(&mut self) {
        if self.items.is_empty() {
            self.flash("The queue is empty.");
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .set_file_name("workshop_queue.txt")
            .add_filter("Text files", &["txt"])
            .save_file()
        else {
            return;
        };
        let mut text = String::new();
        for item in &self.items {
            text.push_str(&format!(
                "URL: https://steamcommunity.com/sharedfiles/filedetails/?id={}\n{}:{}\n",
                item.fileid, item.appid, item.fileid
            ));
        }
        match fs::write(&path, text) {
            Ok(()) => {
                self.push_log(&format!(
                    "[export] wrote {} item(s) to {}",
                    self.items.len(),
                    path.display()
                ));
                self.flash(format!("Queue exported to {}.", path.display()));
            }
            Err(error) => self.flash(format!("Export failed: {error}")),
        }
    }

    fn open_settings(&mut self) {
        self.settings_draft = AccountDraft::from_settings(&self.settings);
        self.settings_open = true;
    }

    fn apply_draft(&mut self) {
        let draft = std::mem::replace(
            &mut self.settings_draft,
            AccountDraft::from_settings(&self.settings),
        );
        self.settings.username = draft.username.trim().to_string();
        self.settings.output_dir = draft.output_dir.trim().to_string();
        if self.settings.output_dir.is_empty() {
            self.settings.output_dir = util::default_output_dir(&self.project)
                .to_string_lossy()
                .into_owned();
        }
        self.settings.anonymous = draft.anonymous;
        self.settings.auto_open = draft.auto_open;
        self.settings.provider = if draft.provider == "ggntw" {
            "ggntw".to_string()
        } else {
            "steamcmd".to_string()
        };
        self.settings_dirty = false;
        self.settings.save(&self.project);
        self.settings_saved_at = Instant::now();
    }

    fn open_manual_login(&mut self) {
        if self.settings.anonymous {
            self.flash("Uncheck Anonymous mode to sign in.");
            return;
        }
        if self.login_running || self.queue_run.running || self.single_run.running {
            self.flash("Wait for the current download or login to finish.");
            return;
        }
        self.login_dialog = Some(LoginPromptState {
            suggested: self.settings.username.clone(),
            username: self.settings.username.clone(),
            password: String::new(),
            error: None,
            reply: None,
            manual: true,
        });
    }

    fn start_login(&mut self, username: &str, password: &str) {
        let username = username.trim().to_string();
        if username.is_empty() {
            self.flash("Enter a Steam username.");
            return;
        }
        if self.login_running {
            self.flash("A login is already running.");
            return;
        }
        if self.queue_run.running || self.single_run.running {
            self.flash("Stop the current download before signing in.");
            return;
        }
        self.login_running = true;
        self.login_cancel = Arc::new(AtomicBool::new(false));
        self.push_log(&format!("[auth] starting login as {username} ..."));

        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let steamcmd = self.project.join("steamcmd");
        let password = password.to_string();
        let cancel = self.login_cancel.clone();
        let spawned = thread::Builder::new()
            .name("login".to_string())
            .spawn(move || {
                let events = Arc::new(ChannelEvents {
                    tx: tx.clone(),
                    ctx: ctx.clone(),
                }) as Arc<dyn Events>;
                let bridge = Arc::new(UiBridge {
                    tx: tx.clone(),
                    ctx: ctx.clone(),
                }) as Arc<dyn Bridge>;
                let manager = SteamCmdManager::new(steamcmd, events, bridge);
                let result = manager.login(&username, &password, &cancel);
                let _ = tx.send(AppEvent::LoginDone(result));
                ctx.request_repaint();
            });
        if spawned.is_err() {
            self.login_running = false;
            self.flash("Could not start the login worker.");
        }
    }

    fn apply_row_action(&mut self, action: RowAction) {
        match action {
            RowAction::Cancel(id) => {
                if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
                    item.cancel.store(true, Ordering::SeqCst);
                    item.kill.store(true, Ordering::SeqCst);
                    if item.status == "queued" {
                        item.status = "cancelled".to_string();
                        item.detail = "Cancelled".to_string();
                    } else if is_active(&item.status) {
                        item.detail = "Cancelling current download ...".to_string();
                    }
                }
            }
            RowAction::Remove(id) => {
                if let Some(index) = self.items.iter().position(|item| item.id == id) {
                    let item = self.items.remove(index);
                    item.cancel.store(true, Ordering::SeqCst);
                    item.kill.store(true, Ordering::SeqCst);
                    self.push_log(&format!("[queue] removed {}", item.fileid));
                }
            }
            RowAction::MoveUp(id) => {
                if let Some(index) = self.items.iter().position(|item| item.id == id) {
                    if index > 0 {
                        self.items.swap(index, index - 1);
                    }
                }
            }
            RowAction::MoveDown(id) => {
                if let Some(index) = self.items.iter().position(|item| item.id == id) {
                    if index + 1 < self.items.len() {
                        self.items.swap(index, index + 1);
                    }
                }
            }
        }
    }

    fn clear_finished(&mut self) {
        let before = self.items.len();
        self.items
            .retain(|item| !matches!(item.status.as_str(), "done" | "failed" | "cancelled"));
        let removed = before - self.items.len();
        if removed > 0 {
            self.push_log(&format!("[queue] removed {removed} finished row(s)"));
        }
    }

    fn summary(&self) -> String {
        if self.items.is_empty() {
            return "Queue is empty".to_string();
        }
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for item in &self.items {
            *counts.entry(item.status.as_str()).or_default() += 1;
        }
        let mut parts = vec![format!("{} item(s)", self.items.len())];
        for status in [
            "queued",
            "downloading",
            "verifying",
            "done",
            "failed",
            "cancelled",
        ] {
            if let Some(count) = counts.get(status) {
                parts.push(format!("{count} {}", nice(status)));
            }
        }
        parts.join("  ·  ")
    }

    fn poll_clipboard(&mut self, ctx: &egui::Context) {
        if self.last_clip_poll.elapsed() < Duration::from_millis(600) {
            return;
        }
        self.last_clip_poll = Instant::now();
        let Some(clipboard) = self.clipboard.as_mut() else {
            return;
        };
        let Ok(text) = clipboard.get_text() else {
            return;
        };
        let text = text.trim().to_string();
        if text.is_empty() || text == self.last_clip {
            return;
        }
        self.last_clip = text.clone();
        if let Some(fileid) = workshop_api::extract_workshop_id(&text) {
            if workshop_api::extract_workshop_id(&self.item_input) != Some(fileid) {
                self.item_input = text;
                self.focus_item = true;
                self.tab = Tab::Single;
                ctx.request_repaint();
            }
            return;
        }
        if let Some(appid) = workshop_api::extract_app_id(&text) {
            if appid > 0 {
                self.game_input = text;
                ctx.request_repaint();
            }
        }
    }

    fn draw_header(&mut self, ctx: &egui::Context) {
        let (auth_text, auth_color) = self.auth_label();
        let can_login = !self.settings.anonymous && !self.login_running;
        let mut open_settings = false;
        let mut open_login = false;
        let mut open_workshop = false;

        egui::TopBottomPanel::top("header")
            .frame(
                Frame::NONE
                    .fill(HEADER_BG)
                    .inner_margin(Margin::symmetric(16, 10)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(logo) = &self.logo {
                        ui.add(
                            egui::Image::new(logo)
                                .fit_to_exact_size(Vec2::splat(36.0))
                                .corner_radius(2.0),
                        );
                    }
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("WorkshopDL")
                                .size(21.0)
                                .strong()
                                .color(TEXT),
                        );
                        ui.label(
                            RichText::new("Steam Workshop downloader")
                                .size(11.0)
                                .color(MUTED),
                        );
                    });
                    ui.add_space(14.0);
                    Frame::NONE
                        .fill(CARD_BG)
                        .corner_radius(2.0)
                        .stroke(Stroke::new(1.0_f32, BORDER))
                        .inner_margin(Margin::symmetric(10, 4))
                        .show(ui, |ui| {
                            ui.label(RichText::new(&auth_text).size(11.5).color(auth_color));
                        });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Steam Workshop").clicked() {
                            open_workshop = true;
                        }
                        if ui.button("Account ...").clicked() {
                            open_settings = true;
                        }
                        if ui
                            .add_enabled(can_login, Button::new("Login ..."))
                            .on_hover_text(if self.settings.anonymous {
                                "Uncheck Anonymous mode to sign in with a Steam account."
                            } else {
                                "Sign in to Steam. Steam Guard is handled inside the app."
                            })
                            .clicked()
                        {
                            open_login = true;
                        }
                        if self.login_running {
                            ui.spinner();
                        }
                    });
                });

                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .checkbox(&mut self.settings.anonymous, "Anonymous mode")
                        .on_hover_text(
                            "Download public items without an account. Uncheck to use your Steam account.",
                        )
                        .changed()
                    {
                        self.settings_dirty = true;
                        self.push_log(if self.settings.anonymous {
                            "[auth] anonymous mode enabled"
                        } else {
                            "[auth] account mode enabled"
                        });
                    }
                    ui.separator();
                    ui.label(RichText::new("Provider").size(11.0).color(MUTED));
                    let mut provider = self.settings.provider.clone();
                    ComboBox::from_id_salt("provider")
                        .selected_text(provider_label(&provider))
                        .width(132.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut provider,
                                "steamcmd".to_string(),
                                "SteamCMD",
                            );
                            ui.selectable_value(
                                &mut provider,
                                "ggntw".to_string(),
                                "GGNetwork",
                            );
                        });
                    if provider != self.settings.provider {
                        self.settings.provider = provider;
                        self.settings_dirty = true;
                        self.push_log(&format!(
                            "[provider] download provider set to {}",
                            provider_label(&self.settings.provider)
                        ));
                    }
                    ui.separator();
                    if ui
                        .checkbox(&mut self.settings.auto_open, "Auto-open folder")
                        .on_hover_text(
                            "Open the output folder in the file manager when a download finishes.",
                        )
                        .changed()
                    {
                        self.settings_dirty = true;
                        self.push_log(if self.settings.auto_open {
                            "[open] auto-open enabled"
                        } else {
                            "[open] auto-open disabled"
                        });
                    }
                    ui.separator();
                    ui.label(
                        RichText::new(format!("{} supported games", games::all().len()))
                            .size(10.0)
                            .color(DIM),
                    );
                });

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Save to").size(11.0).color(MUTED));
                    let width = (ui.available_width() - 190.0).max(180.0);
                    let response = ui.add(
                        TextEdit::singleline(&mut self.settings.output_dir)
                            .desired_width(width)
                            .hint_text("Download folder"),
                    );
                    if response.changed() {
                        self.settings_dirty = true;
                    }
                    if ui.button("Browse ...").clicked() {
                        self.pick_output();
                    }
                    if ui.button("Open").clicked() {
                        self.open_output();
                    }
                });
            });

        if open_settings {
            self.open_settings();
        }
        if open_login {
            self.open_manual_login();
        }
        if open_workshop {
            desktop::open_workshop();
            self.push_log("[browse] opened the Steam Workshop in the browser");
        }
    }

    fn auth_label(&self) -> (String, Color32) {
        if self.settings.anonymous {
            return ("Steam: anonymous mode".to_string(), MUTED);
        }
        if self.login_running {
            return ("Steam: signing in ...".to_string(), BLUE);
        }
        if self.settings.username.trim().is_empty() {
            return ("Steam: account mode — no username set".to_string(), YELLOW);
        }
        (format!("Steam account: {}", self.settings.username), TEXT)
    }

    fn draw_status_bar(&mut self, ctx: &egui::Context) {
        let notice = self
            .notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(6))
            .map(|(text, _)| text.clone());
        let activity = if self.queue_run.running {
            Some("Batch download running")
        } else if self.single_run.running {
            Some("Downloading item")
        } else if self.login_running {
            Some("Signing in to Steam")
        } else {
            None
        };

        egui::TopBottomPanel::bottom("status")
            .frame(
                Frame::NONE
                    .fill(HEADER_BG)
                    .inner_margin(Margin::symmetric(14, 5)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(text) = &notice {
                        ui.label(RichText::new(text).size(11.0).color(TEXT));
                    } else if let Some(text) = activity {
                        ui.spinner();
                        ui.label(RichText::new(text).size(11.0).color(BLUE));
                    } else {
                        ui.label(RichText::new("Ready").size(11.0).color(MUTED));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.toggle_value(&mut self.show_log, "Log");
                        ui.separator();
                        ui.label(
                            RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                                .size(10.0)
                                .color(DIM),
                        );
                        if self.login_running {
                            ui.spinner();
                        }
                    });
                });
            });
    }

    fn draw_log_panel(&mut self, ctx: &egui::Context) {
        if !self.show_log {
            return;
        }
        egui::TopBottomPanel::bottom("log")
            .resizable(true)
            .default_height(150.0)
            .height_range(76.0..=360.0)
            .frame(
                Frame::NONE
                    .fill(CARD_BG)
                    .inner_margin(Margin::symmetric(14, 6)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Activity").size(11.5).strong().color(TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Clear").clicked() {
                            self.log.clear();
                        }
                        ui.label(
                            RichText::new(format!("{} lines", self.log.len()))
                                .size(9.5)
                                .color(DIM),
                        );
                    });
                });
                ui.add_space(2.0);
                ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let start = self.log.len().saturating_sub(800);
                        for line in &self.log[start..] {
                            ui.add(
                                Label::new(
                                    RichText::new(line).monospace().size(10.5).color(LOG_TEXT),
                                )
                                .wrap(),
                            );
                        }
                    });
            });
    }

    fn draw_single_tab(&mut self, ui: &mut Ui) {
        let mut start = false;
        let mut stop = false;
        let mut open_picker = false;

        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Game").size(11.0).color(MUTED));
                    ui.add(
                        TextEdit::singleline(&mut self.game_input)
                            .hint_text("Optional — auto-detected from the link")
                            .desired_width(360.0),
                    );
                    if ui.button("Browse games").clicked() {
                        open_picker = true;
                    }
                });
                let (_, hint) = games::resolve_input(&self.game_input);
                if hint.is_empty() {
                    ui.label(
                        RichText::new(
                            "Leave empty to detect the game from the workshop link or the Steam API.",
                        )
                        .size(10.5)
                        .color(DIM),
                    );
                } else {
                    let color = if hint.starts_with('→') { GREEN } else { YELLOW };
                    ui.label(RichText::new(&hint).size(10.5).color(color));
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Item").size(11.0).color(MUTED));
                    let response = ui.add(
                        TextEdit::singleline(&mut self.item_input)
                            .hint_text(
                                "Workshop item URL or published file ID — multiple IDs go to Batch",
                            )
                            .desired_width(f32::INFINITY),
                    );
                    if self.focus_item {
                        response.request_focus();
                        self.focus_item = false;
                    }
                    if response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter))
                    {
                        start = true;
                    }
                });

                ui.add_space(10.0);
                let (title, subtitle, description, fileid, status, busy) = {
                    let item = self.single_item.as_ref();
                    (
                        item.map(|item| item.title.clone())
                            .unwrap_or_else(|| "No item loaded".to_string()),
                        item.map(|item| {
                            format!("App {}  ·  File {}", item.appid, item.fileid)
                        })
                        .unwrap_or_default(),
                        item.map(|item| item.description.clone())
                            .unwrap_or_default(),
                        item.map(|item| item.fileid),
                        item.map(|item| item.status.clone())
                            .unwrap_or_else(|| self.single_status.clone()),
                        self.single_run.running || self.single_resolving,
                    )
                };
                let preview = fileid.and_then(|id| self.previews.get(&id).cloned());
                let placeholder = if self.single_resolving {
                    "Loading ..."
                } else {
                    "No preview"
                };

                Frame::NONE
                    .fill(CARD_BG)
                    .corner_radius(2.0)
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .inner_margin(12)
                    .show(ui, |ui| {
                        ui.horizontal_top(|ui| {
                            draw_preview(ui, preview.as_deref(), 112.0, placeholder);
                            ui.add_space(12.0);
                            ui.vertical(|ui| {
                                ui.set_width(ui.available_width().max(120.0));
                                ui.add(
                                    Label::new(
                                        RichText::new(&title).size(17.0).strong().color(TEXT),
                                    )
                                    .truncate(),
                                );
                                if !subtitle.is_empty() {
                                    ui.label(
                                        RichText::new(&subtitle)
                                            .monospace()
                                            .size(10.5)
                                            .color(MUTED),
                                    );
                                }
                                ui.add_space(5.0);
                                let text = if description.is_empty() {
                                    if busy {
                                        "Loading item details ...".to_string()
                                    } else {
                                        "No description available.".to_string()
                                    }
                                } else {
                                    description
                                };
                                ScrollArea::vertical()
                                    .max_height(96.0)
                                    .auto_shrink([false, true])
                                    .show(ui, |ui| {
                                        ui.add(
                                            Label::new(
                                                RichText::new(text).size(12.0).color(MUTED),
                                            )
                                            .wrap(),
                                        );
                                    });
                            });
                        });
                    });

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let can_start = !self.single_run.running
                        && !self.queue_run.running
                        && !self.single_resolving
                        && !self.login_running;
                    if primary_button(ui, "Download Item", can_start, 150.0, 34.0) {
                        start = true;
                    }
                    if ui
                        .add_enabled(
                            self.single_run.running || self.single_resolving,
                            Button::new("Stop").min_size(Vec2::new(70.0, 34.0)),
                        )
                        .clicked()
                    {
                        stop = true;
                    }
                    ui.add_space(8.0);
                    let value = self.single_progress.unwrap_or(0.0);
                    let active = self.single_run.running || self.single_resolving;
                    let color = if status == "done" {
                        GREEN
                    } else if status == "failed" {
                        RED
                    } else {
                        BLUE
                    };
                    let fill = if value <= 0.0 && !active {
                        Color32::TRANSPARENT
                    } else {
                        color
                    };
                    ui.add(
                        ProgressBar::new(value)
                            .desired_width(180.0)
                            .desired_height(18.0)
                            .fill(fill)
                            .corner_radius(2.0)
                            .animate(active),
                    );
                    ui.label(
                        RichText::new(nice(&status))
                            .size(11.0)
                            .color(status_color(&status)),
                    );
                });

                ui.add_space(6.0);
                let message_color = status_color(&self.single_status);
                ui.add(
                    Label::new(
                        RichText::new(&self.single_message)
                            .size(12.0)
                            .color(message_color),
                    )
                    .wrap(),
                );
                ui.add_space(2.0);
                ui.label(
                    RichText::new(
                        "The app id is detected automatically. Collections and multiple IDs are added to the Batch tab.",
                    )
                    .size(10.5)
                    .color(DIM),
                );
            });

        if open_picker {
            self.game_search.clear();
            self.game_picker_open = true;
        }
        if start {
            self.begin_single();
        }
        if stop {
            self.stop_single();
        }
    }

    fn draw_batch_tab(&mut self, ui: &mut Ui) {
        let mut add = false;
        let mut import = false;
        let mut export = false;
        let mut open_output = false;
        let mut start = false;
        let mut stop = false;
        let mut clear = false;

        ui.horizontal_wrapped(|ui| {
            if ui.button("Add To Queue").clicked() {
                add = true;
            }
            if ui.button("Import List ...").clicked() {
                import = true;
            }
            if ui.button("Export Queue ...").clicked() {
                export = true;
            }
            if ui.button("Open Output").clicked() {
                open_output = true;
            }
            ui.separator();
            ui.label(
                RichText::new("App ids are detected automatically.")
                    .size(10.0)
                    .color(DIM),
            );
        });
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            let can_start = !self.queue_run.running
                && !self.single_run.running
                && !self.login_running
                && !self.items.is_empty();
            if primary_button(ui, "Start Download", can_start, 140.0, 32.0) {
                start = true;
            }
            if ui
                .add_enabled(
                    self.queue_run.running,
                    Button::new("Stop").min_size(Vec2::new(64.0, 32.0)),
                )
                .clicked()
            {
                stop = true;
            }
            if ui
                .add_enabled(!self.queue_run.running, Button::new("Clear Finished"))
                .clicked()
            {
                clear = true;
            }
            ui.separator();
            ui.label(RichText::new(self.summary()).size(10.5).color(MUTED));
        });
        ui.add_space(8.0);

        if self.items.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new("The queue is empty")
                            .size(16.0)
                            .strong()
                            .color(MUTED),
                    );
                    ui.label(
                        RichText::new(
                            "Paste workshop links into the Item tab and use Add To Queue, or import a list.",
                        )
                        .size(11.0)
                        .color(DIM),
                    );
                });
            });
        } else {
            let mut actions = Vec::new();
            let queue_running = self.queue_run.running;
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for index in 0..self.items.len() {
                        let id = self.items[index].id;
                        let appid = self.items[index].appid;
                        let fileid = self.items[index].fileid;
                        let title = self.items[index].title.clone();
                        let description = self.items[index].description.clone();
                        let status = self.items[index].status.clone();
                        let detail = self.items[index].detail.clone();
                        let progress = self.items[index].progress;
                        let preview = self.previews.get(&fileid).cloned();
                        let active = is_active(&status);
                        let color = status_color(&status);
                        let can_up = !queue_running && index > 0;
                        let can_down = !queue_running && index + 1 < self.items.len();
                        let can_cancel = active || status == "queued";

                        Frame::NONE
                            .fill(CARD_BG)
                            .corner_radius(2.0)
                            .stroke(Stroke::new(1.0_f32, BORDER))
                            .inner_margin(9)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    draw_preview(ui, preview.as_deref(), 54.0, "");
                                    ui.add_space(4.0);
                                    ui.vertical(|ui| {
                                        ui.set_width((ui.available_width() - 330.0).max(150.0));
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(format!("{:>2}.", index + 1))
                                                    .monospace()
                                                    .size(10.5)
                                                    .color(DIM),
                                            );
                                            ui.add(
                                                Label::new(
                                                    RichText::new(&title)
                                                        .size(13.5)
                                                        .strong()
                                                        .color(TEXT),
                                                )
                                                .truncate(),
                                            );
                                        });
                                        ui.label(
                                            RichText::new(format!("App {appid}  ·  File {fileid}"))
                                                .monospace()
                                                .size(10.0)
                                                .color(MUTED),
                                        );
                                        let short = short_description(&description, 150);
                                        if !short.is_empty() {
                                            ui.add(
                                                Label::new(
                                                    RichText::new(short).size(10.5).color(DIM),
                                                )
                                                .truncate(),
                                            );
                                        }
                                    });
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if ui
                                            .add_enabled(
                                                !queue_running,
                                                Button::new("Remove").small(),
                                            )
                                            .clicked()
                                        {
                                            actions.push(RowAction::Remove(id));
                                        }
                                        if ui
                                            .add_enabled(can_down, Button::new("Dn").small())
                                            .clicked()
                                        {
                                            actions.push(RowAction::MoveDown(id));
                                        }
                                        if ui
                                            .add_enabled(can_up, Button::new("Up").small())
                                            .clicked()
                                        {
                                            actions.push(RowAction::MoveUp(id));
                                        }
                                        if ui
                                            .add_enabled(can_cancel, Button::new("Cancel").small())
                                            .clicked()
                                        {
                                            actions.push(RowAction::Cancel(id));
                                        }
                                        ui.add_space(6.0);
                                        ui.allocate_ui_with_layout(
                                            Vec2::new(138.0, 40.0),
                                            Layout::top_down(Align::RIGHT),
                                            |ui| {
                                                ui.label(
                                                    RichText::new(nice(&status))
                                                        .size(11.5)
                                                        .color(color),
                                                )
                                                .on_hover_text(if detail.is_empty() {
                                                    status.clone()
                                                } else {
                                                    detail.clone()
                                                });
                                                let value = if status == "done" {
                                                    1.0
                                                } else {
                                                    progress.unwrap_or(0.0)
                                                };
                                                let fill = if value <= 0.0 && !active {
                                                    Color32::TRANSPARENT
                                                } else {
                                                    color
                                                };
                                                ui.add(
                                                    ProgressBar::new(value)
                                                        .desired_width(128.0)
                                                        .desired_height(6.0)
                                                        .fill(fill)
                                                        .corner_radius(1.0)
                                                        .animate(active),
                                                );
                                            },
                                        );
                                    });
                                });
                            });
                        ui.add_space(3.0);
                    }
                });
            for action in actions {
                self.apply_row_action(action);
            }
        }

        if add {
            self.add_input_to_queue();
        }
        if import {
            self.import_list();
        }
        if export {
            self.export_queue();
        }
        if open_output {
            self.open_output();
        }
        if start {
            self.start_queue();
        }
        if stop {
            self.stop_queue();
        }
        if clear {
            self.clear_finished();
        }
    }

    fn draw_settings(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let busy = self.login_running || self.queue_run.running || self.single_run.running;
        let mut close = false;
        let mut save = false;
        let mut login_now = false;
        let mut browse = false;
        let mut open_folder = false;

        egui::Window::new("Account & Settings")
            .collapsible(false)
            .resizable(false)
            .default_width(470.0)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(RichText::new("Steam account").size(13.0).strong());
                ui.add(
                    TextEdit::singleline(&mut self.settings_draft.username)
                        .hint_text("Steam username")
                        .desired_width(f32::INFINITY),
                );
                ui.checkbox(
                    &mut self.settings_draft.anonymous,
                    "Use anonymous mode (no account)",
                );
                ui.label(
                    RichText::new(
                        "Anonymous mode downloads public items only. Signing in also caches a Steam Guard token for later downloads.",
                    )
                    .size(10.0)
                    .color(DIM),
                );

                ui.add_space(10.0);
                ui.label(RichText::new("Output folder").size(13.0).strong());
                ui.horizontal(|ui| {
                    ui.add(
                        TextEdit::singleline(&mut self.settings_draft.output_dir)
                            .desired_width(330.0),
                    );
                    if ui.button("Browse ...").clicked() {
                        browse = true;
                    }
                });
                ui.label(
                    RichText::new("Finished items are moved to APPID/FILEID folders here.")
                        .size(10.0)
                        .color(DIM),
                );

                ui.add_space(10.0);
                ui.label(RichText::new("Provider").size(13.0).strong());
                ComboBox::from_id_salt("settings-provider")
                    .selected_text(provider_label(&self.settings_draft.provider))
                    .width(150.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.settings_draft.provider,
                            "steamcmd".to_string(),
                            "SteamCMD",
                        );
                        ui.selectable_value(
                            &mut self.settings_draft.provider,
                            "ggntw".to_string(),
                            "GGNetwork",
                        );
                    });
                ui.checkbox(
                    &mut self.settings_draft.auto_open,
                    "Open the output folder when a download finishes",
                );

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let can_login = !self.settings_draft.anonymous
                            && !self.settings_draft.username.trim().is_empty()
                            && !busy;
                        if ui
                            .add_enabled(can_login, Button::new("Login now"))
                            .clicked()
                        {
                            login_now = true;
                        }
                        if ui.button("Open folder").clicked() {
                            open_folder = true;
                        }
                    });
                });
            });

        if browse {
            let mut dialog = rfd::FileDialog::new();
            if !self.settings_draft.output_dir.trim().is_empty() {
                dialog = dialog.set_directory(self.settings_draft.output_dir.trim());
            }
            if let Some(path) = dialog.pick_folder() {
                self.settings_draft.output_dir = path.to_string_lossy().into_owned();
            }
        }
        if open_folder {
            let path = PathBuf::from(self.settings_draft.output_dir.trim());
            let _ = fs::create_dir_all(&path);
            util::open_path(&path);
        }
        if save {
            self.apply_draft();
            self.settings_open = false;
            self.push_log("[settings] saved");
            self.flash("Settings saved.");
        }
        if login_now {
            self.apply_draft();
            self.settings_open = false;
            self.open_manual_login();
        }
        if close {
            self.settings_open = false;
        }
    }

    fn draw_game_picker(&mut self, ctx: &egui::Context) {
        if !self.game_picker_open {
            return;
        }
        let mut close = false;
        let mut chosen: Option<String> = None;
        egui::Window::new("Choose a game")
            .collapsible(false)
            .resizable(true)
            .default_width(430.0)
            .default_height(420.0)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.add(
                    TextEdit::singleline(&mut self.game_search)
                        .hint_text("Search by game name or app id ...")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(4.0);
                ScrollArea::vertical()
                    .max_height(340.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let results = games::search(&self.game_search, 300);
                        if results.is_empty() {
                            ui.label(
                                RichText::new("No matching game. Paste an app id instead.")
                                    .size(11.0)
                                    .color(MUTED),
                            );
                        }
                        for game in results {
                            let label = format!("{}  ({})", game.name, game.appid);
                            if ui.selectable_label(false, label).clicked() {
                                chosen = Some(format!("{} ({})", game.name, game.appid));
                            }
                        }
                    });
                ui.add_space(4.0);
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        if let Some(value) = chosen {
            self.game_input = value;
            self.game_picker_open = false;
        } else if close {
            self.game_picker_open = false;
        }
    }

    fn draw_login_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.login_dialog.take() else {
            return;
        };
        let mut submit = false;
        let mut cancel = false;
        let title = if dialog.manual {
            "Steam Login"
        } else {
            "Steam Login Required"
        };

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .default_width(380.0)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(if dialog.manual {
                        "Sign in to Steam."
                    } else {
                        "SteamCMD needs credentials before it can continue."
                    })
                    .size(12.0)
                    .color(TEXT),
                );
                if !dialog.suggested.is_empty() && dialog.manual {
                    ui.label(
                        RichText::new(format!("Account: {}", dialog.suggested))
                            .size(10.5)
                            .color(MUTED),
                    );
                }
                ui.add_space(6.0);
                ui.label(RichText::new("Username").size(10.5).color(MUTED));
                let user_response = ui.add(
                    TextEdit::singleline(&mut dialog.username)
                        .hint_text("Steam username")
                        .desired_width(f32::INFINITY),
                );
                ui.label(RichText::new("Password").size(10.5).color(MUTED));
                let password_response = ui.add(
                    TextEdit::singleline(&mut dialog.password)
                        .password(true)
                        .hint_text("Password")
                        .desired_width(f32::INFINITY),
                );
                if let Some(error) = &dialog.error {
                    ui.label(RichText::new(error).size(10.5).color(RED));
                }
                ui.label(
                    RichText::new(
                        "The password is passed straight to SteamCMD. A Steam Guard code is requested in the next dialog.",
                    )
                    .size(9.5)
                    .color(DIM),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if primary_button(ui, "Sign In", true, 84.0, 28.0) {
                        submit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
                let enter = ui.input(|input| input.key_pressed(egui::Key::Enter));
                if (user_response.lost_focus() || password_response.lost_focus()) && enter {
                    submit = true;
                }
            });

        if submit {
            let username = dialog.username.trim().to_string();
            if username.is_empty() {
                dialog.error = Some("Enter your Steam username.".to_string());
                self.login_dialog = Some(dialog);
                return;
            }
            let password = std::mem::take(&mut dialog.password);
            if let Some(reply) = dialog.reply.take() {
                self.settings.username = username.clone();
                self.settings_dirty = true;
                let _ = reply.send(BridgeReply::Login(Some((username, password))));
            } else {
                self.start_login(&username, &password);
            }
            self.login_dialog = None;
        } else if cancel {
            if let Some(reply) = dialog.reply.take() {
                let _ = reply.send(BridgeReply::Login(None));
            }
            self.login_dialog = None;
        } else {
            self.login_dialog = Some(dialog);
        }
    }

    fn draw_guard_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.guard_dialog.take() else {
            return;
        };
        let mut submit = false;
        let mut cancel = false;

        egui::Window::new("Steam Guard")
            .collapsible(false)
            .resizable(false)
            .default_width(360.0)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "Enter the Steam Guard code sent to your email or shown in the Steam mobile app.",
                    )
                    .size(12.0)
                    .color(TEXT),
                );
                ui.add_space(6.0);
                let response = ui.add(
                    TextEdit::singleline(&mut dialog.code)
                        .hint_text("Guard code")
                        .desired_width(f32::INFINITY),
                );
                if let Some(error) = &dialog.error {
                    ui.label(RichText::new(error).size(10.5).color(RED));
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if primary_button(ui, "Submit", true, 84.0, 28.0) {
                        submit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
                if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
            });

        if submit {
            let code = dialog.code.trim().replace(' ', "");
            if code.is_empty() {
                dialog.error = Some("Enter the Steam Guard code.".to_string());
                self.guard_dialog = Some(dialog);
                return;
            }
            let _ = dialog.reply.send(BridgeReply::Guard(Some(code)));
            self.guard_dialog = None;
        } else if cancel {
            let _ = dialog.reply.send(BridgeReply::Guard(None));
            self.guard_dialog = None;
        } else {
            self.guard_dialog = Some(dialog);
        }
    }
}

impl eframe::App for WorkshopApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.closing.is_none() && ctx.input(|input| input.viewport().close_requested()) {
            self.cancel_all();
            self.save_settings();
            self.save_window_size(ctx);
            self.closing = Some(Instant::now());
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if let Some(started) = self.closing {
            let busy = self.queue_run.running || self.single_run.running || self.login_running;
            if !busy || started.elapsed() > Duration::from_secs(3) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }

        self.drain_events(ctx);
        self.poll_clipboard(ctx);

        if self
            .notice
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(6))
        {
            self.notice = None;
        }
        if self.settings_dirty && self.settings_saved_at.elapsed() > Duration::from_secs(1) {
            self.save_settings();
        }

        self.draw_header(ctx);
        self.draw_status_bar(ctx);
        self.draw_log_panel(ctx);

        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(BG).inner_margin(Margin::symmetric(16, 10)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if tab_button(ui, self.tab == Tab::Single, "Item Download") {
                        self.tab = Tab::Single;
                    }
                    if tab_button(ui, self.tab == Tab::Batch, "Batch Download") {
                        self.tab = Tab::Batch;
                    }
                });
                ui.add_space(8.0);
                match self.tab {
                    Tab::Single => self.draw_single_tab(ui),
                    Tab::Batch => self.draw_batch_tab(ui),
                }
            });

        self.draw_settings(ctx);
        self.draw_game_picker(ctx);
        self.draw_login_dialog(ctx);
        self.draw_guard_dialog(ctx);

        let animating = self
            .previews
            .values()
            .any(|preview| matches!(preview.as_ref(), PreviewData::Gif { .. }));
        if animating
            || self.queue_run.running
            || self.single_run.running
            || self.login_running
            || self.single_resolving
            || self.closing.is_some()
            || self.notice.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(50));
        } else {
            ctx.request_repaint_after(Duration::from_millis(600));
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BG.to_normalized_gamma_f32()
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.cancel_all();
        self.save_settings();
    }
}

fn setup_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = CARD_BG;
    style.visuals.window_stroke = Stroke::new(1.0_f32, BORDER);
    style.visuals.window_corner_radius = CornerRadius::same(2);
    style.visuals.menu_corner_radius = CornerRadius::same(2);
    style.visuals.extreme_bg_color = INPUT_BG;
    style.visuals.faint_bg_color = CARD_ALT;
    style.visuals.code_bg_color = INPUT_BG;
    style.visuals.hyperlink_color = BLUE;
    style.visuals.warn_fg_color = YELLOW;
    style.visuals.error_fg_color = RED;
    style.visuals.selection.bg_fill = Color32::from_rgb(0x24, 0x4d, 0x69);
    style.visuals.selection.stroke = Stroke::new(1.0_f32, BLUE);
    style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    style.visuals.widgets.noninteractive.bg_fill = CARD_BG;
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(0x22, 0x34, 0x4a);
    style.visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(0x22, 0x34, 0x4a);
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    style.visuals.widgets.inactive.corner_radius = CornerRadius::same(2);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(0x2a, 0x47, 0x5e);
    style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x2a, 0x47, 0x5e);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, BLUE);
    style.visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, TEXT);
    style.visuals.widgets.hovered.corner_radius = CornerRadius::same(2);
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(0x34, 0x5d, 0x7c);
    style.visuals.widgets.active.weak_bg_fill = Color32::from_rgb(0x34, 0x5d, 0x7c);
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, BLUE);
    style.visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, TEXT);
    style.visuals.widgets.active.corner_radius = CornerRadius::same(2);
    style.visuals.widgets.open.bg_fill = Color32::from_rgb(0x2a, 0x47, 0x5e);
    style.visuals.widgets.open.weak_bg_fill = Color32::from_rgb(0x2a, 0x47, 0x5e);
    style.visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, BLUE);
    style.visuals.widgets.open.fg_stroke = Stroke::new(1.0_f32, TEXT);
    style.visuals.widgets.open.corner_radius = CornerRadius::same(2);
    style.visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(10.0, 5.0);
    style.spacing.interact_size.y = 26.0;
    ctx.set_style(style);
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let candidates = [
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
        "/usr/share/fonts/truetype/noto/NotoSansCJKsc-Regular.otf",
        "/usr/share/fonts/truetype/unifont/unifont.ttf",
    ];
    for path in candidates {
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        fonts.font_data.insert(
            "system-cjk".to_string(),
            Arc::new(FontData::from_owned(bytes)),
        );
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            if let Some(list) = fonts.families.get_mut(&family) {
                list.push("system-cjk".to_string());
            }
        }
        break;
    }
    ctx.set_fonts(fonts);
}

fn load_logo(ctx: &egui::Context) -> Option<TextureHandle> {
    let bytes = include_bytes!("../assets/workshopdl-linux-128.png");
    let image = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (width, height) = image.dimensions();
    let color =
        egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], image.as_raw());
    Some(ctx.load_texture("app-logo", color, TextureOptions::LINEAR))
}

fn spawn_resolve_worker(
    tx: Sender<AppEvent>,
    ctx: egui::Context,
    rx: Arc<Mutex<Receiver<ResolveJob>>>,
    name: String,
) {
    let _ = thread::Builder::new().name(name).spawn(move || loop {
        let job = match rx.lock() {
            Ok(guard) => match guard.recv() {
                Ok(job) => job,
                Err(_) => break,
            },
            Err(_) => break,
        };
        match job {
            ResolveJob::Item {
                purpose,
                fileid,
                appid,
            } => perform_resolve(purpose, fileid, appid, &tx, &ctx),
            ResolveJob::Titles { appid, fileids } => perform_titles(appid, fileids, &tx, &ctx),
        }
    });
}

fn spawn_preview_worker(
    tx: Sender<AppEvent>,
    ctx: egui::Context,
    rx: Arc<Mutex<Receiver<PreviewJob>>>,
    name: String,
) {
    let _ = thread::Builder::new().name(name).spawn(move || loop {
        let job = match rx.lock() {
            Ok(guard) => match guard.recv() {
                Ok(job) => job,
                Err(_) => break,
            },
            Err(_) => break,
        };
        match workshop_api::fetch_preview_bytes(&job.url) {
            Ok(bytes) => {
                let _ = tx.send(AppEvent::Preview {
                    fileid: job.fileid,
                    bytes,
                });
            }
            Err(_) => {
                let _ = tx.send(AppEvent::PreviewFailed { fileid: job.fileid });
            }
        }
        ctx.request_repaint();
    });
}

fn perform_resolve(
    purpose: ResolvePurpose,
    fileid: u64,
    appid_hint: Option<u64>,
    tx: &Sender<AppEvent>,
    ctx: &egui::Context,
) {
    let mut resolved = Resolved {
        purpose,
        fileid,
        appid: appid_hint,
        title: None,
        description: String::new(),
        preview_url: String::new(),
        is_collection: false,
        children: Vec::new(),
        resolve_error: None,
        detail_error: None,
    };

    let collection = workshop_api::get_collection_details(fileid);
    if collection.is_collection {
        resolved.is_collection = true;
        resolved.children = collection.children;
    } else if let Some(error) = collection.error {
        resolved.resolve_error = Some(error);
    }

    match workshop_api::get_published_file_details(&[fileid]) {
        Ok(items) if !items.is_empty() => {
            let item = &items[0];
            if let Some(appid) = workshop_api::item_appid(item) {
                resolved.appid = Some(appid);
            }
            resolved.title = item
                .get("title")
                .and_then(Value::as_str)
                .filter(|title| !title.trim().is_empty())
                .map(str::to_string);
            resolved.preview_url = item
                .get("preview_url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if workshop_api::item_result_code(item) != 1 {
                resolved.detail_error = workshop_api::item_error_message(item);
            }

            let mut api_description = item
                .get("file_description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            let short = api_description.trim().len() < 40;
            let scrape = !resolved.is_collection
                && (api_description.is_empty() || (purpose == ResolvePurpose::Single && short));
            if scrape {
                let scraped = workshop_api::get_item_description(fileid);
                if scraped.len() > api_description.len() {
                    api_description = scraped;
                }
            }
            resolved.description = workshop_api::clean_description(&api_description);
        }
        Ok(_) => {}
        Err(error) => {
            if resolved.resolve_error.is_none() {
                resolved.resolve_error = Some(error);
            }
        }
    }

    let _ = tx.send(AppEvent::Resolved(resolved));
    ctx.request_repaint();
}

fn perform_titles(
    appid: Option<u64>,
    fileids: Vec<u64>,
    tx: &Sender<AppEvent>,
    ctx: &egui::Context,
) {
    let mut metas = Vec::new();
    let mut scraped = 0usize;
    for chunk in fileids.chunks(100) {
        match workshop_api::get_published_file_details(chunk) {
            Ok(items) => {
                for item in items {
                    let Some(fileid) = item.get("publishedfileid").and_then(value_u64) else {
                        continue;
                    };
                    let title = item
                        .get("title")
                        .and_then(Value::as_str)
                        .filter(|title| !title.trim().is_empty())
                        .map(str::to_string);
                    let mut description = workshop_api::clean_description(
                        item.get("file_description")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    );
                    if description.is_empty() && scraped < 8 {
                        let candidate = workshop_api::get_item_description(fileid);
                        if !candidate.is_empty() {
                            description = workshop_api::clean_description(&candidate);
                            scraped += 1;
                        }
                    }
                    metas.push(ItemMeta {
                        fileid,
                        appid: workshop_api::item_appid(&item).or(appid),
                        title,
                        description,
                        preview_url: item
                            .get("preview_url")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    });
                }
            }
            Err(error) => {
                let _ = tx.send(AppEvent::Log(format!(
                    "[queue] title lookup failed: {error}"
                )));
            }
        }
    }
    let _ = tx.send(AppEvent::Titles(metas));
    ctx.request_repaint();
}

fn decode_preview(bytes: &[u8]) -> Option<DecodedPreview> {
    let format = image::guess_format(bytes).ok()?;
    if format == image::ImageFormat::Gif {
        let decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).ok()?;
        let mut frames = Vec::new();
        let mut delays = Vec::new();
        for frame in decoder.into_frames().take(60) {
            let Ok(frame) = frame else {
                continue;
            };
            let (numerator, denominator) = frame.delay().numer_denom_ms();
            let millis = if denominator == 0 {
                100
            } else {
                (u64::from(numerator) / u64::from(denominator)).clamp(20, 1000)
            };
            let buffer = frame.into_buffer();
            let image = image::DynamicImage::ImageRgba8(buffer).thumbnail(PREVIEW_MAX, PREVIEW_MAX);
            let rgba = image.to_rgba8();
            let (width, height) = rgba.dimensions();
            frames.push(egui::ColorImage::from_rgba_unmultiplied(
                [width as usize, height as usize],
                rgba.as_raw(),
            ));
            delays.push(Duration::from_millis(millis));
        }
        if frames.len() > 1 {
            return Some(DecodedPreview::Gif { frames, delays });
        }
        return frames.pop().map(DecodedPreview::Image);
    }

    let image = image::load_from_memory(bytes).ok()?;
    let image = image.thumbnail(PREVIEW_MAX, PREVIEW_MAX);
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    Some(DecodedPreview::Image(
        egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], rgba.as_raw()),
    ))
}

fn draw_preview(ui: &mut Ui, preview: Option<&PreviewData>, size: f32, placeholder: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(2), INPUT_BG);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(2),
        Stroke::new(1.0_f32, BORDER),
        egui::StrokeKind::Inside,
    );
    if let Some(data) = preview {
        if let Some(texture) = current_texture(data) {
            let uv = cover_uv(texture.size_vec2());
            ui.put(
                rect,
                egui::Image::new(texture)
                    .fit_to_exact_size(Vec2::splat(size))
                    .uv(uv)
                    .corner_radius(2.0),
            );
            return;
        }
    }
    let text = if placeholder.is_empty() {
        "No preview"
    } else {
        placeholder
    };
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(if size > 80.0 { 11.0 } else { 8.5 }),
        DIM,
    );
}

fn current_texture(data: &PreviewData) -> Option<&TextureHandle> {
    match data {
        PreviewData::Image(texture) => Some(texture),
        PreviewData::Gif {
            frames,
            delays,
            started,
        } => {
            if frames.is_empty() {
                return None;
            }
            let total: Duration = delays.iter().copied().sum();
            if total.is_zero() {
                return frames.first();
            }
            let total_seconds = total.as_secs_f64();
            if total_seconds <= 0.0 {
                return frames.first();
            }
            let elapsed_seconds = started.elapsed().as_secs_f64() % total_seconds;
            let elapsed = Duration::from_secs_f64(elapsed_seconds);
            let mut accumulated = Duration::ZERO;
            for (index, delay) in delays.iter().enumerate() {
                accumulated += *delay;
                if elapsed < accumulated {
                    return frames.get(index).or_else(|| frames.last());
                }
            }
            frames.last()
        }
        PreviewData::None => None,
    }
}

fn cover_uv(size: Vec2) -> egui::Rect {
    if size.x <= 0.0 || size.y <= 0.0 {
        return egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
    }
    let aspect = size.x / size.y;
    if aspect > 1.0 {
        let width = 1.0 / aspect;
        egui::Rect::from_min_max(
            egui::pos2((1.0 - width) / 2.0, 0.0),
            egui::pos2((1.0 + width) / 2.0, 1.0),
        )
    } else {
        let height = aspect;
        egui::Rect::from_min_max(
            egui::pos2(0.0, (1.0 - height) / 2.0),
            egui::pos2(1.0, (1.0 + height) / 2.0),
        )
    }
}

fn primary_button(ui: &mut Ui, text: &str, enabled: bool, min_width: f32, height: f32) -> bool {
    let font = FontId::proportional(13.0);
    let text_width = ui.fonts(|fonts| {
        fonts
            .layout_no_wrap(text.to_string(), font.clone(), ACCENT_DARK)
            .size()
            .x
    });
    let width = (text_width + 28.0).max(min_width);
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), sense);
    let hovered = enabled && response.hovered();
    let pressed = enabled && response.is_pointer_button_down_on();
    let fill = if !enabled {
        Color32::from_rgb(0x3f, 0x4d, 0x22)
    } else if pressed {
        Color32::from_rgb(0x86, 0xaa, 0x05)
    } else if hovered {
        Color32::from_rgb(0xba, 0xe2, 0x1e)
    } else {
        ACCENT
    };
    let text_color = if enabled {
        ACCENT_DARK
    } else {
        Color32::from_rgb(0x80, 0x8d, 0x62)
    };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(2), fill);
    painter.text(rect.center(), Align2::CENTER_CENTER, text, font, text_color);
    if hovered {
        painter.rect_stroke(
            rect,
            CornerRadius::same(2),
            Stroke::new(1.0_f32, Color32::from_rgb(0xd8, 0xf6, 0x72)),
            egui::StrokeKind::Inside,
        );
    }
    response.clicked()
}

fn tab_button(ui: &mut Ui, selected: bool, text: &str) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(132.0, 30.0), egui::Sense::click());
    let hovered = response.hovered();
    let fill = if selected {
        CARD_BG
    } else if hovered {
        CARD_ALT
    } else {
        Color32::TRANSPARENT
    };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(2), fill);
    if selected {
        painter.rect_stroke(
            rect,
            CornerRadius::same(2),
            Stroke::new(1.0_f32, BORDER),
            egui::StrokeKind::Inside,
        );
        painter.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(rect.left() + 12.0, rect.bottom() - 3.0),
                Vec2::new(rect.width() - 24.0, 2.0),
            ),
            CornerRadius::same(0),
            BLUE,
        );
    }
    let color = if selected {
        TEXT
    } else if hovered {
        BLUE
    } else {
        MUTED
    };
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(13.5),
        color,
    );
    response.clicked()
}

fn status_color(status: &str) -> Color32 {
    match status {
        "done" => GREEN,
        "failed" => RED,
        "cancelled" => DIM,
        "downloading" | "verifying" => BLUE,
        "queued" | "resolving" => YELLOW,
        _ => MUTED,
    }
}

fn is_active(status: &str) -> bool {
    matches!(status, "downloading" | "verifying")
}

fn nice(text: &str) -> String {
    let mut characters = text.chars();
    match characters.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), characters.as_str()),
        None => String::new(),
    }
}

fn provider_label(provider: &str) -> &'static str {
    if provider == "ggntw" {
        "GGNetwork"
    } else {
        "SteamCMD"
    }
}

fn short_description(text: &str, max_chars: usize) -> String {
    let cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() <= max_chars {
        return cleaned;
    }
    let mut out = cleaned.chars().take(max_chars).collect::<String>();
    out.push('…');
    out
}

fn is_progress_line(text: &str) -> bool {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix('[') else {
        return false;
    };
    let Some(end) = rest.find(']') else {
        return false;
    };
    let inner = &rest[..end];
    !inner.is_empty()
        && inner
            .chars()
            .all(|character| character.is_ascii_digit() || character == '%' || character == ' ')
}

fn appid_from_url(text: &str) -> Option<u64> {
    if text.contains("steamcommunity.com") && (text.contains("/app/") || text.contains("appid=")) {
        workshop_api::extract_app_id(text)
    } else {
        None
    }
}

fn parse_appid_fileid(line: &str) -> Option<(u64, u64)> {
    let (appid, fileid) = line.split_once(':')?;
    if appid.is_empty() || fileid.is_empty() {
        return None;
    }
    if !appid.chars().all(|character| character.is_ascii_digit())
        || !fileid.chars().all(|character| character.is_ascii_digit())
    {
        return None;
    }
    Some((appid.parse().ok()?, fileid.parse().ok()?))
}

fn value_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|number| u64::try_from(number).ok()))
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

fn item_on_disk(project: &Path, output_dir: &Path, appid: u64, fileid: u64) -> Option<PathBuf> {
    let mut candidates = vec![
        output_dir.join(appid.to_string()).join(fileid.to_string()),
        project
            .join("steamcmd")
            .join("Steam")
            .join("steamapps")
            .join("workshop")
            .join("content")
            .join(appid.to_string())
            .join(fileid.to_string()),
        project
            .join("steamcmd")
            .join("steamapps")
            .join("workshop")
            .join("content")
            .join(appid.to_string())
            .join(fileid.to_string()),
    ];
    if let Ok(entries) = fs::read_dir(project.join("steamcmd")) {
        for entry in entries.flatten() {
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                candidates.push(
                    entry
                        .path()
                        .join("steamapps")
                        .join("workshop")
                        .join("content")
                        .join(appid.to_string())
                        .join(fileid.to_string()),
                );
            }
        }
    }
    for candidate in candidates {
        if candidate.is_dir() && util::dir_has_files(&candidate) {
            return Some(candidate);
        }
        if candidate.is_file()
            && candidate
                .metadata()
                .map(|metadata| metadata.len() > 0)
                .unwrap_or(false)
        {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "fetches live Steam Community pages"]
    fn resolves_live_item_with_description() {
        let (tx, rx) = mpsc::channel();
        let ctx = egui::Context::default();
        perform_resolve(ResolvePurpose::Single, 3863, None, &tx, &ctx);
        let event = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("resolve timed out");
        let AppEvent::Resolved(resolved) = event else {
            panic!("unexpected event");
        };
        assert_eq!(resolved.appid, Some(440));
        assert!(
            resolved
                .title
                .as_deref()
                .unwrap_or_default()
                .contains("Work Accident"),
            "{:?}",
            resolved.title
        );
        assert!(
            resolved.description.contains("UPDATE"),
            "{}",
            resolved.description
        );
        assert!(!resolved.description.contains('<'), "markup leaked");
        assert!(!resolved.preview_url.is_empty());
    }
}
