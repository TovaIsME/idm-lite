//! IDM-lite -- download manager, browser native-messaging host, self-registrar.
//!
//! Modes, chosen from argv:
//!   (nothing)                  run the app window
//!   chrome-extension://<id>/   native-messaging host (the browser passes this)
//!   --register / --unregister  browser integration only
//!
//! The app owns its own process. The browser host mode is a thin forwarder that
//! hands each captured download to the running app over loopback, starting it
//! first if it is not up, so downloads outlive the browser.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod download;
mod ui;

use download::{Job, Status};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const APP_NAME: &str = "IDM-lite";
pub const HOST_NAME: &str = "com.idmlite.host";
/// Pinned by the "key" field in extension/manifest.json.
pub const EXT_ID: &str = "ffmenbaflpdmmlfigjodjcnajmlgndgl";

pub const BROWSER_KEYS: [&str; 4] = [
    r"Software\Google\Chrome\NativeMessagingHosts",
    r"Software\Microsoft\Edge\NativeMessagingHosts",
    r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts",
    r"Software\Chromium\NativeMessagingHosts",
];

// ------------------------------------------------------------------- paths

pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("IDMLITE_DATA") {
        return PathBuf::from(d);
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(APP_NAME)
}

pub fn data_file(name: &str) -> PathBuf {
    data_dir().join(name)
}

pub fn exe_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("IDM-lite.exe"))
}

fn default_download_dir() -> String {
    std::env::var_os("USERPROFILE")
        .map(|p| PathBuf::from(p).join("Downloads"))
        .unwrap_or_else(|| PathBuf::from("Downloads"))
        .to_string_lossy()
        .into_owned()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_json<T: for<'a> Deserialize<'a>>(path: &Path) -> Option<T> {
    // strip a BOM: a config hand-edited in Notepad has one, and silently
    // resetting the user's settings over it would be unkind.
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
}

fn write_json<T: Serialize>(path: &Path, value: &T) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(value) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// Avoid clobbering an existing file -- unless a sidecar says it is our own
/// half-finished download, which we should resume instead. `taken` holds paths
/// already handed to other running downloads, which are not on disk yet.
pub fn unique_path(folder: &str, name: &str, taken: &HashSet<String>) -> String {
    let dir = Path::new(folder);
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let mut n = 0;
    loop {
        let candidate = if n == 0 {
            dir.join(name)
        } else {
            dir.join(format!("{} ({}){}", stem, n, ext))
        };
        let path = candidate.to_string_lossy().into_owned();
        let ours = Path::new(&format!("{}.idmstate", path)).exists();
        if !taken.contains(&path) && (ours || !candidate.exists()) {
            return path;
        }
        n += 1;
    }
}

// ------------------------------------------------------------------ config

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub dir: String,
    pub connections: u64,
    pub parallel: usize,
}

impl Default for Config {
    fn default() -> Self {
        // 16 beats IDM's default of 8 on servers that throttle per connection,
        // and the work-stealing engine keeps the extra sockets busy.
        Config { dir: default_download_dir(), connections: 16, parallel: 3 }
    }
}

