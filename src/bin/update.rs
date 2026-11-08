//! IDM-lite updater.
//!
//! Asks GitHub for the newest release and, when it is newer than this build,
//! downloads that release's setup exe and runs it. The installer already stops
//! a running copy before it replaces the files, so there is nothing to do here
//! beyond handing it over.
//!
//!   update.exe            check, then install anything newer
//!   update.exe --check    say what is available and stop

use std::io::{Read, Write};

const REPO: &str = "TovaIsME/idm-lite";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version numbers, loosest possible reading: anything unparseable is a 0, so
/// a tag nobody expected can never read as newer than what is installed.
fn parts(tag: &str) -> Vec<u64> {
    tag.trim()
        .trim_start_matches(['v', 'V'])
        .split(['.', '-', '+'])
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

fn newer(remote: &str, local: &str) -> bool {
    parts(remote) > parts(local)
}

struct Release {
    tag: String,
    asset: Option<(String, String)>, // (file name, download url)
    page: String,
}

/// Which asset of a release belongs to the machine asking.
///
/// Windows has an installer that can replace a running app. The others get an
/// archive: unpacking one over a running app is the user's call, not ours, so
/// the updater fetches it and says where it put it.
fn wanted_asset(name: &str) -> bool {
    #[cfg(windows)]
    return name.starts_with("IDM-lite-Setup") && name.ends_with(".exe");
    #[cfg(target_os = "macos")]
    return name.ends_with("-macos.zip") || name.ends_with(".dmg");
    #[cfg(all(unix, not(target_os = "macos")))]
    return name.ends_with("-linux-x86_64.tar.gz") || name.ends_with("_amd64.deb");
}

fn latest() -> Result<Release, String> {
    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(20)))
        .build();
    let agent = ureq::Agent::new_with_config(agent);
    let mut resp = agent
        .get(&format!("https://api.github.com/repos/{}/releases/latest", REPO))
        // GitHub refuses a request without one
        .header("User-Agent", "IDM-lite-updater")
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| e.to_string())?;
    let code = resp.status().as_u16();
    if code == 403 || code == 429 {
        // anonymous API calls are rate limited per address; a shared one
        // (an office, a CI runner) can be out of allowance through no
        // fault of ours
        return Err("GitHub is rate limiting this address. Try again later,                     or get the release from github.com/TovaIsME/idm-lite/releases"
            .to_string());
    }
    if !(200..300).contains(&code) {
        return Err(format!("GitHub answered HTTP {}", code));
    }
    let body = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let json: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let tag = json["tag_name"].as_str().unwrap_or_default().to_string();
    if tag.is_empty() {
        return Err("that release has no tag".into());
    }
    let asset = json["assets"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .find_map(|asset| {
            let name = asset["name"].as_str()?;
            let url = asset["browser_download_url"].as_str()?;
            wanted_asset(name).then(|| (name.to_string(), url.to_string()))
        });
    Ok(Release {
        tag,
        asset,
        page: json["html_url"].as_str().unwrap_or_default().to_string(),
    })
}

fn download(url: &str, name: &str) -> Result<std::path::PathBuf, String> {
    let path = std::env::temp_dir().join(name);
    let agent = ureq::Agent::new_with_defaults();
    let resp = agent
        .get(url)
        .header("User-Agent", "IDM-lite-updater")
        .call()
        .map_err(|e| e.to_string())?;
    let code = resp.status().as_u16();
    if !(200..300).contains(&code) {
        return Err(format!("the download answered HTTP {}", code));
    }
    let mut body = Vec::new();
    resp.into_body()
        .into_reader()
        // an installer is a couple of megabytes; anything near this is wrong
        .take(256 << 20)
        .read_to_end(&mut body)
        .map_err(|e| e.to_string())?;
    std::fs::write(&path, &body).map_err(|e| e.to_string())?;
    Ok(path)
}

fn pause() {
    print!("\nPress Enter to close this window.");
    let _ = std::io::stdout().flush();
    let mut sink = String::new();
    let _ = std::io::stdin().read_line(&mut sink);
}

fn main() {
    let check_only = std::env::args().any(|a| a == "--check");
    println!("IDM-lite {} -- checking for updates...\n", VERSION);

    let release = match latest() {
        Ok(r) => r,
        Err(e) => {
            println!("  could not reach GitHub: {}", e);
            pause();
            std::process::exit(1);
        }
    };

    if !newer(&release.tag, VERSION) {
        println!("  {} is the newest release. Nothing to do.", release.tag);
        pause();
        return;
    }
    println!("  {} is available.", release.tag);

    let (name, url) = match release.asset {
        Some(found) => found,
        None => {
            println!("  ...but it ships nothing for this platform. Get it here:");
            println!("  {}", release.page);
            pause();
            std::process::exit(1);
        }
    };

    if check_only {
        println!("  run update.exe without --check to install it.");
        pause();
        return;
    }

    println!("  downloading {} ...", name);
    let installer = match download(&url, &name) {
        Ok(path) => path,
        Err(e) => {
            println!("  download failed: {}", e);
            pause();
            std::process::exit(1);
        }
    };

    // Only Windows ships something that can install itself over a running app.
    #[cfg(windows)]
    {
        println!("  starting the installer. It will close IDM-lite while it works.");
        match std::process::Command::new(&installer).spawn() {
            // the installer takes it from here, and it cannot replace our own
            // exe while we are still holding the console open on top of it
            Ok(_) => println!("  handed over to {}", installer.display()),
            Err(e) => {
                println!("  could not start the installer: {}", e);
                println!("  it is saved here, run it yourself: {}", installer.display());
                pause();
                std::process::exit(1);
            }
        }
    }
    #[cfg(not(windows))]
    {
        println!("  saved to {}", installer.display());
        println!();
        if name.ends_with(".deb") {
            println!("  install it with:  sudo apt install \"{}\"", installer.display());
        } else if name.ends_with(".tar.gz") {
            println!("  unpack it and run install.sh inside.");
        } else {
            println!("  open it and drag IDM-lite to Applications.");
        }
        pause();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_higher_version_counts_as_newer() {
        assert!(newer("v1.0.2", "1.0.1"));
        assert!(newer("1.1.0", "1.0.9"));
        assert!(newer("v1.0.10", "1.0.9")); // not a string comparison
        assert!(!newer("v1.0.1", "1.0.1"));
        assert!(!newer("v1.0.0", "1.0.1")); // a rolled-back release is not an update
        // a tag in some shape we never agreed on must not trigger an install
        assert!(!newer("nightly", "1.0.1"));
        assert!(!newer("", "1.0.1"));
    }
}
