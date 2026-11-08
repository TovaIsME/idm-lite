//! Segmented, resumable HTTP download engine.

use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::windows::fs::FileExt;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CHUNK: usize = 256 * 1024; // fewer syscalls per megabyte
const MIN_SPLIT: u64 = 1 << 20; // below this one connection is plenty
// Smallest slice worth handing to a fresh connection: a steal costs one
// request round trip, and 512 KiB amortises that on any real link. Keeping
// this low is the point -- a high floor leaves a straggler holding megabytes
// while other connections sit idle.
const MIN_STEAL: u64 = 512 << 10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Queued,
    Connecting,
    Downloading,
    Paused,
    Done,
    Error,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Connecting => "connecting",
            Status::Downloading => "downloading",
            Status::Paused => "paused",
            Status::Done => "done",
            Status::Error => "error",
        }
    }
    pub fn parse(s: &str) -> Status {
        match s {
            "done" => Status::Done,
            "error" => Status::Error,
            "paused" => Status::Paused,
            _ => Status::Queued,
        }
    }
    pub fn active(self) -> bool {
        matches!(self, Status::Queued | Status::Connecting | Status::Downloading)
    }
}

/// One download. Shared between the worker thread and the UI thread.
pub struct Job {
    pub id: u64,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub name: Mutex<String>,
    pub path: Mutex<String>,
    pub total: AtomicU64, // 0 = unknown
    pub done: AtomicU64,
    pub status: Mutex<Status>,
    /// What each connection is holding right now, as (start, reached, end)
    /// byte offsets. Published for the window to draw; nothing reads it back.
    pub spans: Mutex<Vec<(u64, u64, u64)>>,
    pub stop: AtomicBool,
    pub error: Mutex<String>,
    pub added: u64,
}

impl Job {
    pub fn status(&self) -> Status {
        *self.status.lock().unwrap()
    }
    pub fn set_status(&self, s: Status) {
        *self.status.lock().unwrap() = s;
    }
}

// ---------------------------------------------------------------- file names