impl Config {
    pub fn load() -> Config {
        let mut c: Config = read_json(&data_file("config.json")).unwrap_or_default();
        c.connections = c.connections.clamp(1, 32);
        c.parallel = c.parallel.clamp(1, 10);
        if c.dir.trim().is_empty() {
            c.dir = default_download_dir();
        }
        c
    }
    pub fn save(&self) {
        write_json(&data_file("config.json"), self);
    }
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct JobRec {
    pub id: u64,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub done: u64,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub added: u64,
}

/// What the browser extension sends us.
#[derive(Deserialize, Default)]
pub struct Capture {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub cookie: String,
    #[serde(default)]
    pub referrer: String,
    #[serde(default)]
    pub ua: String,
}

impl Capture {
    pub fn headers(&self) -> Vec<(String, String)> {
        let mut h = vec![(
            "User-Agent".to_string(),
            if self.ua.is_empty() { "Mozilla/5.0".to_string() } else { self.ua.clone() },
        )];
        if !self.cookie.is_empty() {
            h.push(("Cookie".to_string(), self.cookie.clone()));
        }
        if !self.referrer.is_empty() {
            h.push(("Referer".to_string(), self.referrer.clone()));
        }
        h
    }
}

// ------------------------------------------------------------- work queue

/// mpsc::Receiver is single-consumer, so the worker pool needs its own queue.
pub struct Queue {
    items: Mutex<VecDeque<u64>>,
    ready: Condvar,
}

impl Queue {
    fn new() -> Queue {
        Queue { items: Mutex::new(VecDeque::new()), ready: Condvar::new() }
    }
    pub fn push(&self, id: u64) {
        self.items.lock().unwrap().push_back(id);
        self.ready.notify_one();
    }
    fn pop(&self) -> u64 {
        let mut guard = self.items.lock().unwrap();
        loop {
            if let Some(id) = guard.pop_front() {
                return id;
            }
            guard = self.ready.wait(guard).unwrap();
        }
    }
}

// --------------------------------------------------------------- app state

pub struct App {
    pub jobs: Mutex<Vec<Arc<Job>>>,
    pub config: Mutex<Config>,
    pub queue: Arc<Queue>,
    pub inbox: Mutex<Vec<Capture>>,
    /// Destinations already promised to running downloads. Two downloads whose
    /// server-suggested names collide would otherwise both pick the same file.
    taken_paths: Mutex<HashSet<String>>,
    next_id: AtomicU64,
}

impl App {
    pub fn new() -> Arc<App> {
        let config = Config::load();
        let app = Arc::new(App {
            jobs: Mutex::new(Vec::new()),
            config: Mutex::new(config.clone()),
            queue: Arc::new(Queue::new()),
            inbox: Mutex::new(Vec::new()),
            taken_paths: Mutex::new(HashSet::new()),
            next_id: AtomicU64::new(1),
        });
        for _ in 0..config.parallel {
            let worker = app.clone();
            std::thread::spawn(move || loop {
                let id = worker.queue.pop();
                worker.run_job(id);
            });
        }
        app.restore();
        app
    }

    /// Reserve a destination so no other download can pick the same one.
    fn claim_path(&self, folder: &str, name: &str) -> String {
        let mut taken = self.taken_paths.lock().unwrap();
        let path = unique_path(folder, name, &taken);
        taken.insert(path.clone());
        path
    }

    pub fn find(&self, id: u64) -> Option<Arc<Job>> {
        self.jobs.lock().unwrap().iter().find(|j| j.id == id).cloned()
    }

    pub fn add(&self, url: &str, headers: Vec<(String, String)>, name: &str) -> Option<u64> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let job = Arc::new(Job {
            id,
            url: url.to_string(),
            headers,
            // left blank on purpose: Content-Disposition beats guessing from
            // a URL like /download?id=5, and only run_job can ask the server.
            name: Mutex::new(if name.is_empty() { String::new() } else { download::sanitize(name) }),
            path: Mutex::new(String::new()),
            total: AtomicU64::new(0),
            done: AtomicU64::new(0),
            status: Mutex::new(Status::Queued),
            spans: Mutex::new(Vec::new()),
        stop: AtomicBool::new(false),
            error: Mutex::new(String::new()),
            added: now_secs(),
        });
        self.jobs.lock().unwrap().push(job);
        self.queue.push(id);
        Some(id)
    }

    fn run_job(&self, id: u64) {
        let job = match self.find(id) {
            Some(j) => j,
            None => return,
        };
        if job.stop.load(Ordering::Relaxed) {
            job.set_status(Status::Paused);
            return;
        }
        job.set_status(Status::Connecting);

        // reuse this probe inside download() instead of paying for a second one
        let mut probed: Option<(u64, bool)> = None;
        if job.path.lock().unwrap().is_empty() {
            let folder = self.config.lock().unwrap().dir.clone();
            match download::probe(&job.url, &job.headers) {
                Ok((size, ranges, served)) => {
                    probed = Some((size, ranges));
                    let mut name = job.name.lock().unwrap().clone();
                    if name.is_empty() {
                        name = served;
                    }
                    if name.is_empty() {
                        name = download::filename_from(&job.url, None);
                    }
                    let _ = std::fs::create_dir_all(&folder);
                    *job.path.lock().unwrap() = self.claim_path(&folder, &name);
                    *job.name.lock().unwrap() = name;
                    job.total.store(size, Ordering::Relaxed);
                }
                Err(e) => {
                    *job.error.lock().unwrap() = e;
                    job.set_status(Status::Error);
                    self.save();
                    return;
                }
            }
        }

        let dest = job.path.lock().unwrap().clone();
        let conns = self.config.lock().unwrap().connections;
        job.set_status(Status::Downloading);
        match download::download(&job.url, &dest, &job.headers, conns, &job, probed) {
            Ok(true) => job.set_status(Status::Done),
            Ok(false) => job.set_status(Status::Paused),
            Err(e) => {
                *job.error.lock().unwrap() = e;
                job.set_status(Status::Error);
            }
        }
        self.save();
    }

