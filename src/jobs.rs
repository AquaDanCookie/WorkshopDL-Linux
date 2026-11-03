use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Auth {
    pub anonymous: bool,
    pub username: String,
}

impl Auth {
    pub fn anonymous(&self) -> bool {
        self.anonymous
    }
}

#[derive(Debug, Clone)]
pub struct DownloadItem {
    pub id: u64,
    pub appid: u64,
    pub fileid: u64,
    pub title: String,
    pub cancel: Arc<AtomicBool>,
    pub kill: Arc<AtomicBool>,
}

impl DownloadItem {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::SeqCst)
            || self.kill.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn label(&self) -> String {
        let title = self.title.trim();
        if title.is_empty() {
            format!("item {}", self.fileid)
        } else {
            format!("{title} ({})", self.fileid)
        }
    }
}

#[derive(Debug, Clone)]
pub struct ItemResult {
    pub id: u64,
    pub fileid: u64,
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct RunResult {
    pub ok: bool,
    pub detail: String,
    pub results: Vec<ItemResult>,
}

impl RunResult {
    pub fn failure(detail: impl Into<String>) -> Self {
        Self {
            ok: false,
            detail: detail.into(),
            results: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoginResult {
    pub ok: bool,
    pub username: String,
    pub detail: String,
}

pub trait Events: Send + Sync {
    fn log(&self, text: String);
    fn status(&self, item_id: u64, status: &str, detail: String, progress: Option<f32>);
}

pub trait Bridge: Send + Sync {
    fn ask_login(&self, suggested: &str, cancel: &AtomicBool) -> Option<(String, String)>;
    fn ask_guard(&self, cancel: &AtomicBool) -> Option<String>;
}
