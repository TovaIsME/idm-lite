//! Frameless WebView2 window. Rust owns the downloads; the page owns the pixels.
//!
//! Rust -> page: `__state(json)` pushed on a timer.
//! page -> Rust: one JSON command per `window.ipc.postMessage` call.

use crate::download::Status;
use crate::App;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
#[cfg(windows)]
use tao::platform::windows::WindowBuilderExtWindows;
use tao::window::{Fullscreen, ResizeDirection, Window, WindowBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon as TrayImage, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use wry::WebViewBuilder;

const HTML: &str = include_str!("ui.html");
/// Shared with the extension popup, which loads the same file from its folder.
const SCRAPE_JS: &str = include_str!("../extension/scrape.js");
const FONT_B64: &str = include_str!("../assets/display.b64");
const TRAY_RGBA: &[u8] = include_bytes!("../assets/tray.rgba");
const TRAY_PX: u32 = 32;

#[derive(Debug)]
enum UserEvent {
    Command(String),
    Tick,
    Captured,
    /// A scraped page, as JSON, on its way back to the picker.
    Scraped(String),
    TrayOpen,
    TrayMenu(MenuId),
}

/// A link safe to hand to the shell as one argument.
fn safe_link(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && !url.contains(char::is_whitespace)
        && url.len() < 2000
}

/// Servers hand a plain client a challenge page or a 403; they hand a browser
/// the file. We are fetching on a browser's behalf, so we say so.
fn browser_ua() -> (String, String) {
    ("User-Agent".to_string(), "Mozilla/5.0".to_string())
}

/// Bring the window back from the tray.
fn reveal(window: &Window) {
    window.set_visible(true);
    window.set_minimized(false);
    window.set_focus();
}

#[derive(Serialize)]
struct JobView {
    id: u64,
    url: String,
    /// One [from, reached, to] per connection, as fractions of the file.
    spans: Vec<[f64; 3]>,
    name: String,
    total: u64,
    done: u64,
    status: &'static str,
    error: String,
    speed: f64,
    eta: f64,
}

#[derive(Serialize)]
struct StateView {
    jobs: Vec<JobView>,
    active: usize,
    rate: f64,
    dir: String,
    connections: u64,
    parallel: usize,
    maximized: bool,
    autostart: bool,
}

/// Exponential moving average of each job's byte rate.
#[derive(Default)]
struct Meter {
    marks: HashMap<u64, (Instant, u64, f64)>,
}

impl Meter {
    fn sample(&mut self, id: u64, done: u64, active: bool) -> f64 {
        let now = Instant::now();
        let slot = self.marks.entry(id).or_insert((now, done, 0.0));
        let elapsed = now.duration_since(slot.0).as_secs_f64();
        if elapsed >= 0.35 {
            let instant = done.saturating_sub(slot.1) as f64 / elapsed;
            let smoothed = if slot.2 == 0.0 { instant } else { slot.2 * 0.65 + instant * 0.35 };
            *slot = (now, done, smoothed);
        }
        if active {
            slot.2
        } else {
            0.0
        }
    }

    fn forget_missing(&mut self, live: &[u64]) {
        self.marks.retain(|id, _| live.contains(id));
    }
}

fn snapshot(app: &App, meter: &mut Meter, maximized: bool) -> StateView {
    let mut jobs = app.jobs.lock().unwrap().clone();
    // newest first: the download you just started is the one you want to see
    jobs.sort_by(|a, b| b.id.cmp(&a.id));
    let live: Vec<u64> = jobs.iter().map(|j| j.id).collect();
    meter.forget_missing(&live);

    let mut views = Vec::with_capacity(jobs.len());
    let (mut active, mut rate) = (0usize, 0.0f64);
    for job in jobs.iter() {
        let state = job.status();
        let done = job.done.load(Ordering::Relaxed);
        let total = job.total.load(Ordering::Relaxed);
        let downloading = state == Status::Downloading;
        let speed = meter.sample(job.id, done, downloading);
        if downloading {
            active += 1;
            rate += speed;
        }
        let eta = if total > 0 && speed > 512.0 {
            (total.saturating_sub(done)) as f64 / speed
        } else {
            0.0
        };
        // only while there is something to watch: a finished file is one bar
        let spans = if downloading && total > 0 {
            job.spans
                .lock()
                .unwrap()
                .iter()
                .map(|(start, pos, end)| {
                    let scale = total as f64;
                    [*start as f64 / scale, *pos as f64 / scale, (*end as f64 + 1.0) / scale]
                })
                .collect()
        } else {
            Vec::new()
        };
        views.push(JobView {
            id: job.id,
            url: job.url.clone(),
            spans,
            name: job.name.lock().unwrap().clone(),
            total,
            done,
            status: state.label(),
            error: job.error.lock().unwrap().clone(),
            speed,
            eta,
        });
    }
    let config = app.config.lock().unwrap();
    StateView {
        jobs: views,
        active,
        rate,
        dir: config.dir.clone(),
        connections: config.connections,
        parallel: config.parallel,
        maximized,
        autostart: crate::is_autostart(),
    }
}

fn direction_for(name: &str) -> Option<ResizeDirection> {
    Some(match name {
        "n" => ResizeDirection::North,
        "s" => ResizeDirection::South,
        "e" => ResizeDirection::East,
        "w" => ResizeDirection::West,
        "ne" => ResizeDirection::NorthEast,
        "nw" => ResizeDirection::NorthWest,
        "se" => ResizeDirection::SouthEast,
        "sw" => ResizeDirection::SouthWest,
        _ => return None,
    })
}

/// Show a path in the system's file manager, selecting it where that is a
/// thing the file manager can do.
fn open_in_explorer(path: &str, select: bool) {
    #[cfg(windows)]
    {
        let mut command = std::process::Command::new("explorer");
        if select {
            command.arg("/select,");
        }
        let _ = command.arg(path).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = std::process::Command::new("open");
        if select {
            command.arg("-R"); // reveal it in Finder rather than open it
        }
        let _ = command.arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // No portable "select this file" across Linux file managers, so the
        // folder is what gets opened.
        let target = if select {
            std::path::Path::new(path)
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string())
        } else {
            path.to_string()
        };
        let _ = std::process::Command::new("xdg-open").arg(target).spawn();
    }
}