    pub fn pause(&self, id: u64) {
        if let Some(job) = self.find(id) {
            if job.status().active() {
                job.stop.store(true, Ordering::Relaxed);
            }
        }
    }

    pub fn resume(&self, id: u64) {
        if let Some(job) = self.find(id) {
            if matches!(job.status(), Status::Paused | Status::Error) {
                job.stop.store(false, Ordering::Relaxed);
                job.error.lock().unwrap().clear();
                job.set_status(Status::Queued);
                self.queue.push(id);
            }
        }
    }

    /// Stop the job and forget it; an unfinished file is deleted with it.
    pub fn remove(&self, id: u64) {
        let job = match self.find(id) {
            Some(j) => j,
            None => return,
        };
        job.stop.store(true, Ordering::Relaxed);
        let finished = job.status() == Status::Done;
        let path = job.path.lock().unwrap().clone();
        self.jobs.lock().unwrap().retain(|j| j.id != id);
        if !path.is_empty() {
            self.taken_paths.lock().unwrap().remove(&path);
        }
        if !finished && !path.is_empty() {
            // Wait for the worker to actually let go. Deleting while it still
            // has the file open fails on Windows and strands the partial file.
            let watched = job.clone();
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline && watched.status() == Status::Downloading {
                    std::thread::sleep(Duration::from_millis(100));
                }
                std::thread::sleep(Duration::from_millis(250));
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(format!("{}.idmstate", path));
            });
        }
        self.save();
    }

    pub fn save(&self) {
        let recs: Vec<JobRec> = self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .map(|j| JobRec {
                id: j.id,
                url: j.url.clone(),
                headers: j.headers.clone(),
                name: j.name.lock().unwrap().clone(),
                path: j.path.lock().unwrap().clone(),
                total: j.total.load(Ordering::Relaxed),
                done: j.done.load(Ordering::Relaxed),
                status: j.status().label().to_string(),
                error: j.error.lock().unwrap().clone(),
                added: j.added,
            })
            .collect();
        write_json(&data_file("downloads.json"), &recs);
    }

    /// Drop finished rows from the list. The files they produced stay on disk:
    /// this is the history being cleared, not the downloads being deleted.
    pub fn clear_finished(&self) {
        let mut jobs = self.jobs.lock().unwrap();
        let mut taken = self.taken_paths.lock().unwrap();
        jobs.retain(|job| {
            let keep = job.status().active() || job.status() == Status::Paused;
            if !keep {
                taken.remove(&*job.path.lock().unwrap());
            }
            keep
        });
        drop(taken);
        drop(jobs);
        self.save();
    }

    fn restore(&self) {
        let recs: Vec<JobRec> = read_json(&data_file("downloads.json")).unwrap_or_default();
        for rec in recs {
            if rec.url.is_empty() {
                continue;
            }
            let was_active = Status::parse(&rec.status).active();
            let job = Arc::new(Job {
                id: rec.id,
                url: rec.url,
                headers: rec.headers,
                name: Mutex::new(rec.name),
                path: Mutex::new(rec.path),
                total: AtomicU64::new(rec.total),
                done: AtomicU64::new(rec.done),
                status: Mutex::new(if was_active { Status::Queued } else { Status::parse(&rec.status) }),
                spans: Mutex::new(Vec::new()),
        stop: AtomicBool::new(false),
                error: Mutex::new(rec.error),
                added: rec.added,
            });
            self.next_id.fetch_max(rec.id + 1, Ordering::Relaxed);
            let restored = job.path.lock().unwrap().clone();
            if !restored.is_empty() {
                self.taken_paths.lock().unwrap().insert(restored);
            }
            self.jobs.lock().unwrap().push(job);
            if was_active {
                self.queue.push(rec.id); // pick up where we left off
            }
        }
    }
}

// --------------------------------------------------------------------- IPC

#[derive(Serialize, Deserialize)]
struct IpcInfo {
    port: u16,
    token: String,
    pid: u32,
}

fn random_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("system randomness");
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Deserialize)]
struct IpcMessage {
    #[serde(default)]
    token: String,
    #[serde(default)]
    job: Capture,
}