pub fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if "<>:\"/\\|?*".contains(c) || (c as u32) < 32 {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches(|c| c == ' ' || c == '.');
    if trimmed.is_empty() {
        "download".to_string()
    } else {
        trimmed.to_string()
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Pick a filename from Content-Disposition, falling back to the URL path.
pub fn filename_from(url: &str, disposition: Option<&str>) -> String {
    if let Some(cd) = disposition {
        let lower = cd.to_ascii_lowercase();
        if let Some(p) = lower.find("filename*=utf-8''") {
            let rest = &cd[p + "filename*=utf-8''".len()..];
            let val = rest.split(';').next().unwrap_or("").trim();
            if !val.is_empty() {
                return sanitize(&percent_decode(val));
            }
        }
        if let Some(p) = lower.find("filename=") {
            let rest = cd[p + "filename=".len()..].trim();
            let val = if let Some(stripped) = rest.strip_prefix('"') {
                stripped.split('"').next().unwrap_or("")
            } else {
                rest.split(';').next().unwrap_or("").trim()
            };
            if !val.is_empty() {
                return sanitize(&percent_decode(val));
            }
        }
    }
    let path = url
        .split(&['?', '#'][..])
        .next()
        .unwrap_or(url)
        .trim_end_matches('/');
    sanitize(&percent_decode(path.rsplit('/').next().unwrap_or("download")))
}

// -------------------------------------------------------------------- HTTP

/// One agent per download, shared by every slice. Re-splitting work means
/// many short requests to the same host, so pooling sockets and TLS sessions
/// saves a handshake on each one.
fn agent() -> ureq::Agent {
    let cfg = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .max_idle_connections(64)
        .max_idle_connections_per_host(64)
        .build();
    ureq::Agent::new_with_config(cfg)
}

fn get(
    ag: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    range: Option<(u64, Option<u64>)>,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    let mut req = ag.get(url);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    if let Some((lo, hi)) = range {
        let spec = match hi {
            Some(h) => format!("bytes={}-{}", lo, h),
            None => format!("bytes={}-", lo),
        };
        req = req.header("Range", spec.as_str());
    }
    req.call().map_err(|e| e.to_string())
}

/// Fetch a URL as text so the page can look for links in it.
///
/// Bounded on purpose: someone will paste a link to a 4 GB ISO, and reading
/// that into a String to hunt for `href` would be a fresh way to hang the
/// machine. 2 MiB covers any real page or paste.
pub fn fetch_text(url: &str) -> Result<String, String> {
    const LIMIT: u64 = 2 << 20;
    // Everything after '#' stays here. On a PrivateBin link that fragment is
    // the decryption key, and sending it to the server would hand over the
    // very secret the format exists to keep from it.
    let bare = url.split('#').next().unwrap_or(url);
    let ag = agent();
    let resp = ag
        .get(bare)
        .header("User-Agent", "Mozilla/5.0")
        .header("Accept", "text/html,application/json;q=0.9,text/plain;q=0.8,*/*;q=0.1")
        // PrivateBin answers this with the paste JSON; every other server
        // treats it as the ordinary XHR hint it looks like.
        .header("X-Requested-With", "JSONHttpRequest")
        .call()
        .map_err(|e| e.to_string())?;
    let code = resp.status().as_u16();
    if !(200..300).contains(&code) {
        return Err(refusal(code, |n| header(&resp, n)));
    }
    let kind = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !(kind.is_empty()
        || kind.starts_with("text/")
        || kind.contains("json")
        || kind.contains("xml"))
    {
        let short = kind.split(';').next().unwrap_or("").to_string();
        return Err(format!("that link is a {} file, not a page", short));
    }
    let mut raw = Vec::new();
    resp.into_body()
        .into_reader()
        .take(LIMIT)
        .read_to_end(&mut raw)
        .map_err(|e| e.to_string())?;
    // a page in some 8-bit encoding is still worth scraping for links
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

fn header(resp: &ureq::http::Response<ureq::Body>, name: &str) -> String {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Why a server said no, in words that say what to do about it.
///
/// "HTTP 403" tells nobody anything. Most refusals a download manager meets
/// are one of a handful of situations, and each has a different answer.
fn refusal(code: u16, head: impl Fn(&str) -> String) -> String {
    // Cloudflare and friends answer a plain client with an interstitial that
    // only a real browser can clear. No header is spoofable into passing it,
    // and the cookie it issues is bound to the browser that earned it.
    let challenged = !head("cf-mitigated").is_empty()
        || (matches!(code, 403 | 503 | 429)
            && (head("server").contains("cloudflare") || !head("cf-ray").is_empty()));
    if challenged {
        return "blocked by a browser check. Use the IDM-lite button in your \
                browser and Scan this link there -- it opens the page out of \
                sight, presses the download itself, and the file lands back here"
            .to_string();
    }
    match code {
        401 | 403 => format!(
            "the server refused this link (HTTP {}). It may want a sign-in, or it only \
             serves the file to the page that links to it -- try Scan from the \
             IDM-lite button in your browser instead",
            code
        ),
        404 | 410 => format!("that link is gone (HTTP {})", code),
        429 => "the server is rate limiting us (HTTP 429). Fewer connections per file \
                in Settings may get through"
            .to_string(),
        500..=599 => format!("the server is having trouble (HTTP {})", code),
        _ => format!("HTTP {}", code),
    }
}

/// Could we actually fetch this ourselves? One range request for one byte.
///
/// The extension cancels the browser's own download only once we accept it, so
/// accepting something we cannot fetch trades a download that was working for
/// an error row. Checking first costs one tiny request per capture.
pub fn can_fetch(url: &str, headers: &[(String, String)]) -> Result<(), String> {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(6)))
        // the browser is waiting on this answer: give up early and let it keep
        // the download rather than stall the click the user just made
        .timeout_global(Some(Duration::from_secs(12)))
        .http_status_as_error(false)
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let resp = get(&agent, url, headers, Some((0, Some(0))))?;
    let code = resp.status().as_u16();
    if (200..300).contains(&code) || code == 416 {
        Ok(())
    } else {
        Err(refusal(code, |n| header(&resp, n)))
    }
}

/// Returns (total size or 0 if unknown, server honours ranges, suggested name).
pub fn probe(url: &str, headers: &[(String, String)]) -> Result<(u64, bool, String), String> {
    let ag = agent();
    let mut resp = get(&ag, url, headers, Some((0, Some(0))))?;
    if resp.status() == 416 {
        resp = get(&ag, url, headers, None)?; // server dislikes range probes
    }
    let code = resp.status().as_u16();
    if !(200..300).contains(&code) {
        return Err(refusal(code, |n| header(&resp, n)));
    }
    let head = |n: &str| {
        resp.headers()
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    };
    let content_range = head("content-range");
    let content_length = head("content-length");
    let name = filename_from(url, head("content-disposition").as_deref());

    let (total, ranges) = match content_range.as_deref() {
        Some(cr) => (
            cr.rsplit('/').next().and_then(|t| t.trim().parse().ok()).unwrap_or(0),
            true,
        ),
        None => (
            content_length.and_then(|c| c.trim().parse().ok()).unwrap_or(0),
            false,
        ),
    };
    Ok((total, ranges, name))
}

// ------------------------------------------------------------ resume sidecar

/// One contiguous slice of the file: bytes [start, end], fetched up to `pos`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
struct Span {
    start: u64,
    pos: u64,
    end: u64,
}

#[derive(Serialize, Deserialize)]
struct Sidecar {
    url: String,
    size: u64,
    spans: Vec<Span>,
}

/// A slice a worker is pulling. `end` shrinks when another worker steals the
/// tail of it, which is how a straggler stops holding up the whole file.
struct Piece {
    start: u64,
    pos: AtomicU64,
    end: AtomicU64,
    claimed: AtomicBool,
}

impl Piece {
    fn new(span: Span) -> Piece {
        Piece {
            start: span.start,
            pos: AtomicU64::new(span.pos),
            end: AtomicU64::new(span.end),
            claimed: AtomicBool::new(false),
        }
    }
    fn span(&self) -> Span {
        Span {
            start: self.start,
            pos: self.pos.load(Ordering::Relaxed),
            end: self.end.load(Ordering::Relaxed),
        }
    }
    fn remaining(&self) -> u64 {
        let s = self.span();
        if s.pos > s.end { 0 } else { s.end - s.pos + 1 }
    }
    fn fetched(&self) -> u64 {
        self.pos.load(Ordering::Relaxed).saturating_sub(self.start)
    }
}

fn sidecar_path(dest: &str) -> String {
    format!("{}.idmstate", dest)
}

/// Spans must cover [0, size) exactly once or the resume data is unusable.
fn tiles(spans: &[Span], size: u64) -> bool {
    let mut sorted = spans.to_vec();
    sorted.sort_by_key(|s| s.start);
    let mut cursor = 0u64;
    for s in &sorted {
        if s.start != cursor || s.end < s.start || s.pos < s.start || s.pos > s.end + 1 {
            return false;
        }
        cursor = s.end + 1;
    }
    cursor == size
}

fn load_spans(dest: &str, url: &str, size: u64) -> Option<Vec<Span>> {
    let text = std::fs::read_to_string(sidecar_path(dest)).ok()?;
    let saved: Sidecar = serde_json::from_str(&text).ok()?;
    if saved.url != url || saved.size != size || !tiles(&saved.spans, size) {
        return None;
    }
    Some(saved.spans)
}

fn save_spans(dest: &str, url: &str, size: u64, registry: &Mutex<Vec<Arc<Piece>>>) {
    let spans: Vec<Span> = registry.lock().unwrap().iter().map(|p| p.span()).collect();
    let rec = Sidecar { url: url.to_string(), size, spans };
    if let Ok(text) = serde_json::to_string(&rec) {
        let tmp = format!("{}.tmp", sidecar_path(dest));
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, sidecar_path(dest));
        }
    }
}