/// Hand a link to whatever the desktop uses to open links.
fn open_externally(url: &str) {
    #[cfg(windows)]
    let opener = "explorer";
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let opener = "xdg-open";
    let _ = std::process::Command::new(opener).arg(url).spawn();
}

pub fn run() {
    let app = App::new();
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let builder = WindowBuilder::new()
        .with_title(crate::APP_NAME)
        .with_inner_size(LogicalSize::new(1020.0, 640.0))
        .with_min_inner_size(LogicalSize::new(760.0, 460.0))
        .with_decorations(false)
        .with_visible(false); // revealed once the page paints, so there is no white flash
    // a drop shadow on a frameless window is Windows' to draw; the other two
    // window managers do it themselves
    #[cfg(windows)]
    let builder = builder.with_undecorated_shadow(true);
    // GTK does not create the underlying window until the widget is shown,
    // and wry needs that handle to attach a webview, so on Linux it is built
    // shown and hidden again a line later. Everywhere else it starts hidden
    // and is revealed once the page has painted.
    #[cfg(all(unix, not(target_os = "macos")))]
    let builder = builder.with_visible(true);
    let window = builder.build(&event_loop).expect("create window");

    let ipc_proxy = proxy.clone();
    // Served over a custom protocol rather than with_html on purpose. with_html
    // lands the page on about:blank, which is not a secure context, and there
    // window.crypto.subtle does not exist -- the paste scraper needs it. A
    // *.localhost origin is trustworthy to Chromium, so this one is secure.
    let page = HTML.replace("{{FONT}}", FONT_B64.trim());
    let webview = WebViewBuilder::new()
        .with_custom_protocol("idm".into(), move |_id, req| {
            let (kind, body) = if req.uri().path().ends_with("scrape.js") {
                ("text/javascript; charset=utf-8", SCRAPE_JS.as_bytes().to_vec())
            } else {
                ("text/html; charset=utf-8", page.clone().into_bytes())
            };
            wry::http::Response::builder()
                .header("Content-Type", kind)
                .body(std::borrow::Cow::Owned(body))
                .expect("serve the page")
        })
        .with_url("idm://localhost/")
        .with_background_color((12, 13, 20, 255))
        .with_ipc_handler(move |request| {
            let _ = ipc_proxy.send_event(UserEvent::Command(request.into_body()));
        })
        .build(&window)
        .expect("create webview");
    // see above: it was only shown so GTK would make the window real
    #[cfg(all(unix, not(target_os = "macos")))]
    window.set_visible(false);

    // The app lives in the tray so a captured download opens instantly
    // instead of paying for a cold start.
    let tray_menu = Menu::new();
    let open_item = MenuItem::new("Open IDM-lite", true, None);
    let folder_item = MenuItem::new("Open download folder", true, None);
    let quit_item = MenuItem::new("Exit", true, None);
    let _ = tray_menu.append(&open_item);
    let _ = tray_menu.append(&PredefinedMenuItem::separator());
    let _ = tray_menu.append(&folder_item);
    let _ = tray_menu.append(&PredefinedMenuItem::separator());
    let _ = tray_menu.append(&quit_item);
    let (open_id, folder_id, quit_id) =
        (open_item.id().clone(), folder_item.id().clone(), quit_item.id().clone());

    let tray = TrayImage::from_rgba(TRAY_RGBA.to_vec(), TRAY_PX, TRAY_PX)
        .ok()
        .and_then(|icon| {
            TrayIconBuilder::new()
                .with_menu(Box::new(tray_menu))
                .with_tooltip(crate::APP_NAME)
                .with_icon(icon)
                .build()
                .ok()
        });

    let menu_proxy = proxy.clone();
    std::thread::spawn(move || {
        while let Ok(event) = MenuEvent::receiver().recv() {
            if menu_proxy.send_event(UserEvent::TrayMenu(event.id)).is_err() {
                return;
            }
        }
    });
    let tray_proxy = proxy.clone();
    std::thread::spawn(move || {
        while let Ok(event) = TrayIconEvent::receiver().recv() {
            let wants_open = matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
            );
            if wants_open && tray_proxy.send_event(UserEvent::TrayOpen).is_err() {
                return;
            }
        }
    });

    // captures arriving from the browser host stub
    let wake = proxy.clone();
    if let Err(e) = crate::serve_ipc(app.clone(), Box::new(move || {
        let _ = wake.send_event(UserEvent::Captured);
    })) {
        eprintln!("loopback port unavailable: {}", e);
    }
    if !crate::is_registered() {
        crate::register();
    }

    // state pump: brisk while something moves, lazy when nothing does
    let tick_app = app.clone();
    let tick_proxy = proxy.clone();
    std::thread::spawn(move || loop {
        let busy = tick_app
            .jobs
            .lock()
            .unwrap()
            .iter()
            .any(|j| j.status().active());
        std::thread::sleep(Duration::from_millis(if busy { 200 } else { 800 }));
        if tick_proxy.send_event(UserEvent::Tick).is_err() {
            return;
        }
    });

    let scrape_proxy = proxy.clone();
    let mut meter = Meter::default();
    let mut shown = false;
    let mut visible = false;

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;
        let _ = &tray; // keep the icon alive for as long as the loop runs

        let push = |meter: &mut Meter, visible: bool| {
            if !visible {
                return; // nothing is on screen to update
            }
            let state = snapshot(&app, meter, window.is_maximized());
            if let Ok(json) = serde_json::to_string(&state) {
                let _ = webview.evaluate_script(&format!("window.__state({})", json));
            }
        };

        match event {
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                // a download manager belongs in the tray, not gone
                window.set_visible(false);
                visible = false;
            }
            Event::UserEvent(UserEvent::Tick) => push(&mut meter, visible),
            Event::UserEvent(UserEvent::Scraped(json)) => {
                let _ = webview.evaluate_script(&format!("window.__scraped({})", json));
            }
            Event::UserEvent(UserEvent::TrayOpen) => {
                reveal(&window);
                visible = true;
                push(&mut meter, visible);
            }
            Event::UserEvent(UserEvent::TrayMenu(id)) => {
                if id == open_id {
                    reveal(&window);
                    visible = true;
                    push(&mut meter, visible);
                } else if id == folder_id {
                    let dir = app.config.lock().unwrap().dir.clone();
                    let _ = std::fs::create_dir_all(&dir);
                    open_in_explorer(&dir, false);
                } else if id == quit_id {
                    shutdown(&app);
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::Captured) => {
                for capture in app.inbox.lock().unwrap().drain(..).collect::<Vec<_>>() {
                    if !capture.url.is_empty() {
                        app.add(&capture.url, capture.headers(), &capture.filename);
                    }
                }
                reveal(&window);
                visible = true;
                push(&mut meter, visible);
            }
            Event::UserEvent(UserEvent::Command(raw)) => {
                let msg: serde_json::Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let cmd = msg.get("cmd").and_then(|c| c.as_str()).unwrap_or("");
                let ids: Vec<u64> = msg
                    .get("ids")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_u64()).collect())
                    .unwrap_or_default();

                match cmd {
                    "ready" => {
                        if !shown {
                            shown = true;
                            window.set_visible(true);
                            visible = true;
                        }
                        push(&mut meter, visible);
                        return;
                    }
                    "drag" => {
                        let _ = window.drag_window();
                        return;
                    }
                    "resize" => {
                        if let Some(dir) =
                            msg.get("dir").and_then(|d| d.as_str()).and_then(direction_for)
                        {
                            let _ = window.drag_resize_window(dir);
                        }
                        return;
                    }
                    "minimize" => {
                        window.set_minimized(true);
                        return;
                    }
                    "maximize" => {
                        window.set_fullscreen(None::<Fullscreen>);
                        window.set_maximized(!window.is_maximized());
                    }
                    "close" => {
                        window.set_visible(false);
                        visible = false;
                        return;
                    }
                    "quit" => {
                        shutdown(&app);
                        *control_flow = ControlFlow::Exit;
                        return;
                    }
                    "add" => {
                        if let Some(url) = msg.get("url").and_then(|u| u.as_str()) {
                            app.add(url.trim(), vec![browser_ua()], "");
                        }
                    }
                    // one link, many files: fetch the page off the UI thread and
                    // let the picker decide what to do with what is in it
                    "scrape" => {
                        if let Some(url) = msg.get("url").and_then(|u| u.as_str()) {
                            let url = url.trim().to_string();
                            let back = scrape_proxy.clone();
                            std::thread::spawn(move || {
                                let found = match crate::download::fetch_text(&url) {
                                    Ok(body) => serde_json::json!({ "url": url, "body": body }),
                                    Err(e) => serde_json::json!({ "url": url, "error": e }),
                                };
                                let _ = back.send_event(UserEvent::Scraped(found.to_string()));
                            });
                        }
                    }
                    "add-many" => {
                        if let Some(list) = msg.get("urls").and_then(|v| v.as_array()) {
                            for item in list {
                                let url = item.get("url").and_then(|u| u.as_str()).unwrap_or("");
                                let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("");
                                app.add(url.trim(), vec![browser_ua()], name);
                            }
                        }
                    }
                    "clear-finished" => app.clear_finished(),
                    "pause" => ids.iter().for_each(|id| app.pause(*id)),
                    "resume" => ids.iter().for_each(|id| app.resume(*id)),
                    "remove" => {
                        ids.iter().for_each(|id| app.remove(*id));
                    }
                    "reveal" => {
                        for id in &ids {
                            if let Some(job) = app.find(*id) {
                                let path = job.path.lock().unwrap().clone();
                                if !path.is_empty() && std::path::Path::new(&path).exists() {
                                    open_in_explorer(&path, true);
                                }
                            }
                        }
                    }
                    "open" => {
                        for id in &ids {
                            if let Some(job) = app.find(*id) {
                                let path = job.path.lock().unwrap().clone();
                                if job.status() == Status::Done
                                    && std::path::Path::new(&path).exists()
                                {
                                    open_in_explorer(&path, false);
                                }
                            }
                        }
                    }
                    "link" => {
                        // one argument, never a shell string
                        if let Some(url) = msg.get("url").and_then(|u| u.as_str()) {
                            if safe_link(url) {
                                open_externally(url);
                            }
                        }
                        return;
                    }
                    "folder" => {
                        let dir = app.config.lock().unwrap().dir.clone();
                        let _ = std::fs::create_dir_all(&dir);
                        open_in_explorer(&dir, false);
                        return;
                    }
                    "pick" => {
                        let current = app.config.lock().unwrap().dir.clone();
                        if let Some(picked) =
                            rfd::FileDialog::new().set_directory(&current).pick_folder()
                        {
                            let mut config = app.config.lock().unwrap();
                            config.dir = picked.to_string_lossy().into_owned();
                            config.save();
                        }
                    }
                    "settings" => {
                        let mut config = app.config.lock().unwrap();
                        if let Some(dir) = msg.get("dir").and_then(|d| d.as_str()) {
                            if !dir.trim().is_empty() {
                                config.dir = dir.to_string();
                            }
                        }
                        if let Some(n) = msg.get("connections").and_then(|v| v.as_u64()) {
                            config.connections = n.clamp(1, 32);
                        }
                        if let Some(n) = msg.get("parallel").and_then(|v| v.as_u64()) {
                            config.parallel = (n as usize).clamp(1, 10);
                        }
                        config.save();
                        drop(config);
                        if let Some(on) = msg.get("autostart").and_then(|v| v.as_bool()) {
                            crate::set_autostart(on);
                        }
                    }
                    "register" => {
                        crate::register();
                    }
                    _ => return,
                }
                push(&mut meter, visible);
            }
            _ => {}
        }
    });
}