/// A browser replaying its download history hands us one capture per history
/// row -- 120 of them in three minutes once, each a host process and a job with
/// 16 sockets, which froze the machine. The extension filters those rows out,
/// but the app must not depend on that: past this many captures in a minute we
/// refuse, and the browser keeps the download.
/// ponytail: fixed window, a token bucket only if real use ever trips it.
const CAPTURES_PER_MINUTE: u32 = 20;

/// Count a capture against the current minute. False once the burst is over
/// budget. `window` is (minute start, captures so far).
fn burst_ok(window: &mut (Instant, u32), now: Instant, max: u32) -> bool {
    if now.duration_since(window.0) >= Duration::from_secs(60) {
        *window = (now, 0);
    }
    window.1 += 1;
    window.1 <= max
}

/// Listen on loopback for captures from the host stub. `wake` nudges the UI.
pub fn serve_ipc(app: Arc<App>, wake: Box<dyn Fn() + Send>) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let token = random_token();
    write_json(
        &data_file("ipc.json"),
        &IpcInfo { port, token: token.clone(), pid: std::process::id() },
    );
    std::thread::spawn(move || {
        let mut window = (Instant::now(), 0u32);
        for stream in listener.incoming().flatten() {
            let mut buf = Vec::new();
            let mut limited = stream.try_clone().map(|s| s.take(1 << 18));
            if let Ok(ref mut r) = limited {
                let _ = r.read_to_end(&mut buf);
            }
            let msg: IpcMessage = match serde_json::from_slice(&buf) {
                Ok(m) => m,
                Err(_) => continue,
            };
            // Loopback is reachable by any local process and by page scripts,
            // so this token is the only thing proving the sender is our stub.
            if !constant_time_eq(&msg.token, &token) {
                continue;
            }
            let mut stream = stream;
            // Refuse anything we cannot fetch ourselves. The extension waits for
            // this answer before it cancels the browser's download, so a refusal
            // leaves the file with the browser -- which can clear a check we
            // cannot -- instead of losing it to an error row here.
            if !msg.job.url.is_empty() {
                if let Err(why) = download::can_fetch(&msg.job.url, &msg.job.headers()) {
                    eprintln!("idmlite: left to the browser -- {}", why);
                    let _ = stream.write_all(b"no");
                    continue;
                }
            }
            if !burst_ok(&mut window, Instant::now(), CAPTURES_PER_MINUTE) {
                let _ = stream.write_all(b"no"); // answered, so the stub stops
                continue;
            }
            let _ = stream.write_all(b"ok");
            app.inbox.lock().unwrap().push(msg.job);
            wake();
        }
    });
    Ok(())
}

