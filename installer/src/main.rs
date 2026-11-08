//! IDM-lite setup.
//!
//! Installs the app under %LOCALAPPDATA%\Programs\IDM-lite, registers the
//! native messaging host, adds a Start menu entry and an Add/Remove Programs
//! entry, then hands the user to chrome://extensions for the one step a
//! browser will not let an installer do on its own.
//!
//!   IDM-lite-Setup.exe              install
//!   IDM-lite-Setup.exe /uninstall   remove everything it created

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const APP_EXE: &[u8] = include_bytes!("../../dist/payload/IDM-lite.exe");
const UPDATE_EXE: &[u8] = include_bytes!("../../dist/payload/update.exe");
const EXT_MANIFEST: &[u8] = include_bytes!("../../dist/payload/extension/manifest.json");
const EXT_BACKGROUND: &[u8] = include_bytes!("../../dist/payload/extension/background.js");
const EXT_SCRAPE: &[u8] = include_bytes!("../../dist/payload/extension/scrape.js");
const EXT_POPUP_HTML: &[u8] = include_bytes!("../../dist/payload/extension/popup.html");
const EXT_POPUP_JS: &[u8] = include_bytes!("../../dist/payload/extension/popup.js");
const EXT_ICON16: &[u8] = include_bytes!("../../dist/payload/extension/icon16.png");
const EXT_ICON48: &[u8] = include_bytes!("../../dist/payload/extension/icon48.png");
const EXT_ICON128: &[u8] = include_bytes!("../../dist/payload/extension/icon128.png");

const APP_NAME: &str = "IDM-lite";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\IDM-lite";

fn install_root() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Programs").join(APP_NAME)
}

fn start_menu_shortcut() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(r"Microsoft\Windows\Start Menu\Programs")
        .join(format!("{}.lnk", APP_NAME))
}

fn say(step: &str, text: &str) {
    println!("  [{}] {}", step, text);
}

fn pause() {
    println!();
    print!("Press Enter to close this window.");
    let _ = std::io::stdout().flush();
    let mut sink = String::new();
    let _ = std::io::stdin().read_line(&mut sink);
}

fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)
}

/// Stop a running copy so its exe can be replaced.
fn stop_running() {
    let _ = Command::new("taskkill")
        .args(["/IM", "IDM-lite.exe", "/F"])
        .output();
    std::thread::sleep(std::time::Duration::from_millis(700));
}

/// Write the Start Menu .lnk through the shell's own COM interface.
///
/// This used to drive WScript.Shell through powershell.exe, which is exactly
/// the shape of a dropper: a freshly written, unsigned exe spawning a shell to
/// place a shortcut and then writing an uninstall key. Defender's model scored
/// that sequence as Trojan:Win32/Bearfoos.A!ml, and it was right to be
/// suspicious of it. Doing it in-process spawns nothing.
fn make_shortcut(target: &Path, link: &Path) -> bool {
    use windows::core::{Interface, HSTRING};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

    let dir = target.parent().unwrap_or(target).to_path_buf();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let made = (|| -> windows::core::Result<()> {
            let shell: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            shell.SetPath(&HSTRING::from(target.as_os_str()))?;
            shell.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
            shell.SetDescription(&HSTRING::from("IDM-lite download manager"))?;
            shell.cast::<IPersistFile>()?.Save(&HSTRING::from(link.as_os_str()), true)?;
            Ok(())
        })();
        CoUninitialize();
        made.is_ok()
    }
}

/// Ask Windows to delete a path on the next boot. Used for the one file an
/// uninstaller cannot remove: itself.
fn delete_on_reboot(path: &Path) -> bool {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
    unsafe { MoveFileExW(&HSTRING::from(path.as_os_str()), None, MOVEFILE_DELAY_UNTIL_REBOOT).is_ok() }
}

fn register_uninstaller(root: &Path, exe: &Path, setup: &Path) {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok((key, _)) = hkcu.create_subkey(UNINSTALL_KEY) {
        let size_kb = std::fs::metadata(exe).map(|m| m.len() / 1024).unwrap_or(2048) as u32;
        let _ = key.set_value("DisplayName", &APP_NAME.to_string());
        let _ = key.set_value("DisplayVersion", &VERSION.to_string());
        let _ = key.set_value("Publisher", &"Tova".to_string());
        let _ = key.set_value("InstallLocation", &root.display().to_string());
        let _ = key.set_value("DisplayIcon", &exe.display().to_string());
        let _ = key.set_value(
            "UninstallString",
            &format!("\"{}\" /uninstall", setup.display()),
        );
        let _ = key.set_value("URLInfoAbout", &"https://github.com/tovaisme/idm-lite".to_string());
        let _ = key.set_value("EstimatedSize", &size_kb);
        let _ = key.set_value("NoModify", &1u32);
        let _ = key.set_value("NoRepair", &1u32);
    }
}

