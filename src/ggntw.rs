use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::jobs::{DownloadItem, Events, ItemResult, RunResult};
use crate::util::{dir_has_files, human_bytes, move_path, recursive_copy};

const API_REQUEST: &str = "https://api.ggntw.com/steam.request";
const API_UPDATE: &str = "https://api.ggntw.com/steam.update";
const ITEM_URL: &str = "https://ggntw.com/steam/";
const DOWNLOAD_PAGE: &str = "https://ggntw.com/download/";
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const POLL_MAX_ROUNDS: usize = 120;
const EXTRA_DIRECT_ROUNDS: usize = 6;

const SERVERS: [(&str, &str); 3] = [
    ("Moscow", "https://cdn.ggntw.ru"),
    ("Netherlands", "https://cdn.ggntw.com"),
    ("Hong Kong", "https://hk-cdn.ggntw.com"),
];

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) WorkshopDL-Linux/2.0";

struct Mirror {
    label: &'static str,
    domain: &'static str,
    latency: Option<f64>,
}

pub struct GgNetworkManager {
    events: Arc<dyn Events>,
}

impl GgNetworkManager {
    pub fn new(events: Arc<dyn Events>) -> Self {
        Self { events }
    }

    pub fn run_queue(
        &self,
        items: &[DownloadItem],
        output_dir: &Path,
        cancel: &AtomicBool,
    ) -> RunResult {
        let mut results = Vec::new();
        for (index, item) in items.iter().enumerate() {
            if item.cancelled() || cancel.load(Ordering::SeqCst) {
                self.events
                    .status(item.id, "cancelled", "Queue stopped".to_string(), None);
                results.push(ItemResult {
                    id: item.id,
                    fileid: item.fileid,
                    status: "cancelled".to_string(),
                    detail: "Queue stopped".to_string(),
                });
                if cancel.load(Ordering::SeqCst) {
                    for rest in &items[index + 1..] {
                        if !rest.cancelled() {
                            self.events.status(
                                rest.id,
                                "cancelled",
                                "Queue stopped".to_string(),
                                None,
                            );
                            results.push(ItemResult {
                                id: rest.id,
                                fileid: rest.fileid,
                                status: "cancelled".to_string(),
                                detail: "Queue stopped".to_string(),
                            });
                        }
                    }
                    break;
                }
                continue;
            }

            self.events
                .log(format!("[gg] submitting {} to GGNetwork ...", item.label()));
            self.events.status(
                item.id,
                "downloading",
                "Submitting to GGNetwork ...".to_string(),
                None,
            );
            match self.download_item(item, output_dir, cancel) {
                Ok(dest) => {
                    self.events
                        .log(format!("[item] finished -> {}", dest.display()));
                    self.events.status(
                        item.id,
                        "done",
                        format!("Saved at {}", dest.display()),
                        Some(1.0),
                    );
                    results.push(ItemResult {
                        id: item.id,
                        fileid: item.fileid,
                        status: "done".to_string(),
                        detail: format!("Saved at {}", dest.display()),
                    });
                }
                Err(detail) if detail == "__cancelled__" => {
                    self.events
                        .status(item.id, "cancelled", "Cancelled".to_string(), None);
                    results.push(ItemResult {
                        id: item.id,
                        fileid: item.fileid,
                        status: "cancelled".to_string(),
                        detail: "Cancelled".to_string(),
                    });
                    if cancel.load(Ordering::SeqCst) {
                        for rest in &items[index + 1..] {
                            if !rest.cancelled() {
                                self.events.status(
                                    rest.id,
                                    "cancelled",
                                    "Queue stopped".to_string(),
                                    None,
                                );
                                results.push(ItemResult {
                                    id: rest.id,
                                    fileid: rest.fileid,
                                    status: "cancelled".to_string(),
                                    detail: "Queue stopped".to_string(),
                                });
                            }
                        }
                        break;
                    }
                }
                Err(detail) => {
                    self.events
                        .log(format!("[gg] item {}: {detail}", item.fileid));
                    self.events.status(item.id, "failed", detail.clone(), None);
                    results.push(ItemResult {
                        id: item.id,
                        fileid: item.fileid,
                        status: "failed".to_string(),
                        detail,
                    });
                }
            }
        }
        RunResult {
            ok: true,
            detail: String::new(),
            results,
        }
    }