/// Hand a capture to the running app. None if no app answered at all, which is
/// the only case worth starting one for -- a refusal must not become a retry
/// storm on top of the burst that caused it.
fn ipc_send(job: &serde_json::Value, wait: Duration) -> Option<bool> {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(info) = read_json::<IpcInfo>(&data_file("ipc.json")) {
            if let Ok(mut stream) =
                TcpStream::connect_timeout(&(([127, 0, 0, 1], info.port).into()), Duration::from_secs(3))
            {
                let payload = serde_json::json!({ "token": info.token, "job": job });
                let ok = stream.write_all(payload.to_string().as_bytes()).is_ok()
                    && stream.shutdown(std::net::Shutdown::Write).is_ok();
                if ok {
                    let mut reply = String::new();
                    let _ = stream.take(16).read_to_string(&mut reply);
                    if reply == "ok" {
                        return Some(true);
                    }
                    if reply == "no" {
                        return Some(false);
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// Single-instance lock. An unshared file handle: Windows refuses a second
/// open while this one lives, and releases it when the process dies, so a
/// crash cannot strand a stale lock. Held for the app's whole lifetime.
///
/// Without it, a burst of captures makes every host stub spawn its own app
/// (they all race past the ipc.json check before any instance has written
/// it), and N copies of the download engine run at once.
fn instance_lock() -> Option<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    let path = data_file("instance.lock");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::OpenOptions::new().write(true).create(true).share_mode(0).open(path).ok()
}

fn launch_app() -> bool {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    std::process::Command::new(exe_path())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

// ------------------------------------------------------ native messaging

/// One native-messaging frame: a little-endian length, then the JSON.
fn reply(stdout: &std::io::Stdout, body: &str) -> std::io::Result<()> {
    let mut out = stdout.lock();
    out.write_all(&(body.len() as u32).to_le_bytes())?;
    out.write_all(body.as_bytes())?;
    out.flush()
}

fn host_mode() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    loop {
        let mut head = [0u8; 4];
        if stdin.lock().read_exact(&mut head).is_err() {
            return; // browser closed the pipe
        }
        let len = u32::from_le_bytes(head) as usize;
        if len > (1 << 20) {
            return;
        }
        let mut buf = vec![0u8; len];
        if stdin.lock().read_exact(&mut buf).is_err() {
            return;
        }
        let job: serde_json::Value = serde_json::from_slice(&buf).unwrap_or(serde_json::Value::Null);

        // A scan is just a fetch; it needs no app and starts none. The page
        // goes back to the extension, which reads the links out of it.
        if let Some(page) = job.get("scrape").and_then(|u| u.as_str()) {
            let answer = match download::fetch_text(page) {
                Ok(body) => serde_json::json!({ "scraped": page, "body": body }),
                Err(e) => serde_json::json!({ "scraped": page, "error": e }),
            };
            if reply(&stdout, &answer.to_string()).is_err() {
                return;
            }
            continue;
        }

        let url = job.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
        let accepted = match ipc_send(&job, Duration::ZERO) {
            Some(answer) => answer,
            None => launch_app() && ipc_send(&job, Duration::from_secs(25)) == Some(true),
        };

        let answer = serde_json::json!({ "accepted": accepted, "url": url }).to_string();
        if reply(&stdout, &answer).is_err() {
            return;
        }
    }
}

// ------------------------------------------------------------- registration

fn host_manifest_path() -> PathBuf {
    data_file(&format!("{}.json", HOST_NAME))
}

pub fn register() -> Vec<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let manifest = host_manifest_path();
    write_json(
        &manifest,
        &serde_json::json!({
            "name": HOST_NAME,
            "description": format!("{} downloader", APP_NAME),
            "path": exe_path().to_string_lossy(),
            "type": "stdio",
            "allowed_origins": [format!("chrome-extension://{}/", EXT_ID)],
        }),
    );
    let manifest_str = manifest.to_string_lossy().into_owned();
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let mut done = Vec::new();
    for parent in BROWSER_KEYS {
        let path = format!("{}\\{}", parent, HOST_NAME);
        if let Ok((key, _)) = hkcu.create_subkey(&path) {
            if key.set_value("", &manifest_str).is_ok() {
                done.push(parent.split('\\').nth(1).unwrap_or(parent).to_string());
            }
        }
    }
    done
}

pub fn unregister() {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    for parent in BROWSER_KEYS {
        let _ = hkcu.delete_subkey(format!("{}\\{}", parent, HOST_NAME));
    }
    let _ = std::fs::remove_file(host_manifest_path());
}

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Launch at sign-in, so the tray icon is already there when a download starts.
/// The registry is the single source of truth; config.json does not mirror it.
pub fn set_autostart(on: bool) {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok((key, _)) = hkcu.create_subkey(RUN_KEY) {
        if on {
            let cmd = format!("\"{}\"", exe_path().to_string_lossy());
            let _ = key.set_value(APP_NAME, &cmd);
        } else {
            let _ = key.delete_value(APP_NAME);
        }
    }
}

pub fn is_autostart() -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let exe = exe_path().to_string_lossy().into_owned();
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY)
        .and_then(|k| k.get_value::<String, _>(APP_NAME))
        .map(|v| v.contains(&exe))
        .unwrap_or(false)
}

pub fn is_registered() -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let manifest = host_manifest_path();
    let recorded: Option<serde_json::Value> = read_json(&manifest);
    let points_at_us = recorded
        .and_then(|v| v.get("path").and_then(|p| p.as_str()).map(|s| s.to_string()))
        .map(|p| p == exe_path().to_string_lossy())
        .unwrap_or(false);
    if !points_at_us {
        return false;
    }
    let want = manifest.to_string_lossy().into_owned();
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    BROWSER_KEYS.iter().any(|parent| {
        hkcu.open_subkey(format!("{}\\{}", parent, HOST_NAME))
            .and_then(|k| k.get_value::<String, _>(""))
            .map(|v| v == want)
            .unwrap_or(false)
    })
}

