use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use regex::Regex;

use crate::jobs::{Auth, Bridge, DownloadItem, Events, ItemResult, LoginResult, RunResult};
use crate::util::{dir_has_files, move_path, strip_ansi};

const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);
const COMMAND_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const HEARTBEAT: Duration = Duration::from_secs(30);

fn guard_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)steam guard\s*code|guard code|code from your (?:email|phone)|two.factor|mobile auth",
        )
        .unwrap()
    })
}

fn password_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(?:^|\s)password\s*:").unwrap())
}

fn login_ok_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)logged in ok|logging in user[^\n]*ok|connecting anonymously[^\n]*ok|waiting for user info[^\n]*ok",
        )
        .unwrap()
    })
}

fn login_progress_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)logging in user|connecting anonymously|waiting for client config|waiting for user info",
        )
        .unwrap()
    })
}

fn auth_fail_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)login failure|invalid password|ratelimit|rate limit|account.*(?:denied|disabled|locked|restricted)|not authorized|too many (?:login|attempts)",
        )
        .unwrap()
    })
}

fn success_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)success[^\n]*downloaded item\s+(\d+)\s+to\s+"([^"]+)"(?:\s*\((\d+)\s*bytes\))?"#,
        )
        .unwrap()
    })
}

fn downloading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)downloading item\s+(\d+)").unwrap())
}

fn percent_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[\s*(\d{1,3})%\]").unwrap())
}

fn progress_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)progress[:\s]+([0-9]+(?:\.[0-9]+)?)").unwrap())
}

#[derive(Debug)]
enum SessionEvent {
    Line(String),
    Tail(String),
    Prompt,
    Eof,
}

pub struct SteamCmdManager {
    base: PathBuf,
    events: Arc<dyn Events>,
    bridge: Arc<dyn Bridge>,
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<SessionEvent>,
    reader: Option<thread::JoinHandle<()>>,
    alive: bool,
}

enum Outcome {
    Success { path: Option<String>, bytes: u64 },
    Failed { detail: String, retryable: bool },
    Killed,
    SessionLost,
}

impl SteamCmdManager {
    pub fn new(base: PathBuf, events: Arc<dyn Events>, bridge: Arc<dyn Bridge>) -> Self {
        Self {
            base,
            events,
            bridge,
        }
    }

    fn launcher(&self) -> Option<PathBuf> {
        let script = self.base.join("steamcmd.sh");
        if script.is_file() {
            return Some(script);
        }
        for name in ["linux64", "linux32"] {
            let program = self.base.join(name).join("steamcmd");
            if program.is_file() {
                return Some(program);
            }
        }
        None
    }