    fn download_item(
        &self,
        item: &DownloadItem,
        output_dir: &Path,
        cancel: &AtomicBool,
    ) -> Result<PathBuf, String> {
        let data = self.submit(item)?;
        let url = self.wait_direct_link(&data, item, cancel)?;
        let key = extract_key(&url);
        let appid = data.get("game").and_then(value_u64).unwrap_or(item.appid);
        let size = data
            .get("size")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = data.get("name").and_then(Value::as_str).unwrap_or("");

        self.events.log(format!(
            "[gg] {}: '{}' (app {}, {}) -- link ready",
            item.fileid, name, appid, size
        ));

        let mirrors = self.ping_servers();
        for mirror in &mirrors {
            let latency = mirror
                .latency
                .map(|value| format!("{value:.0} ms"))
                .unwrap_or_else(|| "unreachable".to_string());
            self.events
                .log(format!("[gg] server {}: {latency}", mirror.label));
        }

        self.download_and_unpack(&key, item, appid, output_dir, cancel, &size, &mirrors)
    }

    fn submit(&self, item: &DownloadItem) -> Result<Value, String> {
        let target = format!(
            "https://steamcommunity.com/sharedfiles/filedetails/?id={}",
            item.fileid
        );
        let data = self.post_json(API_REQUEST, &json!({"url": target, "game": ""}))?;
        self.check_result(&data, item)?;
        Ok(data)
    }

    fn wait_direct_link(
        &self,
        data: &Value,
        item: &DownloadItem,
        cancel: &AtomicBool,
    ) -> Result<String, String> {
        let mut data = data.clone();
        let mut rounds = 0usize;
        let mut extra = 0usize;

        loop {
            if item.cancelled() || cancel.load(Ordering::SeqCst) {
                return Err("__cancelled__".to_string());
            }
            self.check_result(&data, item)?;
            let status = data.get("status").map(value_to_string).unwrap_or_default();
            let url = data
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            if status == "1" && url.starts_with(DOWNLOAD_PAGE) {
                return Ok(url);
            }

            if status == "1" && !url.is_empty() && extra < EXTRA_DIRECT_ROUNDS {
                extra += 1;
                self.events.log(format!(
                    "[gg] {}: ad-gated link ({}...), waiting for direct link ...",
                    item.fileid,
                    &url[..url.len().min(40)]
                ));
            } else if status == "0" || status == "2" {
                let position = data
                    .get("queue")
                    .and_then(|queue| queue.get("position"))
                    .map(value_to_string)
                    .unwrap_or_default();
                let detail = if position.is_empty() {
                    "In GGNetwork queue ...".to_string()
                } else {
                    format!("In GGNetwork queue (position {position}) ...")
                };
                self.events.status(item.id, "downloading", detail, None);
            } else if status == "3" {
                return Err(format!(
                    "GGNetwork reported an error preparing this item (status 3). Manual page: {ITEM_URL}{}",
                    item.fileid
                ));
            } else {
                return Err(format!(
                    "GGNetwork returned status '{status}' for {}. Manual page: {ITEM_URL}{}",
                    item.fileid, item.fileid
                ));
            }

            rounds += 1;
            if rounds > POLL_MAX_ROUNDS {
                return Err(format!(
                    "GGNetwork did not finish preparing the item in time. Manual page: {ITEM_URL}{}",
                    item.fileid
                ));
            }
            std::thread::sleep(POLL_INTERVAL);
            let id = data.get("id").cloned().unwrap_or(Value::Null);
            data = self.post_json(API_UPDATE, &json!({"id": id}))?;
        }
    }