// -------------------------------------------------------------------- main

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some(a) if a.starts_with("chrome-extension://") => host_mode(),
        Some("--register") => {
            let done = register();
            if done.is_empty() {
                std::process::exit(1);
            }
        }
        Some("--unregister") => unregister(),
        Some(_) => std::process::exit(2),
        None => {
            // another instance owns the app: it raises its window, we exit
            let _lock = match instance_lock() {
                Some(lock) => lock,
                None => {
                    let _ = ipc_send(&serde_json::json!({}), Duration::ZERO);
                    return;
                }
            };
            ui::run();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_path_skips_existing_but_resumes_our_own() {
        let dir = std::env::temp_dir().join(format!("idmlite-up-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let folder = dir.to_string_lossy().into_owned();
        let taken = dir.join("a.zip");
        std::fs::write(&taken, b"x").unwrap();

        let none = HashSet::new();
        assert!(unique_path(&folder, "a.zip", &none).ends_with("a (1).zip"));

        // a path already promised to another running download is not free,
        // even though nothing is on disk for it yet
        let mut claimed = HashSet::new();
        claimed.insert(dir.join("a (1).zip").to_string_lossy().into_owned());
        assert!(unique_path(&folder, "a.zip", &claimed).ends_with("a (2).zip"));

        // a sidecar means that file is our own half-finished download
        std::fs::write(dir.join("a.zip.idmstate"), b"{}").unwrap();
        assert!(unique_path(&folder, "a.zip", &none).ends_with("a.zip"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_burst_is_capped_per_minute() {
        let t0 = Instant::now();
        let mut window = (t0, 0);
        for _ in 0..3 {
            assert!(burst_ok(&mut window, t0, 3));
        }
        // a history replay keeps arriving; everything past the budget is refused
        assert!(!burst_ok(&mut window, t0, 3));
        assert!(!burst_ok(&mut window, t0 + Duration::from_secs(59), 3));
        // next minute, a real download is taken again
        assert!(burst_ok(&mut window, t0 + Duration::from_secs(60), 3));
    }

    #[test]
    fn clearing_history_keeps_what_is_still_running() {
        let dir = std::env::temp_dir().join(format!("idmlite-clear-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("IDMLITE_DATA", &dir);

        let app = App::new();
        let keep = app.add("http://h/live.bin", vec![], "live.bin").unwrap();
        let gone = app.add("http://h/old.bin", vec![], "old.bin").unwrap();
        let failed = app.add("http://h/bad.bin", vec![], "bad.bin").unwrap();
        app.find(gone).unwrap().set_status(Status::Done);
        app.find(failed).unwrap().set_status(Status::Error);
        app.find(keep).unwrap().set_status(Status::Downloading);

        app.clear_finished();

        let left: Vec<u64> = app.jobs.lock().unwrap().iter().map(|j| j.id).collect();
        assert_eq!(left, vec![keep], "finished and failed rows go, running ones stay");

        // a paused download is unfinished business, not history
        app.find(keep).unwrap().set_status(Status::Paused);
        app.clear_finished();
        assert_eq!(app.jobs.lock().unwrap().len(), 1, "a paused download is kept");

        std::env::remove_var("IDMLITE_DATA");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_comparison_rejects_mismatches() {
        assert!(constant_time_eq("abc123", "abc123"));
        assert!(!constant_time_eq("abc123", "abc124"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("", "x"));
    }

    #[test]
    fn capture_becomes_request_headers() {
        let c = Capture {
            url: "http://h/f".into(),
            filename: String::new(),
            cookie: "s=1".into(),
            referrer: "http://p/".into(),
            ua: "UA/1".into(),
        };
        assert_eq!(
            c.headers(),
            vec![
                ("User-Agent".to_string(), "UA/1".to_string()),
                ("Cookie".to_string(), "s=1".to_string()),
                ("Referer".to_string(), "http://p/".to_string()),
            ]
        );
        // a capture with no browser identity still needs a User-Agent
        assert_eq!(Capture::default().headers(), vec![("User-Agent".to_string(), "Mozilla/5.0".to_string())]);
    }

    #[test]
    fn config_clamps_nonsense_values() {
        let raw = r#"{"dir":"","connections":9999,"parallel":0}"#;
        let mut c: Config = serde_json::from_str(raw).unwrap();
        c.connections = c.connections.clamp(1, 32);
        c.parallel = c.parallel.clamp(1, 10);
        assert_eq!((c.connections, c.parallel), (32, 1));
    }

    // Progress formatting now lives in the page (bytes()/duration() in ui.html),
    // so it is exercised by the UI, not from here.
}