fn initial_spans(size: u64, n: u64) -> Vec<Span> {
    let step = size / n;
    (0..n)
        .map(|i| {
            let start = i * step;
            let end = if i == n - 1 { size - 1 } else { (i + 1) * step - 1 };
            Span { start, pos: start, end }
        })
        .collect()
}

/// Give an idle worker something to do. An untouched slice first, otherwise
/// the back half of whoever has the most left: equal fixed segments finish at
/// wildly different times on a real link, and this is what stops finished
/// connections idling behind one straggler.
fn next_piece(registry: &Mutex<Vec<Arc<Piece>>>) -> Option<Arc<Piece>> {
    let mut pieces = registry.lock().unwrap();

    // A resumed download can carry far more slices than we run workers (every
    // steal adds one), so claim an unworked slice before splitting anything.
    // Without this, resuming would spawn a connection per saved slice.
    let unclaimed = pieces
        .iter()
        .filter(|p| p.remaining() > 0 && !p.claimed.load(Ordering::Relaxed))
        .max_by_key(|p| p.remaining())
        .cloned();
    if let Some(free) = unclaimed {
        free.claimed.store(true, Ordering::Relaxed);
        return Some(free);
    }

    let victim = pieces.iter().max_by_key(|p| p.remaining())?.clone();
    let left = victim.remaining();
    if left < MIN_STEAL * 2 {
        return None; // splitting further would cost more than it saves
    }
    let pos = victim.pos.load(Ordering::Relaxed);
    let end = victim.end.load(Ordering::Relaxed);
    let mid = pos + left / 2;
    victim.end.store(mid - 1, Ordering::Relaxed);
    let fresh = Arc::new(Piece::new(Span { start: mid, pos: mid, end }));
    fresh.claimed.store(true, Ordering::Relaxed);
    pieces.push(fresh.clone());
    Some(fresh)
}