    fn check_result(&self, data: &Value, item: &DownloadItem) -> Result<(), String> {
        let raw = data.get("result").cloned().unwrap_or(json!(1));
        let result = value_i64(&raw).ok_or_else(|| {
            format!(
                "GGNetwork returned an unexpected result value {raw}. Manual page: {ITEM_URL}{}",
                item.fileid
            )
        })?;
        if result == 1 {
            return Ok(());
        }
        let reason = match result {
            0 => "item not found on GGNetwork".to_string(),
            3 => "GGNetwork server error for this item".to_string(),
            4 => "file too large for GGNetwork".to_string(),
            10 => "GGNetwork requires an account login for this item".to_string(),
            11 => "daily request limit reached on GGNetwork".to_string(),
            12 => "item is blocked (NSFW) on GGNetwork".to_string(),
            15 => "this item requires GGNetwork premium".to_string(),
            other => format!("unexpected result code {other}"),
        };
        Err(format!(
            "{reason} (result {result}). Manual page: {ITEM_URL}{}",
            item.fileid
        ))
    }

    fn ping_servers(&self) -> Vec<Mirror> {
        SERVERS
            .iter()
            .map(|(label, domain)| Mirror {
                label,
                domain,
                latency: ping_domain(domain),
            })
            .collect()
    }

    fn download_and_unpack(
        &self,
        key: &str,
        item: &DownloadItem,
        appid: u64,
        output_dir: &Path,
        cancel: &AtomicBool,
        size: &str,
        mirrors: &[Mirror],
    ) -> Result<PathBuf, String> {
        if appid == 0 {
            return Err("App ID unknown for this item".to_string());
        }
        let out_dir = output_dir
            .join(appid.to_string())
            .join(item.fileid.to_string());
        fs::create_dir_all(&out_dir)
            .map_err(|error| format!("Could not create output folder: {error}"))?;
        let tmp_zip = out_dir.join(format!(".ggntw-{}.part", item.fileid));

        let mut ordered: Vec<&Mirror> = mirrors.iter().collect();
        ordered.sort_by(|a, b| {
            let a_none = a.latency.is_none();
            let b_none = b.latency.is_none();
            a_none.cmp(&b_none).then_with(|| {
                a.latency
                    .unwrap_or(0.0)
                    .total_cmp(&b.latency.unwrap_or(0.0))
            })
        });

        let mut last_detail = "no mirror served the file".to_string();
        let mut downloaded = false;
        for mirror in ordered {
            if item.cancelled() || cancel.load(Ordering::SeqCst) {
                let _ = fs::remove_file(&tmp_zip);
                return Err("__cancelled__".to_string());
            }
            self.events
                .log(format!("[gg] trying mirror {} ...", mirror.label));
            match self.stream_file(
                mirror.domain,
                key,
                &tmp_zip,
                item,
                cancel,
                size,
                mirror.label,
            ) {
                Ok(()) => {
                    downloaded = true;
                    break;
                }
                Err(error) if error.starts_with("__mirror__") => {
                    last_detail = error.trim_start_matches("__mirror__").to_string();
                }
                Err(error) => {
                    let _ = fs::remove_file(&tmp_zip);
                    return Err(error);
                }
            }
        }
        if !downloaded {
            let _ = fs::remove_file(&tmp_zip);
            return Err(format!("Could not download from GGNetwork: {last_detail}"));
        }

        let archive = File::open(&tmp_zip)
            .map_err(|error| format!("Could not open the downloaded archive: {error}"))?;
        let mut zip = zip::ZipArchive::new(archive)
            .map_err(|error| format!("Downloaded file is not a valid zip archive: {error}"))?;
        for index in 0..zip.len() {
            let mut entry = zip
                .by_index(index)
                .map_err(|error| format!("Could not read the downloaded archive: {error}"))?;
            if entry.is_symlink() {
                continue;
            }
            if !entry.is_dir() {
                let mut sink = std::io::sink();
                std::io::copy(&mut entry, &mut sink)
                    .map_err(|error| format!("Downloaded archive is corrupted: {error}"))?;
            }
        }

        let staging = out_dir.join(".ggntw-extract");
        if staging.exists() {
            let _ = fs::remove_dir_all(&staging);
        }
        fs::create_dir_all(&staging)
            .map_err(|error| format!("Could not create extraction folder: {error}"))?;

        if let Err(error) = extract_zip(&tmp_zip, &staging) {
            let _ = fs::remove_dir_all(&staging);
            let _ = fs::remove_file(&tmp_zip);
            return Err(format!("Could not unpack the downloaded archive: {error}"));
        }

        let mut root = staging.clone();
        if let Ok(entries) = fs::read_dir(&staging) {
            let entries: Vec<_> = entries.flatten().collect();
            if entries.len() == 1
                && entries[0]
                    .file_type()
                    .map(|kind| kind.is_dir())
                    .unwrap_or(false)
            {
                root = entries[0].path();
            }
        }

        if let Ok(entries) = fs::read_dir(&root) {
            for entry in entries.flatten() {
                let target = out_dir.join(entry.file_name());
                if target.exists() {
                    if target.is_dir() {
                        let _ = fs::remove_dir_all(&target);
                    } else {
                        let _ = fs::remove_file(&target);
                    }
                }
                if let Err(error) = move_path(&entry.path(), &target) {
                    let _ = fs::remove_dir_all(&staging);
                    let _ = fs::remove_file(&tmp_zip);
                    return Err(format!("Could not move extracted files: {error}"));
                }
            }
        }
        let _ = fs::remove_dir_all(&staging);
        let _ = fs::remove_file(&tmp_zip);
        Ok(out_dir)
    }