    pub fn ensure_installed(&self, cancel: &AtomicBool) -> bool {
        if self.launcher().is_some() {
            self.events
                .log("Using existing steamcmd installation.".to_string());
            return true;
        }

        self.events
            .log("SteamCMD missing -- downloading the installer ...".to_string());
        if let Err(error) = fs::create_dir_all(&self.base) {
            self.events
                .log(format!("Could not create steamcmd folder: {error}"));
            return false;
        }
        let url = "https://steamcdn-a.akamaihd.net/client/installer/steamcmd_linux.tar.gz";
        let tar_path = self.base.join("steamcmd_linux.tar.gz");

        let response = match ureq::get(url).call() {
            Ok(response) => response,
            Err(error) => {
                self.events
                    .log(format!("Failed to download steamcmd: {error}"));
                return false;
            }
        };
        let mut reader = response.into_reader();
        let mut file = match fs::File::create(&tar_path) {
            Ok(file) => file,
            Err(error) => {
                self.events
                    .log(format!("Failed to create installer file: {error}"));
                return false;
            }
        };
        let mut buffer = vec![0u8; 1 << 20];
        let mut total = 0u64;
        loop {
            if cancel.load(Ordering::SeqCst) {
                self.events.log("Installation cancelled.".to_string());
                let _ = fs::remove_file(&tar_path);
                return false;
            }
            let read = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) => {
                    self.events
                        .log(format!("Failed to download steamcmd: {error}"));
                    return false;
                }
            };
            if file.write_all(&buffer[..read]).is_err() {
                self.events
                    .log("Failed to write the steamcmd installer.".to_string());
                return false;
            }
            total += read as u64;
            if total % (8 << 20) < read as u64 {
                self.events.log(format!(
                    "Downloading steamcmd ... {}",
                    crate::util::human_bytes(total)
                ));
            }
        }

        let status = Command::new("tar")
            .arg("-xzf")
            .arg(&tar_path)
            .arg("-C")
            .arg(&self.base)
            .status();
        let _ = fs::remove_file(&tar_path);
        match status {
            Ok(status) if status.success() => {}
            Ok(status) => {
                self.events.log(format!(
                    "Could not unpack steamcmd (tar exited with {status})."
                ));
                return false;
            }
            Err(error) => {
                self.events.log(format!("Could not run tar: {error}"));
                return false;
            }
        }

        let Some(launcher) = self.launcher() else {
            self.events
                .log("SteamCMD installer finished but no steamcmd binary was found.".to_string());
            return false;
        };
        self.events
            .log("Booting steamcmd for the first-run update ...".to_string());
        let mut command = Command::new(launcher);
        command
            .arg("+quit")
            .current_dir(&self.base)
            .env("HOME", &self.base)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let Ok(mut child) = command.spawn() else {
            self.events.log("Could not start steamcmd.".to_string());
            return false;
        };
        let deadline = Instant::now() + Duration::from_secs(240);
        loop {
            if cancel.load(Ordering::SeqCst) {
                let _ = child.kill();
                return false;
            }
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(200));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    self.events
                        .log("steamcmd first-run update timed out.".to_string());
                    return false;
                }
            }
        }
        self.launcher().is_some()
    }

    fn spawn(&self, args: &[String]) -> Result<Session, String> {
        let launcher = self
            .launcher()
            .ok_or_else(|| "steamcmd binary missing".to_string())?;
        let mut command = Command::new(&launcher);
        command
            .args(args)
            .current_dir(&self.base)
            .env("HOME", &self.base)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        #[cfg(target_os = "linux")]
        unsafe {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong);
                Ok(())
            });
        }

        let mut child = command
            .spawn()
            .map_err(|error| format!("Could not start steamcmd: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "steamcmd stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "steamcmd stdout unavailable".to_string())?;

        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || reader_loop(stdout, tx));
        Ok(Session {
            child,
            stdin,
            rx,
            reader: Some(reader),
            alive: true,
        })
    }

    fn acquire_session(&self, auth: &Auth, cancel: &AtomicBool) -> Result<Session, String> {
        if auth.anonymous() {
            let args = vec!["+login".to_string(), "anonymous".to_string()];
            let mut session = self.spawn(&args)?;
            session.wait_for_prompt(self, cancel)?;
            self.events
                .log("[session] logged in (anonymous)".to_string());
            return Ok(session);
        }

        let mut username = auth.username.trim().to_string();
        if username.is_empty() {
            let Some((user, password)) = self.bridge.ask_login("", cancel) else {
                return Err("Login cancelled".to_string());
            };
            let args = vec!["+login".to_string(), user.clone(), password];
            let mut session = self.spawn(&args)?;
            session.wait_for_prompt(self, cancel)?;
            self.events
                .log(format!("[session] logged in as {user} (full)"));
            return Ok(session);
        }

        let args = vec!["+login".to_string(), username.clone()];
        match self.spawn(&args) {
            Ok(mut session) => match session.wait_for_prompt(self, cancel) {
                Ok(()) => {
                    self.events
                        .log("[session] logged in (cached credentials)".to_string());
                    return Ok(session);
                }
                Err(detail) => {
                    self.events
                        .log(format!("[session] cached login did not complete: {detail}"));
                    session.teardown();
                }
            },
            Err(error) => {
                self.events
                    .log(format!("[session] could not start steamcmd: {error}"));
            }
        }

        self.events
            .log("[session] asking for Steam credentials ...".to_string());
        let Some((user, password)) = self.bridge.ask_login(&username, cancel) else {
            return Err("Login cancelled".to_string());
        };
        username = user.clone();
        let args = vec!["+login".to_string(), user, password];
        let mut session = self.spawn(&args)?;
        session.wait_for_prompt(self, cancel)?;
        self.events
            .log(format!("[session] logged in as {username} (full)"));
        Ok(session)
    }

    pub fn login(&self, username: &str, password: &str, cancel: &AtomicBool) -> LoginResult {
        if username.trim().is_empty() {
            return LoginResult {
                ok: false,
                username: username.to_string(),
                detail: "No username given".to_string(),
            };
        }
        if !self.ensure_installed(cancel) {
            return LoginResult {
                ok: false,
                username: username.to_string(),
                detail: "SteamCMD could not be set up".to_string(),
            };
        }
        let args = vec![
            "+login".to_string(),
            username.trim().to_string(),
            password.to_string(),
        ];
        let result = (|| -> Result<(), String> {
            let mut session = self.spawn(&args)?;
            session.wait_for_prompt(self, cancel)?;
            session.teardown();
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.events
                    .log(format!("[session] logged in as {username} (full)"));
                LoginResult {
                    ok: true,
                    username: username.trim().to_string(),
                    detail: "Logged in".to_string(),
                }
            }
            Err(detail) => {
                self.events.log(format!("[session] login failed: {detail}"));
                LoginResult {
                    ok: false,
                    username: username.trim().to_string(),
                    detail,
                }
            }
        }
    }

    fn download_item(&self, session: &mut Session, item: &DownloadItem) -> Outcome {
        while let Ok(event) = session.rx.try_recv() {
            match event {
                SessionEvent::Line(line) => self.events.log(line),
                SessionEvent::Tail(line) => {
                    if !line.trim().is_empty() {
                        self.events.log(line);
                    }
                }
                _ => {}
            }
        }

        let command = format!("workshop_download_item {} {}", item.appid, item.fileid);
        self.events.log(format!("[cmd] {command}"));
        if let Err(error) = session.send(&command) {
            self.events.log(format!("[session] {error}"));
            return Outcome::SessionLost;
        }

        let mut seen_downloading = false;
        let mut last_activity = Instant::now();
        let mut last_heartbeat = Instant::now();
        let sent_at = Instant::now();
        let mut prompt_grace = false;

        loop {
            if item.cancelled() {
                self.events
                    .log(format!("[item] cancelling download {}", item.fileid));
                session.hard_stop();
                return Outcome::Killed;
            }

            match session.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(SessionEvent::Line(line)) => {
                    last_activity = Instant::now();
                    self.events.log(line.clone());

                    if let Some(caps) = success_re().captures(&line) {
                        let fileid = caps
                            .get(1)
                            .and_then(|value| value.as_str().parse::<u64>().ok());
                        if fileid == Some(item.fileid) || fileid.is_none() {
                            return Outcome::Success {
                                path: caps.get(2).map(|value| value.as_str().to_string()),
                                bytes: caps
                                    .get(3)
                                    .and_then(|value| value.as_str().parse::<u64>().ok())
                                    .unwrap_or(0),
                            };
                        }
                    }

                    let lower = line.to_lowercase();
                    if lower.contains("error!") || lower.starts_with("error") {
                        let retryable = lower.contains("staged file validation failed")
                            || lower.contains("missing update files")
                            || lower.contains("timed out")
                            || lower.contains("connection")
                            || lower.contains("http");
                        let detail = if lower.contains("access denied") {
                            format!(
                                "{line} -- Steam refused access. Log in with an account that owns the game, \
                                 or try the GGNetwork provider."
                            )
                        } else if lower.contains("no subscription") {
                            format!("{line} -- this account does not own the required game.")
                        } else {
                            line.clone()
                        };
                        return Outcome::Failed { detail, retryable };
                    }

                    if downloading_re().is_match(&line) {
                        seen_downloading = true;
                        self.events.status(
                            item.id,
                            "downloading",
                            "SteamCMD is downloading the item ...".to_string(),
                            None,
                        );
                    }

                    if let Some(caps) = percent_re().captures(&line) {
                        if let Ok(percent) = caps[1].parse::<f32>() {
                            self.events.status(
                                item.id,
                                "downloading",
                                format!("Downloading ... {percent:.0}%"),
                                Some((percent / 100.0).clamp(0.0, 1.0)),
                            );
                        }
                    } else if let Some(caps) = progress_re().captures(&line) {
                        self.events.status(
                            item.id,
                            "downloading",
                            format!("Downloading ... {}", &caps[1]),
                            None,
                        );
                    }

                    if seen_downloading && lower.contains("already up to date") {
                        return Outcome::Success {
                            path: None,
                            bytes: 0,
                        };
                    }
                }
                Ok(SessionEvent::Tail(_)) => {
                    last_activity = Instant::now();
                }
                Ok(SessionEvent::Prompt) => {
                    if seen_downloading {
                        return Outcome::Failed {
                            detail: "steamcmd returned to its prompt without reporting a download result."
                                .to_string(),
                            retryable: true,
                        };
                    }
                    if !prompt_grace && sent_at.elapsed() < Duration::from_millis(800) {
                        // A duplicate login prompt can still be queued when the command is sent.
                        prompt_grace = true;
                        continue;
                    }
                    return Outcome::Failed {
                        detail: "steamcmd did not start the download (returned to its prompt)."
                            .to_string(),
                        retryable: true,
                    };
                }
                Ok(SessionEvent::Eof) => {
                    session.alive = false;
                    return Outcome::SessionLost;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    session.alive = false;
                    return Outcome::SessionLost;
                }
            }

            if last_heartbeat.elapsed() >= HEARTBEAT {
                last_heartbeat = Instant::now();
                let idle = last_activity.elapsed().as_secs();
                self.events.log(format!(
                    "[steamcmd] still working on {} (no new output for {}s)",
                    item.fileid, idle
                ));
                self.events.status(
                    item.id,
                    "downloading",
                    format!("SteamCMD is still working (no output for {idle}s) ..."),
                    None,
                );
            }

            if last_activity.elapsed() >= COMMAND_IDLE_TIMEOUT {
                self.events.log(format!(
                    "[steamcmd] no output for {} minutes -- restarting the session",
                    COMMAND_IDLE_TIMEOUT.as_secs() / 60
                ));
                session.hard_stop();
                return Outcome::Failed {
                    detail: "SteamCMD stopped responding while downloading".to_string(),
                    retryable: true,
                };
            }
        }
    }

    pub fn run_queue(
        &self,
        items: &[DownloadItem],
        auth: &Auth,
        output_dir: &Path,
        cancel: &AtomicBool,
    ) -> RunResult {
        if items.is_empty() {
            return RunResult {
                ok: true,
                detail: String::new(),
                results: Vec::new(),
            };
        }
        if !self.ensure_installed(cancel) {
            return RunResult::failure("SteamCMD could not be set up");
        }

        let mut session = match self.acquire_session(auth, cancel) {
            Ok(session) => Some(session),
            Err(detail) => return RunResult::failure(detail),
        };
        let mut results: Vec<ItemResult> = Vec::new();
        let mut index = 0usize;

        while index < items.len() {
            let item = &items[index];
            index += 1;

            if item.cancelled() || cancel.load(Ordering::SeqCst) {
                let detail = if cancel.load(Ordering::SeqCst) {
                    "Queue stopped"
                } else {
                    "Cancelled"
                };
                self.events
                    .status(item.id, "cancelled", detail.to_string(), None);
                results.push(ItemResult {
                    id: item.id,
                    fileid: item.fileid,
                    status: "cancelled".to_string(),
                    detail: detail.to_string(),
                });
                continue;
            }

            let mut attempts = 0usize;
            loop {
                attempts += 1;
                if session.is_none() {
                    match self.acquire_session(auth, cancel) {
                        Ok(new_session) => session = Some(new_session),
                        Err(detail) => {
                            self.events
                                .log(format!("[session] could not reconnect: {detail}"));
                            for rest in &items[index - 1..] {
                                if !rest.cancelled() && !cancel.load(Ordering::SeqCst) {
                                    self.events.status(
                                        rest.id,
                                        "failed",
                                        format!("Session lost: {detail}"),
                                        None,
                                    );
                                    results.push(ItemResult {
                                        id: rest.id,
                                        fileid: rest.fileid,
                                        status: "failed".to_string(),
                                        detail: format!("Session lost: {detail}"),
                                    });
                                }
                            }
                            self.teardown_session(&mut session);
                            return RunResult {
                                ok: true,
                                detail: String::new(),
                                results,
                            };
                        }
                    }
                }

                self.events.status(
                    item.id,
                    "downloading",
                    format!(
                        "Downloading {} (app {}) via SteamCMD ...",
                        item.label(),
                        item.appid
                    ),
                    None,
                );

                let outcome = self.download_item(session.as_mut().unwrap(), item);
                match outcome {
                    Outcome::Success { path, bytes } => {
                        self.events.status(
                            item.id,
                            "verifying",
                            "Downloaded; moving the item to the output folder ...".to_string(),
                            Some(1.0),
                        );
                        let source = self.resolve_source(
                            item.appid,
                            item.fileid,
                            path.as_deref(),
                            output_dir,
                        );
                        match self.move_output(
                            item.appid,
                            item.fileid,
                            output_dir,
                            source.as_deref(),
                        ) {
                            Ok(dest) => {
                                if bytes > 0 {
                                    self.events.log(format!(
                                        "[item] downloaded {} in {}",
                                        crate::util::human_bytes(bytes),
                                        item.fileid
                                    ));
                                }
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
                            Err(detail) => {
                                self.events.status(item.id, "failed", detail.clone(), None);
                                results.push(ItemResult {
                                    id: item.id,
                                    fileid: item.fileid,
                                    status: "failed".to_string(),
                                    detail,
                                });
                            }
                        }
                        break;
                    }
                    Outcome::Killed => {
                        self.events
                            .status(item.id, "cancelled", "Cancelled".to_string(), None);
                        results.push(ItemResult {
                            id: item.id,
                            fileid: item.fileid,
                            status: "cancelled".to_string(),
                            detail: "Cancelled".to_string(),
                        });
                        self.teardown_session(&mut session);
                        if cancel.load(Ordering::SeqCst) {
                            for rest in &items[index..] {
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
                            index = items.len();
                        }
                        break;
                    }
                    Outcome::SessionLost => {
                        self.events
                            .log("[session] steamcmd exited during the download".to_string());
                        self.teardown_session(&mut session);
                        if attempts < 2 && !cancel.load(Ordering::SeqCst) {
                            self.events.log(
                                "[session] restarting steamcmd and retrying the item".to_string(),
                            );
                            continue;
                        }
                        let detail = "steamcmd exited unexpectedly".to_string();
                        self.events.status(item.id, "failed", detail.clone(), None);
                        results.push(ItemResult {
                            id: item.id,
                            fileid: item.fileid,
                            status: "failed".to_string(),
                            detail,
                        });
                        break;
                    }
                    Outcome::Failed { detail, retryable } => {
                        if self.recover_landed(item, output_dir, &detail, &mut results) {
                            break;
                        }
                        if retryable && attempts < 2 && !cancel.load(Ordering::SeqCst) {
                            self.events.log(format!(
                                "[steamcmd] {detail} -- restarting steamcmd and retrying once"
                            ));
                            self.teardown_session(&mut session);
                            continue;
                        }
                        self.events.status(item.id, "failed", detail.clone(), None);
                        results.push(ItemResult {
                            id: item.id,
                            fileid: item.fileid,
                            status: "failed".to_string(),
                            detail,
                        });
                        break;
                    }
                }
            }
        }

        self.teardown_session(&mut session);
        RunResult {
            ok: true,
            detail: String::new(),
            results,
        }
    }

    fn recover_landed(
        &self,
        item: &DownloadItem,
        output_dir: &Path,
        detail: &str,
        results: &mut Vec<ItemResult>,
    ) -> bool {
        if output_dir
            .join(item.appid.to_string())
            .join(item.fileid.to_string())
            .is_dir()
            && dir_has_files(
                &output_dir
                    .join(item.appid.to_string())
                    .join(item.fileid.to_string()),
            )
        {
            let dest = output_dir
                .join(item.appid.to_string())
                .join(item.fileid.to_string());
            self.events.log(format!(
                "[item] steamcmd reported a failure, but the item is present at {}",
                dest.display()
            ));
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
            return true;
        }

        let Some(source) = self.find_source_dir(item.appid, item.fileid) else {
            let _ = detail;
            return false;
        };
        if !dir_has_files(&source) {
            return false;
        }
        match self.move_output(item.appid, item.fileid, output_dir, Some(&source)) {
            Ok(dest) => {
                self.events.log(format!(
                    "[item] steamcmd did not report success, but files exist -- moved them to {}",
                    dest.display()
                ));
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
                true
            }
            Err(_) => false,
        }
    }

    fn resolve_source(
        &self,
        appid: u64,
        fileid: u64,
        reported_path: Option<&str>,
        output_dir: &Path,
    ) -> Option<PathBuf> {
        if let Some(path) = reported_path {
            let path = PathBuf::from(path);
            if path.is_dir() || path.is_file() {
                return Some(path);
            }
        }
        if let Some(path) = self.find_source_dir(appid, fileid) {
            return Some(path);
        }
        let existing = output_dir.join(appid.to_string()).join(fileid.to_string());
        existing.is_dir().then_some(existing)
    }

    pub fn source_dir(&self, appid: u64, fileid: u64) -> PathBuf {
        self.base
            .join("Steam")
            .join("steamapps")
            .join("workshop")
            .join("content")
            .join(appid.to_string())
            .join(fileid.to_string())
    }

    pub fn find_source_dir(&self, appid: u64, fileid: u64) -> Option<PathBuf> {
        let expected = self.source_dir(appid, fileid);
        if expected.is_dir() {
            return Some(expected);
        }
        let legacy = self
            .base
            .join("steamapps")
            .join("workshop")
            .join("content")
            .join(appid.to_string())
            .join(fileid.to_string());
        if legacy.is_dir() {
            return Some(legacy);
        }
        let Ok(entries) = fs::read_dir(&self.base) else {
            return None;
        };
        for entry in entries.flatten() {
            if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }
            let candidate = entry
                .path()
                .join("steamapps")
                .join("workshop")
                .join("content")
                .join(appid.to_string())
                .join(fileid.to_string());
            if candidate.is_dir() {
                return Some(candidate);
            }
        }
        None
    }

    fn move_output(
        &self,
        appid: u64,
        fileid: u64,
        output_dir: &Path,
        source: Option<&Path>,
    ) -> Result<PathBuf, String> {
        let dest = output_dir.join(appid.to_string()).join(fileid.to_string());
        if dest.is_dir() && dir_has_files(&dest) {
            return Ok(dest);
        }
        let Some(source) = source else {
            return Err(format!(
                "steamcmd reported success, but no downloaded folder was found for {fileid}"
            ));
        };
        if source == dest {
            return Ok(dest);
        }
        if dest.exists() {
            if dest.is_dir() {
                let _ = fs::remove_dir_all(&dest);
            } else {
                let _ = fs::remove_file(&dest);
            }
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("Could not create {}: {error}", parent.display()))?;
        }

        if source.is_file() {
            fs::create_dir_all(&dest)
                .map_err(|error| format!("Could not create {}: {error}", dest.display()))?;
            let name = source
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(fileid.to_string()));
            move_path(source, &dest.join(name))
                .map_err(|error| format!("Could not move the downloaded file: {error}"))?;
        } else {
            move_path(source, &dest)
                .map_err(|error| format!("Could not move the downloaded item: {error}"))?;
        }

        let mut parent = source.parent().map(Path::to_path_buf);
        while let Some(path) = parent {
            if path == self.base || !path.starts_with(&self.base) {
                break;
            }
            match fs::read_dir(&path) {
                Ok(entries) => {
                    let mut entries = entries;
                    if entries.next().is_some() {
                        break;
                    }
                    if fs::remove_dir(&path).is_err() {
                        break;
                    }
                    parent = path.parent().map(Path::to_path_buf);
                }
                _ => break,
            }
        }

        Ok(dest)
    }

    fn teardown_session(&self, session: &mut Option<Session>) {
        if let Some(mut session) = session.take() {
            session.teardown();
        }
    }
}