fn shutdown(app: &Arc<App>) {
    for job in app.jobs.lock().unwrap().iter() {
        job.stop.store(true, Ordering::Relaxed);
    }
    // let workers flush their resume sidecars before the process goes away
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline
        && app.jobs.lock().unwrap().iter().any(|j| j.status() == Status::Downloading)
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    app.save();
    let _ = std::fs::remove_file(crate::data_file("ipc.json"));
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_reports_zero_for_idle_jobs() {
        let mut meter = Meter::default();
        meter.sample(1, 0, true);
        std::thread::sleep(Duration::from_millis(400));
        let moving = meter.sample(1, 1_000_000, true);
        assert!(moving > 0.0, "a job that advanced should report a rate");
        assert_eq!(meter.sample(1, 1_000_000, false), 0.0, "paused jobs show no speed");
    }

    #[test]
    fn meter_forgets_removed_jobs() {
        let mut meter = Meter::default();
        meter.sample(1, 10, true);
        meter.sample(2, 10, true);
        meter.forget_missing(&[2]);
        assert!(!meter.marks.contains_key(&1));
        assert!(meter.marks.contains_key(&2));
    }

    #[test]
    fn resize_directions_cover_all_eight_edges() {
        for name in ["n", "s", "e", "w", "ne", "nw", "se", "sw"] {
            assert!(direction_for(name).is_some(), "missing {}", name);
        }
        assert!(direction_for("middle").is_none());
    }
}