    fn stream_file(
        &self,
        domain: &str,
        key: &str,
        tmp_zip: &Path,
        item: &DownloadItem,
        cancel: &AtomicBool,
        size: &str,
        label: &str,
    ) -> Result<(), String> {
        let url = format!("{domain}/{key}");
        let response = match ureq::get(&url)
            .timeout(Duration::from_secs(180))
            .set("User-Agent", USER_AGENT)
            .set("Referer", "https://ggntw.com/steam")
            .set("Accept", "application/zip,*/*")
            .call()
        {
            Ok(response) => response,
            Err(ureq::Error::Status(code, _)) if code >= 400 => {
                return Err(format!("__mirror__{label} has no file (HTTP {code})"));
            }
            Err(error) => return Err(format!("GGNetwork file download failed: {error}")),
        };
        let content_type = response
            .header("Content-Type")
            .unwrap_or("")
            .to_ascii_lowercase();
        if content_type.starts_with("text/html") {
            return Err(format!(
                "__mirror__{label} has no file (redirected to an error page)"
            ));
        }

        let mut reader = response.into_reader();
        let mut file = File::create(tmp_zip)
            .map_err(|error| format!("Could not create the download file: {error}"))?;
        let mut total = 0u64;
        let mut buffer = vec![0u8; 1 << 20];
        let started = Instant::now();
        let mut last_update = Instant::now();

        loop {
            if item.cancelled() || cancel.load(Ordering::SeqCst) {
                let _ = fs::remove_file(tmp_zip);
                return Err("__cancelled__".to_string());
            }
            let read = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) => {
                    let _ = fs::remove_file(tmp_zip);
                    return Err(format!("GGNetwork download failed: {error}"));
                }
            };
            file.write_all(&buffer[..read])
                .map_err(|error| format!("Could not write the download: {error}"))?;
            total += read as u64;