impl Session {
    fn send(&mut self, text: &str) -> Result<(), String> {
        if !self.alive {
            return Err("No active steamcmd session".to_string());
        }
        writeln!(self.stdin, "{text}")
            .and_then(|_| self.stdin.flush())
            .map_err(|error| format!("Could not write to steamcmd: {error}"))
    }

    fn wait_for_prompt(
        &mut self,
        manager: &SteamCmdManager,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let deadline = Instant::now() + LOGIN_TIMEOUT;
        let mut last_error: Option<String> = None;
        let mut guard_attempts = 0usize;
        let mut login_confirmed = false;
        let mut login_started = false;

        loop {
            if cancel.load(Ordering::SeqCst) {
                self.hard_stop();
                return Err("Cancelled".to_string());
            }
            if Instant::now() >= deadline {
                self.hard_stop();
                return Err(last_error
                    .unwrap_or_else(|| "Timed out waiting for steamcmd to log in".to_string()));
            }

            let event = match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    self.alive = false;
                    return Err("SteamCMD process exited unexpectedly".to_string());
                }
            };

            match event {
                SessionEvent::Line(line) => {
                    manager.events.log(line.clone());
                    if guard_re().is_match(&line) {
                        guard_attempts += 1;
                        if guard_attempts > 3 {
                            self.hard_stop();
                            return Err("Too many invalid Steam Guard codes".to_string());
                        }
                        let Some(code) = manager.bridge.ask_guard(cancel) else {
                            self.hard_stop();
                            return Err("Steam Guard entry dismissed".to_string());
                        };
                        self.send(&code)?;
                    } else if password_re().is_match(&line) {
                        self.hard_stop();
                        return Err("SteamCMD asked for a password".to_string());
                    } else if auth_fail_re().is_match(&line) {
                        last_error = Some(line.clone());
                    }
                    if login_ok_re().is_match(&line) {
                        login_confirmed = true;
                    }
                    if login_progress_re().is_match(&line) {
                        login_started = true;
                    }
                }
                SessionEvent::Tail(text) => {
                    if guard_re().is_match(&text) {
                        guard_attempts += 1;
                        if guard_attempts > 3 {
                            self.hard_stop();
                            return Err("Too many invalid Steam Guard codes".to_string());
                        }
                        manager.events.log(text);
                        let Some(code) = manager.bridge.ask_guard(cancel) else {
                            self.hard_stop();
                            return Err("Steam Guard entry dismissed".to_string());
                        };
                        self.send(&code)?;
                    } else if password_re().is_match(&text) {
                        self.hard_stop();
                        return Err("SteamCMD asked for a password".to_string());
                    } else if login_progress_re().is_match(&text) {
                        login_started = true;
                    }
                }
                SessionEvent::Prompt => {
                    if let Some(detail) = last_error {
                        return Err(detail);
                    }
                    if login_confirmed || login_started {
                        return Ok(());
                    }
                    return Err(
                        "SteamCMD returned to its prompt without confirming the login".to_string(),
                    );
                }
                SessionEvent::Eof => {
                    self.alive = false;
                    return Err(last_error
                        .unwrap_or_else(|| "SteamCMD process exited unexpectedly".to_string()));
                }
            }
        }
    }

    fn alive(&mut self) -> bool {
        if !self.alive {
            return false;
        }
        match self.child.try_wait() {
            Ok(Some(_)) => {
                self.alive = false;
                false
            }
            Ok(None) => true,
            Err(_) => false,
        }
    }

    fn hard_stop(&mut self) {
        self.alive = false;
        kill_process_group(&mut self.child);
        let _ = self.child.wait();
    }

    fn teardown(&mut self) {
        if self.alive() {
            let _ = self.send("quit");
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                match self.child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => thread::sleep(Duration::from_millis(50)),
                    Err(_) => break,
                }
            }
        }
        kill_process_group(&mut self.child);
        let _ = self.child.wait();
        self.alive = false;
        if let Some(reader) = self.reader.take() {
            join_with_timeout(reader, Duration::from_secs(2));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.teardown();
    }
}