// ---------------------------------------------------------------- the engine

/// Download `url` into `dest`, resuming from the sidecar if one is there.
/// Pass `known` to reuse a probe the caller already paid for.
/// Returns Ok(true) when the file is complete, Ok(false) when the caller
/// asked it to stop (progress is left on disk for a later resume).
pub fn download(
    url: &str,
    dest: &str,
    headers: &[(String, String)],
    connections: u64,
    job: &Job,
    known: Option<(u64, bool)>,
) -> Result<bool, String> {
    let (size, ranges) = match known {
        Some(pair) => pair,
        None => {
            let (size, ranges, _) = probe(url, headers)?;
            (size, ranges)
        }
    };
    job.total.store(size, Ordering::Relaxed);

    let sized = size > 0;
    let resumable = sized && ranges;
    // never open more sockets than the file can usefully feed
    let useful = (size / MIN_STEAL).max(1);
    let conns = if resumable && size >= MIN_SPLIT {
        connections.max(1).min(useful)
    } else {
        1
    };
    let http = agent();

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(!resumable)
        .open(dest)
        .map_err(|e| e.to_string())?;
    if sized {
        file.set_len(size).map_err(|e| e.to_string())?;
    }

    let spans = if resumable {
        load_spans(dest, url, size).unwrap_or_else(|| initial_spans(size, conns))
    } else if sized {
        vec![Span { start: 0, pos: 0, end: size - 1 }]
    } else {
        vec![Span { start: 0, pos: 0, end: u64::MAX }]
    };
    let registry: Mutex<Vec<Arc<Piece>>> = Mutex::new(
        spans.iter().map(|s| Arc::new(Piece::new(*s))).collect(),
    );
    let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let finished = AtomicUsize::new(0);
    // one worker per connection, not per saved slice
    let outstanding = spans.iter().filter(|s| s.pos <= s.end).count();
    let crew = conns.min(outstanding.max(1) as u64).max(1) as usize;

    std::thread::scope(|scope| {
        for _ in 0..crew {
            let (registry, errors, finished, file, http) =
                (&registry, &errors, &finished, &file, &http);
            scope.spawn(move || {
                let pull = |piece: &Piece| -> Result<(), String> {
                    let begin = piece.pos.load(Ordering::Relaxed);
                    let stop_at = piece.end.load(Ordering::Relaxed);
                    if begin > stop_at {
                        return Ok(());
                    }
                    let range = if resumable {
                        Some((begin, Some(stop_at)))
                    } else if begin > 0 {
                        Some((begin, None))
                    } else {
                        None
                    };
                    let resp = get(http, url, headers, range)?;
                    let code = resp.status().as_u16();
                    if !(200..300).contains(&code) {
                        return Err(format!("HTTP {}", code));
                    }
                    let mut body = resp.into_body().into_reader();
                    let mut buf = vec![0u8; CHUNK];
                    let mut at = begin;
                    loop {
                        if job.stop.load(Ordering::Relaxed) {
                            return Ok(());
                        }
                        let n = body.read(&mut buf).map_err(|e| e.to_string())?;
                        if n == 0 {
                            return Ok(());
                        }
                        let mut written = 0;
                        while written < n {
                            let w = file
                                .seek_write(&buf[written..n], at + written as u64)
                                .map_err(|e| e.to_string())?;
                            if w == 0 {
                                return Err("short write".into());
                            }
                            written += w;
                        }
                        at += n as u64;
                        piece.pos.store(at, Ordering::Relaxed);
                        // another worker may have taken our tail while we read
                        let edge = piece.end.load(Ordering::Relaxed);
                        if at > edge {
                            piece.pos.store(edge.saturating_add(1), Ordering::Relaxed);
                            return Ok(());
                        }
                    }
                };

                // A reset mid-transfer is ordinary on a real network, so a
                // slice reconnects where it stopped instead of failing the
                // file. "stale" counts only attempts that moved no bytes, so a
                // slow-but-progressing link is never given up on.
                const MAX_STALE: u32 = 4;
                const MAX_ATTEMPTS: u32 = 64;
                let mut held: Option<Arc<Piece>> = None;
                let mut stale = 0u32;
                let mut attempts = 0u32;
                let mut last_error = String::new();

                loop {
                    if job.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let piece = match &held {
                        Some(p) if p.remaining() > 0 => p.clone(),
                        _ => match next_piece(registry) {
                            Some(p) => {
                                held = Some(p.clone());
                                stale = 0;
                                attempts = 0;
                                last_error.clear();
                                p
                            }
                            None => break, // nothing left anywhere
                        },
                    };
                    // a stream the server will not let us re-enter cannot be retried
                    if !resumable && piece.pos.load(Ordering::Relaxed) > piece.start {
                        break;
                    }
                    attempts += 1;
                    let before = piece.pos.load(Ordering::Relaxed);
                    let outcome = pull(&piece);
                    let moved = piece.pos.load(Ordering::Relaxed) > before;
                    stale = if moved { 0 } else { stale + 1 };

                    if let Err(e) = outcome {
                        last_error = e;
                    } else if !sized {
                        break; // single unsized stream ran to EOF
                    } else if !moved && piece.remaining() > 0 {
                        last_error = "connection closed early".to_string();
                    }
                    if stale > MAX_STALE || attempts > MAX_ATTEMPTS {
                        if !last_error.is_empty() {
                            errors.lock().unwrap().push(last_error.clone());
                        }
                        break;
                    }
                    if stale > 0 {
                        std::thread::sleep(Duration::from_millis(200 << (stale - 1)));
                    }
                }
                finished.fetch_add(1, Ordering::Release);
            });
        }

        // this thread watches progress while the workers run
        let mut flushed = Instant::now();
        loop {
            let (got, live) = {
                let pieces = registry.lock().unwrap();
                (
                    pieces.iter().map(|p| p.fetched()).sum::<u64>(),
                    pieces
                        .iter()
                        .map(|p| {
                            let s = p.span();
                            (s.start, s.pos, s.end)
                        })
                        .collect::<Vec<_>>(),
                )
            };
            job.done.store(got, Ordering::Relaxed);
            *job.spans.lock().unwrap() = live;
            if finished.load(Ordering::Acquire) >= crew {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
            // ponytail: sidecar flushed every ~1s, so a resume may refetch up
            // to 1s per slice. Flush per chunk if that ever matters.
            if resumable && flushed.elapsed() > Duration::from_secs(1) {
                save_spans(dest, url, size, &registry);
                flushed = Instant::now();
            }
        }
    });

    let got: u64 = registry.lock().unwrap().iter().map(|p| p.fetched()).sum();
    job.done.store(got, Ordering::Relaxed);

    if job.stop.load(Ordering::Relaxed) {
        if resumable {
            save_spans(dest, url, size, &registry);
        }
        return Ok(false);
    }
    let failures = errors.lock().unwrap().clone();
    if !failures.is_empty() {
        if resumable {
            save_spans(dest, url, size, &registry);
        }
        return Err(failures.join("; "));
    }
    if sized && got != size {
        if resumable {
            save_spans(dest, url, size, &registry);
        }
        return Err(format!("incomplete: {} of {} bytes", got, size));
    }
    if !sized {
        let _ = file.set_len(got);
    }
    let _ = std::fs::remove_file(sidecar_path(dest));
    Ok(true)
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicBool;

    struct Fake {
        url: String,
        requests: std::sync::Arc<AtomicUsize>,
    }

    /// Minimal HTTP/1.1 server that honours Range, so the engine can be
    /// exercised for real instead of mocked. `kill_first` connections are cut
    /// off halfway through the body, imitating a flaky link; a range starting
    /// at or past `slow_from` is dribbled out, imitating a straggler.
    fn serve(
        body: Vec<u8>,
        honour_ranges: bool,
        kill_first: usize,
        slow_from: Option<u64>,
    ) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(AtomicUsize::new(0));
        let requests = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let body = body.clone();
                let seen = seen.clone();
                std::thread::spawn(move || {
                    let nth = seen.fetch_add(1, Ordering::SeqCst);
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut range = None;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        let lower = line.to_ascii_lowercase();
                        if let Some(rest) = lower.strip_prefix("range: bytes=") {
                            let rest = rest.trim();
                            let (lo, hi) = rest.split_once('-').unwrap_or((rest, ""));
                            let lo: u64 = lo.parse().unwrap_or(0);
                            let hi: u64 = hi.trim().parse().unwrap_or(body.len() as u64 - 1);
                            range = Some((lo, hi));
                        }
                    }
                    let mut stream = stream;
                    let total = body.len() as u64;
                    let (status, extra, slice, lo) = match range {
                        Some((lo, hi)) if honour_ranges => (
                            "206 Partial Content",
                            format!("Content-Range: bytes {}-{}/{}\r\n", lo, hi, total),
                            &body[lo as usize..=(hi.min(total - 1)) as usize],
                            lo,
                        ),
                        _ => ("200 OK", String::new(), &body[..], 0),
                    };
                    let head = format!(
                        "HTTP/1.1 {}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n\
                         Content-Disposition: attachment; filename=\"payload.bin\"\r\n\
                         {}Connection: close\r\n\r\n",
                        status,
                        slice.len(),
                        extra
                    );
                    let _ = stream.write_all(head.as_bytes());
                    if nth < kill_first {
                        let _ = stream.write_all(&slice[..slice.len() / 2]);
                        return; // drop the socket mid-body
                    }
                    let crawl = slow_from.map_or(false, |mark| lo >= mark && slice.len() > 1);
                    if crawl {
                        for part in slice.chunks(64 << 10) {
                            if stream.write_all(part).is_err() {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(12));
                        }
                        return;
                    }
                    let _ = stream.write_all(slice);
                });
            }
        });
        Fake {
            url: format!("http://127.0.0.1:{}/get?id=99", port),
            requests,
        }
    }

    fn job_for(url: &str) -> Job {
        Job {
            id: 1,
            url: url.to_string(),
            headers: vec![],
            name: Mutex::new(String::new()),
            path: Mutex::new(String::new()),
            total: AtomicU64::new(0),
            done: AtomicU64::new(0),
            status: Mutex::new(Status::Queued),
            spans: Mutex::new(Vec::new()),
        stop: AtomicBool::new(false),
            error: Mutex::new(String::new()),
            added: 0,
        }
    }

    fn blob(n: usize) -> Vec<u8> {
        (0..n).map(|i| ((i * 2654435761) >> 13) as u8).collect()
    }

    fn temp(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("idmlite-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name).to_string_lossy().into_owned()
    }

    #[test]
    fn parallel_download_is_byte_exact() {
        let data = blob(3 << 20);
        let url = serve(data.clone(), true, 0, None).url;
        let dest = temp("parallel.bin");
        let _ = std::fs::remove_file(&dest);
        let job = job_for(&url);
        assert_eq!(download(&url, &dest, &[], 8, &job, None), Ok(true));
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert!(!std::path::Path::new(&format!("{}.idmstate", dest)).exists());
    }

    #[test]
    fn resumes_each_slice_at_the_right_offset() {
        let data = blob(3 << 20);
        let url = serve(data.clone(), true, 0, None).url;
        let dest = temp("resume.bin");
        let spans: Vec<Span> = initial_spans(data.len() as u64, 8)
            .into_iter()
            .map(|s| Span { pos: s.start + (s.end - s.start + 1) / 2, ..s })
            .collect();

        // the first half of every slice is already on disk, per the sidecar
        let mut partial = vec![0u8; data.len()];
        for s in &spans {
            let (a, b) = (s.start as usize, s.pos as usize);
            partial[a..b].copy_from_slice(&data[a..b]);
        }
        std::fs::write(&dest, &partial).unwrap();
        std::fs::write(
            format!("{}.idmstate", dest),
            serde_json::to_string(&Sidecar {
                url: url.clone(),
                size: data.len() as u64,
                spans,
            })
            .unwrap(),
        )
        .unwrap();

        let job = job_for(&url);
        assert_eq!(download(&url, &dest, &[], 8, &job, None), Ok(true));
        assert_eq!(std::fs::read(&dest).unwrap(), data, "resume wrote to wrong offsets");
    }

    #[test]
    fn stale_sidecar_is_discarded() {
        let data = blob(3 << 20);
        let url = serve(data.clone(), true, 0, None).url;
        let dest = temp("stale.bin");
        let _ = std::fs::remove_file(&dest);
        std::fs::write(
            format!("{}.idmstate", dest),
            serde_json::to_string(&Sidecar {
                url: url.clone(),
                size: 123,
                spans: vec![Span { start: 0, pos: 999, end: 122 }],
            })
            .unwrap(),
        )
        .unwrap();
        let job = job_for(&url);
        assert_eq!(download(&url, &dest, &[], 8, &job, None), Ok(true));
        assert_eq!(std::fs::read(&dest).unwrap(), data, "stale sidecar was trusted");
    }

    #[test]
    fn server_without_ranges_falls_back_to_one_stream() {
        let data = blob(2 << 20);
        let url = serve(data.clone(), false, 0, None).url;
        let dest = temp("noranges.bin");
        let _ = std::fs::remove_file(&dest);
        let (total, ranges, name) = probe(&url, &[]).unwrap();
        assert_eq!((total, ranges, name.as_str()), (data.len() as u64, false, "payload.bin"));
        let job = job_for(&url);
        assert_eq!(download(&url, &dest, &[], 8, &job, None), Ok(true));
        assert_eq!(std::fs::read(&dest).unwrap(), data);
    }

    #[test]
    fn a_refusal_says_what_to_do_about_it() {
        let none = |_: &str| String::new();
        let cloudflare = |n: &str| match n {
            "server" => "cloudflare".to_string(),
            "cf-ray" => "8f2c1d4e0000-ams".to_string(),
            _ => String::new(),
        };

        // a bot check is the one refusal the browser can clear and we cannot
        for code in [403, 503, 429] {
            assert!(refusal(code, cloudflare).contains("browser"),
                "HTTP {} from a challenge must point at the browser", code);
        }
        assert!(refusal(403, |n: &str| if n == "cf-mitigated" { "challenge".into() } else { String::new() })
            .contains("browser"));

        // the same codes without the fingerprint are ordinary refusals
        assert!(!refusal(403, none).contains("browser check"));
        assert!(refusal(403, none).contains("sign-in"));
        assert!(refusal(429, none).contains("Settings"));
        assert!(refusal(404, none).contains("gone"));
        assert!(refusal(500, none).contains("trouble"));
        assert!(refusal(418, none).contains("418"));
        // a 200 is never a refusal, but if one ever reaches here, say nothing clever
        assert_eq!(refusal(200, none), "HTTP 200");
    }

    #[test]
    fn names_come_from_disposition_then_url() {
        assert_eq!(filename_from("http://h/a/b.zip", None), "b.zip");
        assert_eq!(filename_from("http://h/download?id=5", None), "download");
        assert_eq!(
            filename_from("http://h/x", Some("attachment; filename=\"my report.pdf\"")),
            "my report.pdf"
        );
        assert_eq!(
            filename_from("http://h/x", Some("attachment; filename*=UTF-8''caf%C3%A9%20v2.bin")),
            "café v2.bin"
        );
        assert_eq!(sanitize("a/b:c*?.txt"), "a_b_c__.txt");
        assert_eq!(sanitize("  ..  "), "download");
    }

    #[test]
    fn survives_connections_dropped_mid_transfer() {
        let data = blob(3 << 20);
        // every segment's first attempt dies halfway; only retrying can finish
        let url = serve(data.clone(), true, 8, None).url;
        let dest = temp("flaky.bin");
        let _ = std::fs::remove_file(&dest);
        let _ = std::fs::remove_file(format!("{}.idmstate", dest));
        let job = job_for(&url);
        assert_eq!(download(&url, &dest, &[], 8, &job, None), Ok(true));
        assert_eq!(std::fs::read(&dest).unwrap(), data, "retry reassembled the file wrongly");
    }

    #[test]
    fn idle_workers_take_over_a_slow_slice() {
        let data = blob(16 << 20);
        let size = data.len() as u64;
        // the last quarter dribbles out; the rest arrives instantly
        let fake = serve(data.clone(), true, 0, Some(size * 3 / 4));
        let dest = temp("straggler.bin");
        let _ = std::fs::remove_file(&dest);
        let _ = std::fs::remove_file(format!("{}.idmstate", dest));

        let job = job_for(&fake.url);
        assert_eq!(download(&fake.url, &dest, &[], 4, &job, None), Ok(true));
        assert_eq!(std::fs::read(&dest).unwrap(), data, "re-split file came back wrong");

        // 1 probe + 4 initial slices = 5 requests if nothing is ever re-split
        let n = fake.requests.load(Ordering::SeqCst);
        assert!(n > 5, "slow slice was never taken over: only {} requests", n);
    }

    #[test]
    fn slices_tile_the_file_without_gaps() {
        let spans = initial_spans(1000, 8);
        assert!(tiles(&spans, 1000));
        assert_eq!(spans[0].start, 0);
        assert_eq!(spans.last().unwrap().end, 999);
        // resume data that does not cover the whole file must be rejected
        assert!(!tiles(&spans[1..], 1000));
        assert!(!tiles(&[Span { start: 0, pos: 0, end: 500 }], 1000));
        assert!(!tiles(&[Span { start: 0, pos: 9999, end: 999 }], 1000));
    }

    #[test]
    fn an_idle_worker_takes_the_back_half_of_the_straggler() {
        let registry = Mutex::new(vec![
            Arc::new(Piece::new(Span { start: 0, pos: 199, end: 199 })), // nearly done
            Arc::new(Piece::new(Span { start: 200, pos: 200, end: 200 + (16 << 20) })),
        ]);
        // both slices already have a worker, so the only way to get more work
        // is to split one
        for p in registry.lock().unwrap().iter() {
            p.claimed.store(true, Ordering::Relaxed);
        }

        let before = registry.lock().unwrap()[1].remaining();
        let taken = next_piece(&registry).expect("the straggler should be split");
        let after = registry.lock().unwrap()[1].remaining();

        assert!(after < before, "victim should have handed work over");
        assert_eq!(after + taken.remaining(), before, "stealing lost or duplicated bytes");
        assert_eq!(
            registry.lock().unwrap()[1].end.load(Ordering::Relaxed) + 1,
            taken.start,
            "the two slices must stay adjacent"
        );

        // nothing worth splitting: workers must exit rather than spin
        let tiny = Mutex::new(vec![Arc::new(Piece::new(Span { start: 0, pos: 0, end: 1000 }))]);
        tiny.lock().unwrap()[0].claimed.store(true, Ordering::Relaxed);
        assert!(next_piece(&tiny).is_none());
    }

    #[test]
    fn resumed_slices_are_claimed_before_anything_is_split() {
        // Every steal appends a slice, so a resumed download can hold far more
        // slices than connections. Workers must pick those up instead of
        // splitting one and leaving the rest orphaned.
        let registry = Mutex::new(vec![
            Arc::new(Piece::new(Span { start: 0, pos: 0, end: (8 << 20) - 1 })),
            Arc::new(Piece::new(Span { start: 8 << 20, pos: 8 << 20, end: (16 << 20) - 1 })),
        ]);
        let before = registry.lock().unwrap().len();

        let first = next_piece(&registry).expect("first worker gets a slice");
        let second = next_piece(&registry).expect("second worker gets the other slice");

        assert_eq!(
            registry.lock().unwrap().len(),
            before,
            "existing slices should be claimed, not split"
        );
        assert_ne!(first.start, second.start, "two workers claimed the same slice");
    }
}