            if last_update.elapsed() >= Duration::from_secs(1) {
                last_update = Instant::now();
                let elapsed = started.elapsed().as_secs_f64().max(0.1);
                let rate = human_bytes((total as f64 / elapsed) as u64);
                let progress = if !size.is_empty() {
                    format!("{} / {}", human_bytes(total), size)
                } else {
                    human_bytes(total)
                };
                self.events.status(
                    item.id,
                    "downloading",
                    format!("GGNetwork ({label}) ... {progress} ({rate}/s)"),
                    None,
                );
            }
        }

        if total == 0 {
            let _ = fs::remove_file(tmp_zip);
            return Err(format!("__mirror__{label} returned an empty response"));
        }
        Ok(())
    }

    fn post_json(&self, url: &str, payload: &Value) -> Result<Value, String> {
        let response = ureq::post(url)
            .timeout(Duration::from_secs(60))
            .set("Content-Type", "application/json")
            .set("User-Agent", USER_AGENT)
            .set("Origin", "https://ggntw.com")
            .set("Referer", "https://ggntw.com/steam")
            .send_json(payload.clone())
            .map_err(|error| format!("GGNetwork request failed: {error}"))?;
        let mut text = String::new();
        response
            .into_reader()
            .take(4 * 1024 * 1024)
            .read_to_string(&mut text)
            .map_err(|error| format!("GGNetwork read failed: {error}"))?;
        if text.trim().is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&text)
            .map_err(|error| format!("GGNetwork returned invalid JSON: {error}"))
    }
}

fn value_u64(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_u64() {
        return Some(number);
    }
    if let Some(number) = value.as_i64() {
        return u64::try_from(number).ok();
    }
    value.as_str().and_then(|text| text.parse().ok())
}

fn value_i64(value: &Value) -> Option<i64> {
    if let Some(number) = value.as_i64() {
        return Some(number);
    }
    if let Some(number) = value.as_u64() {
        return i64::try_from(number).ok();
    }
    value.as_str().and_then(|text| text.parse().ok())
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        _ => String::new(),
    }
}

fn extract_key(url: &str) -> String {
    let path = url
        .split_once("download/")
        .map(|(_, rest)| rest)
        .unwrap_or(url);
    let path = path.split(['?', '#']).next().unwrap_or(path);
    path.trim_matches('/').trim_end_matches(".html").to_string()
}

fn ping_domain(domain: &str) -> Option<f64> {
    let mut best: Option<f64> = None;
    for _ in 0..3 {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_millis())
            .unwrap_or(0);
        let url = format!("{domain}/ping.bin?{stamp}");
        let started = Instant::now();
        if let Ok(response) = ureq::get(&url)
            .timeout(Duration::from_secs(4))
            .set("User-Agent", USER_AGENT)
            .call()
        {
            let _ = response.into_reader().take(32).read_to_end(&mut Vec::new());
            let elapsed = started.elapsed().as_secs_f64() * 1000.0;
            best = Some(best.map_or(elapsed, |current| current.min(elapsed)));
        }
    }
    best
}

fn extract_zip(path: &Path, target: &Path) -> Result<(), String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|error| error.to_string())?;
    let target = target
        .canonicalize()
        .map_err(|error| format!("Could not resolve extraction folder: {error}"))?;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        if entry.is_symlink() {
            continue;
        }
        let Some(relative) = entry.enclosed_name() else {
            return Err(format!("Unsafe path in archive: {:?}", entry.name()));
        };
        let destination = target.join(relative);
        if !destination.starts_with(&target) {
            return Err(format!("Unsafe path in archive: {:?}", entry.name()));
        }
        if entry.is_dir() {
            fs::create_dir_all(&destination).map_err(|error| error.to_string())?;
            continue;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let mut output = File::create(&destination).map_err(|error| error.to_string())?;
        std::io::copy(&mut entry, &mut output).map_err(|error| error.to_string())?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&destination, fs::Permissions::from_mode(mode));
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn copy_directory(source: &Path, dest: &Path) -> Result<(), String> {
    recursive_copy(source, dest).map_err(|error| error.to_string())?;
    if dir_has_files(dest) {
        Ok(())
    } else {
        Err("No files were extracted".to_string())
    }
}