fn reader_loop(mut stdout: std::process::ChildStdout, tx: Sender<SessionEvent>) {
    let mut buffer = String::new();
    let mut last_tail = String::new();
    let mut chunk = [0u8; 16 * 1024];

    loop {
        let read = match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => break,
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk[..read]));

        loop {
            let split = buffer
                .char_indices()
                .find(|(_, ch)| *ch == '\n' || *ch == '\r')
                .map(|(index, _)| index);
            let Some(index) = split else {
                break;
            };
            let mut line: String = buffer.drain(..=index).collect();
            line.pop();
            let line = strip_ansi(&line);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            emit_line(line, &tx);
        }

        let tail = strip_ansi(&buffer);
        let tail_trimmed = tail.trim();
        if let Some(index) = tail.rfind("Steam>") {
            let before = tail[..index].trim();
            if !before.is_empty() {
                emit_line(before, &tx);
            }
            let _ = tx.send(SessionEvent::Prompt);
            buffer.clear();
            last_tail.clear();
        } else if !tail_trimmed.is_empty() && tail_trimmed != last_tail {
            last_tail = tail_trimmed.to_string();
            let _ = tx.send(SessionEvent::Tail(tail_trimmed.to_string()));
        }
    }

    let tail = strip_ansi(&buffer);
    let tail = tail.trim();
    if !tail.is_empty() {
        if let Some(index) = tail.rfind("Steam>") {
            let before = tail[..index].trim();
            if !before.is_empty() {
                emit_line(before, &tx);
            }
            let _ = tx.send(SessionEvent::Prompt);
        } else {
            emit_line(tail, &tx);
        }
    }
    let _ = tx.send(SessionEvent::Eof);
}