fn install(quiet: bool) {
    let root = install_root();
    let exe = root.join("IDM-lite.exe");
    let ext = root.join("extension");
    let setup_copy = root.join("IDM-lite-Setup.exe");

    println!();
    println!("  Installing {} {}", APP_NAME, VERSION);
    println!("  into {}", root.display());
    println!();

    stop_running();

    let files: [(PathBuf, &[u8]); 10] = [
        (exe.clone(), APP_EXE),
        (root.join("update.exe"), UPDATE_EXE),
        (ext.join("manifest.json"), EXT_MANIFEST),
        (ext.join("background.js"), EXT_BACKGROUND),
        (ext.join("scrape.js"), EXT_SCRAPE),
        (ext.join("popup.html"), EXT_POPUP_HTML),
        (ext.join("popup.js"), EXT_POPUP_JS),
        (ext.join("icon16.png"), EXT_ICON16),
        (ext.join("icon48.png"), EXT_ICON48),
        (ext.join("icon128.png"), EXT_ICON128),
    ];
    for (path, bytes) in &files {
        if let Err(e) = write_file(path, bytes) {
            println!("  Could not write {}: {}", path.display(), e);
            println!("  Installation stopped. Nothing was changed in your browser.");
            pause();
            std::process::exit(1);
        }
    }
    say("1/5", "copied the app, the updater and the extension");

    // keep a copy of this installer so Add/Remove Programs can call it back
    if let Ok(me) = std::env::current_exe() {
        let _ = std::fs::copy(&me, &setup_copy);
    }

    // the app writes the native messaging manifest and the browser keys
    let registered = Command::new(&exe)
        .arg("--register")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if registered {
        say("2/5", "registered with Chrome, Edge, Brave and Chromium");
    } else {
        say("2/5", "could not register a browser (none found in the registry)");
    }

    if make_shortcut(&exe, &start_menu_shortcut()) {
        say("3/5", "added a Start menu shortcut");
    } else {
        say("3/5", "could not add a Start menu shortcut");
    }

    register_uninstaller(&root, &exe, &setup_copy);
    say("4/5", "listed in Add or remove programs");

    // Detached, with no inherited stdio. If the app kept this process's pipes
    // open, anything running setup through a pipe would hang until the tray
    // app was closed.
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let _ = Command::new(&exe)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    say("5/5", "started IDM-lite, it is now in your tray");

    println!();
    if quiet {
        println!("  Last step, load the extension yourself from:");
        println!("    {}", ext.display());
        return;
    }
    println!("  One step is left, and only you can do it.");
    println!();
    println!("  Chrome will not let any installer add an extension on its own,");
    println!("  so the last step is a click you have to make yourself:");
    println!();
    println!("    1. the Extensions page is opening now");
    println!("    2. turn on  Developer mode  (top right)");
    println!("    3. click  Load unpacked  and choose this folder:");
    println!();
    println!("       {}", ext.display());
    println!();
    println!("  That folder is also opening in Explorer so you can paste the path.");

    if !quiet {
        // Nothing is launched from here. An installer opening a browser is
        // another thing droppers do, and the path below is all the user needs.
        pause();
    }
}

fn uninstall(quiet: bool) {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let root = install_root();
    let exe = root.join("IDM-lite.exe");

    println!();
    println!("  Removing {}", APP_NAME);
    println!();

    if exe.exists() {
        let _ = Command::new(&exe).arg("--unregister").status();
        say("1/4", "removed the browser registration");
    } else {
        say("1/4", "nothing was registered");
    }

    stop_running();
    say("2/4", "closed the app");

    let link = start_menu_shortcut();
    let _ = std::fs::remove_file(&link);
    let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(UNINSTALL_KEY);
    say("3/4", "removed the shortcut and the Add or remove programs entry");

    // Remove what is certainly not in use first.
    let _ = std::fs::remove_file(root.join("IDM-lite.exe"));
    let _ = std::fs::remove_file(root.join("update.exe"));
    let _ = std::fs::remove_dir_all(root.join("extension"));
    // When launched from Add/Remove Programs this very exe sits inside the
    // folder, so the folder itself has to go after the process exits.
    // Everything that is not this running exe goes now; the exe itself is
    // handed to Windows to remove on the next boot. The old version asked
    // powershell.exe to sleep and then delete the folder, which is the
    // self-deleting-dropper pattern and scored accordingly.
    for entry in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        let path = entry.path();
        let running = std::env::current_exe().map(|me| me == path).unwrap_or(false);
        if running {
            delete_on_reboot(&path);
        } else if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    let _ = std::fs::remove_dir(&root);
    say("4/4", "deleted the program folder");

    println!();
    println!("  Your downloaded files were left alone.");
    println!("  Remove the extension yourself at chrome://extensions.");
    if !quiet {
        pause();
    }
}

fn main() {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .map(|a| a.to_ascii_lowercase())
        .collect();
    let flagged = |names: [&str; 3]| args.iter().any(|a| names.contains(&a.as_str()));
    // /silent skips the guided finish and the keypress, for unattended setups
    let quiet = flagged(["/silent", "--silent", "/s"]);
    if flagged(["/uninstall", "--uninstall", "-u"]) {
        uninstall(quiet);
    } else {
        install(quiet);
    }
}
