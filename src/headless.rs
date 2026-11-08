//! The app, without a window.
//!
//! The window is WebView2 and Windows-only for now. Everything underneath it
//! — the segmented engine, the native messaging host, registration — is not,
//! so on macOS and Linux the app runs headless: the browser extension captures
//! a download, the host stub hands it over, and this reports progress on
//! stdout while the engine does the work.
//!
//! Quit with Ctrl-C; downloads resume from their sidecars next time.

use crate::download::Status;
use crate::App;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.1} {}", size, UNITS[unit])
    }
}

pub fn run() {
    let app = App::new();

    if let Err(e) = crate::serve_ipc(app.clone(), Box::new(|| {})) {
        eprintln!("{}: loopback port unavailable: {}", crate::APP_NAME, e);
        return;
    }
    if !crate::is_registered() {
        let done = crate::register();
        if !done.is_empty() {
            println!("registered with {}", done.join(", "));
        }
    }

    println!(
        "{} is running. Downloads land in {}.",
        crate::APP_NAME,
        app.config.lock().unwrap().dir
    );
    println!("Start a download in your browser, or quit with Ctrl-C.");

    // Captures arrive in the inbox; nothing wakes us, so we look.
    let mut said: Vec<(u64, Status, u64)> = Vec::new();
    loop {
        for capture in app.inbox.lock().unwrap().drain(..).collect::<Vec<_>>() {
            if !capture.url.is_empty() {
                app.add(&capture.url, capture.headers(), &capture.filename);
            }
        }

        // one line per change, rather than a cursor dance no log would keep
        for job in app.jobs.lock().unwrap().iter() {
            let state = job.status();
            let done = job.done.load(Ordering::Relaxed);
            let total = job.total.load(Ordering::Relaxed);
            let step = if total > 0 { done * 20 / total } else { 0 };
            let seen = said.iter().position(|(id, _, _)| *id == job.id);
            let fresh = match seen {
                Some(i) => said[i].1 != state || said[i].2 != step,
                None => true,
            };
            if !fresh {
                continue;
            }
            match seen {
                Some(i) => said[i] = (job.id, state, step),
                None => said.push((job.id, state, step)),
            }
            let name = job.name.lock().unwrap().clone();
            let name = if name.is_empty() { job.url.clone() } else { name };
            match state {
                Status::Downloading if total > 0 => println!(
                    "  {}  {}%  {} of {}",
                    name,
                    done * 100 / total,
                    human(done),
                    human(total)
                ),
                Status::Downloading => println!("  {}  {}", name, human(done)),
                Status::Done => println!("  {}  done, {}", name, human(done)),
                Status::Error => {
                    println!("  {}  failed: {}", name, job.error.lock().unwrap())
                }
                other => println!("  {}  {}", name, other.label()),
            }
        }

        std::thread::sleep(Duration::from_millis(400));
    }
}

/// Keeps the Arc import honest on platforms that compile this file.
#[allow(dead_code)]
fn _assert_shared(_: Arc<App>) {}