fn kill_process_group(child: &mut Child) {
    if let Ok(None) = child.try_wait() {
        #[cfg(unix)]
        unsafe {
            let pid = child.id() as i32;
            if pid > 0 {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
}

fn join_with_timeout(handle: thread::JoinHandle<()>, timeout: Duration) {
    let (tx, rx) = mpsc::channel();
    let spawned = thread::Builder::new()
        .name("steamcmd-reader-join".to_string())
        .spawn(move || {
            let _ = handle.join();
            let _ = tx.send(());
        });
    if spawned.is_ok() {
        let _ = rx.recv_timeout(timeout);
    }
}

fn emit_line(line: &str, tx: &Sender<SessionEvent>) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    if line == "Steam>" {
        let _ = tx.send(SessionEvent::Prompt);
        return;
    }
    if let Some(index) = line.rfind("Steam>") {
        let before = line[..index].trim();
        if !before.is_empty() {
            let _ = tx.send(SessionEvent::Line(before.to_string()));
        }
        let _ = tx.send(SessionEvent::Prompt);
        return;
    }
    let _ = tx.send(SessionEvent::Line(line.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEvents;

    impl Events for TestEvents {
        fn log(&self, text: String) {
            eprintln!("[test] {text}");
        }
        fn status(&self, _item_id: u64, _status: &str, _detail: String, _progress: Option<f32>) {}
    }

    struct TestBridge;

    impl Bridge for TestBridge {
        fn ask_login(&self, _suggested: &str, _cancel: &AtomicBool) -> Option<(String, String)> {
            None
        }
        fn ask_guard(&self, _cancel: &AtomicBool) -> Option<String> {
            None
        }
    }

    fn test_session() -> (Session, Sender<SessionEvent>) {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let stdin = child.stdin.take().expect("stdin");
        let (tx, rx) = mpsc::channel();
        (
            Session {
                child,
                stdin,
                rx,
                reader: None,
                alive: true,
            },
            tx,
        )
    }

    fn test_manager() -> SteamCmdManager {
        SteamCmdManager::new(
            PathBuf::from("."),
            Arc::new(TestEvents),
            Arc::new(TestBridge),
        )
    }

    #[test]
    fn password_prompt_stops_login_quickly() {
        let (mut session, tx) = test_session();
        tx.send(SessionEvent::Line(
            "Cached credentials not found.".to_string(),
        ))
        .unwrap();
        tx.send(SessionEvent::Tail("password:".to_string()))
            .unwrap();
        let started = Instant::now();
        let error = session
            .wait_for_prompt(&test_manager(), &AtomicBool::new(false))
            .unwrap_err();
        assert!(error.contains("password"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn login_prompt_requires_confirmation() {
        let (mut session, tx) = test_session();
        tx.send(SessionEvent::Prompt).unwrap();
        let error = session
            .wait_for_prompt(&test_manager(), &AtomicBool::new(false))
            .unwrap_err();
        assert!(error.contains("without confirming"), "{error}");
    }

    #[test]
    fn login_confirmation_accepts_prompt() {
        let (mut session, tx) = test_session();
        tx.send(SessionEvent::Line(
            "Connecting anonymously to Steam Public...OK".to_string(),
        ))
        .unwrap();
        tx.send(SessionEvent::Prompt).unwrap();
        session
            .wait_for_prompt(&test_manager(), &AtomicBool::new(false))
            .unwrap();
    }

    #[test]
    #[ignore = "starts SteamCMD and contacts Steam"]
    fn live_login_falls_back_when_no_cached_credentials() {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Project/steamcmd");
        if !base.join("steamcmd.sh").is_file() {
            eprintln!("steamcmd not installed; skipping");
            return;
        }
        let manager = SteamCmdManager::new(base, Arc::new(TestEvents), Arc::new(TestBridge));
        let auth = Auth {
            anonymous: false,
            username: "workshopdl_missing_account".to_string(),
        };
        let item = DownloadItem {
            id: 1,
            appid: 440,
            fileid: 3863,
            title: "test".to_string(),
            cancel: Arc::new(AtomicBool::new(false)),
            kill: Arc::new(AtomicBool::new(false)),
        };
        let started = Instant::now();
        let result = manager.run_queue(
            &[item],
            &auth,
            &std::env::temp_dir().join("workshopdl-login-test"),
            &AtomicBool::new(false),
        );
        assert!(!result.ok, "{result:?}");
        assert!(result.detail.contains("cancelled"), "{result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(90),
            "fallback took {:?}",
            started.elapsed()
        );
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    struct PrintEvents;

    impl Events for PrintEvents {
        fn log(&self, text: String) {
            eprintln!("[e2e] {text}");
        }
        fn status(&self, item_id: u64, status: &str, detail: String, progress: Option<f32>) {
            eprintln!("[e2e] status {item_id} {status} {detail} {progress:?}");
        }
    }

    struct NoBridge;

    impl Bridge for NoBridge {
        fn ask_login(&self, _suggested: &str, _cancel: &AtomicBool) -> Option<(String, String)> {
            None
        }
        fn ask_guard(&self, _cancel: &AtomicBool) -> Option<String> {
            None
        }
    }

    #[test]
    #[ignore = "downloads a public workshop item with SteamCMD"]
    fn downloads_public_item() {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Project/steamcmd");
        if !base.join("steamcmd.sh").is_file() {
            eprintln!("steamcmd not installed; skipping");
            return;
        }
        let output = std::env::temp_dir().join("workshopdl-e2e-620");
        let _ = fs::remove_dir_all(&output);
        fs::create_dir_all(&output).unwrap();
        let manager = SteamCmdManager::new(base, Arc::new(PrintEvents), Arc::new(NoBridge));
        let item = DownloadItem {
            id: 7,
            appid: 620,
            fileid: 120589393,
            title: "normal coop".to_string(),
            cancel: Arc::new(AtomicBool::new(false)),
            kill: Arc::new(AtomicBool::new(false)),
        };
        let result = manager.run_queue(
            &[item],
            &Auth {
                anonymous: true,
                username: String::new(),
            },
            &output,
            &AtomicBool::new(false),
        );
        assert!(result.ok, "{result:?}");
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].status, "done", "{result:?}");
        let dest = output.join("620/120589393");
        assert!(dest.is_dir(), "missing {}", dest.display());
        assert!(dir_has_files(&dest), "empty {}", dest.display());
    }
}
