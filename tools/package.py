"""Assemble whatever this platform ships.

    python tools/package.py

Windows  ->  dist/IDM-lite/, a setup exe and a zip
macOS    ->  dist/IDM-lite.app, zipped, and a .dmg
Linux    ->  a .tar.gz with an install script, and a .deb

Nothing here cross-compiles; each platform builds its own. CI runs this on all
three and uploads what comes out.
"""
import os
import platform
import shutil
import stat
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "tools"))

WINDOWS = platform.system() == "Windows"
MACOS = platform.system() == "Darwin"
SUFFIX = ".exe" if WINDOWS else ""
EXE = os.path.join(ROOT, "target", "release", "IDM-lite" + SUFFIX)
UPDATER = os.path.join(ROOT, "target", "release", "update" + SUFFIX)
DIST = os.path.join(ROOT, "dist", "IDM-lite")


def version():
    """Cargo.toml is the one place the version is written."""
    with open(os.path.join(ROOT, "Cargo.toml")) as f:
        for line in f:
            if line.startswith("version"):
                return line.split('"')[1]
    sys.exit("no version in Cargo.toml")

INSTALL = """@echo off
title IDM-lite setup
echo Registering IDM-lite with Chrome / Edge / Brave...
rem start /wait: IDM-lite is a GUI-subsystem exe, so cmd would not wait for it
start /wait "" "%~dp0IDM-lite.exe" --register
if errorlevel 1 (echo   FAILED - no supported browser found in the registry.) else (echo   done.)
echo.
echo Last step - load the browser extension:
echo    1^) open  chrome://extensions
echo    2^) switch on "Developer mode" (top right)
echo    3^) click "Load unpacked" and choose this folder:
echo         %~dp0extension
echo.
echo Then start IDM-lite.exe, or just download something.
echo.
echo To update later, run update.exe from this folder.
pause
"""

UNINSTALL = """@echo off
"%~dp0IDM-lite.exe" --unregister
echo Removed. Delete this folder, and the extension from chrome://extensions.
pause
"""


def build_clean():
    """Release build with absolute paths stripped.

    rustc bakes the source path of every panic site into the binary, including
    ones from crates under CARGO_HOME, so a plain `cargo build` ships the
    builder's home directory to everyone who downloads the exe. Remapping the
    prefixes keeps the username out without hardcoding it in a tracked file.
    """
    cargo_home = os.environ.get("CARGO_HOME") or os.path.join(
        os.path.expanduser("~"), ".cargo")
    # CARGO_ENCODED_RUSTFLAGS, not RUSTFLAGS: the latter splits on spaces, and
    # a Windows home directory very often contains one.
    flags = "\x1f".join([
        "--remap-path-prefix=%s=/cargo" % cargo_home,
        "--remap-path-prefix=%s=/idm-lite" % ROOT,
    ])
    env = dict(os.environ, CARGO_ENCODED_RUSTFLAGS=flags)
    env.pop("RUSTFLAGS", None)
    print("building with remapped paths...")
    subprocess.run(["cargo", "build", "--release"], cwd=ROOT, env=env, check=True)

    for built in (EXE, UPDATER):
        with open(built, "rb") as f:
            blob = f.read()
        for leak in (cargo_home, ROOT, os.path.expanduser("~")):
            if leak.encode("ascii", "ignore") in blob:
                sys.exit("refusing to package: %r is embedded in %s"
                         % (leak, os.path.basename(built)))
    print("binaries carry no local paths")


def build_installer():
    """Stage the payload, then build the single file setup exe around it.

    The installer is its own crate precisely so that a plain `cargo build` at
    the repo root never depends on a staged payload that may not exist yet.
    """
    payload = os.path.join(ROOT, "dist", "payload")
    shutil.rmtree(payload, ignore_errors=True)
    os.makedirs(payload)
    shutil.copy2(EXE, payload)
    shutil.copy2(UPDATER, payload)
    shutil.copytree(os.path.join(ROOT, "extension"),
                    os.path.join(payload, "extension"),
                    ignore=shutil.ignore_patterns("*.pem"))

    crate = os.path.join(ROOT, "installer")
    cargo_home = os.environ.get("CARGO_HOME") or os.path.join(
        os.path.expanduser("~"), ".cargo")
    env = dict(os.environ, CARGO_ENCODED_RUSTFLAGS="\x1f".join([
        "--remap-path-prefix=%s=/cargo" % cargo_home,
        "--remap-path-prefix=%s=/idm-lite" % ROOT,
    ]))
    env.pop("RUSTFLAGS", None)
    print("building the installer...")
    subprocess.run(["cargo", "build", "--release"], cwd=crate, env=env, check=True)

    built = os.path.join(crate, "target", "release", "IDM-lite-Setup.exe")
    with open(built, "rb") as f:
        blob = f.read()
    for leak in (cargo_home, ROOT, os.path.expanduser("~")):
        if leak.encode("ascii", "ignore") in blob:
            sys.exit("refusing to ship installer: %r is embedded in it" % leak)
    shutil.copy2(built, os.path.join(
        ROOT, "dist", "IDM-lite-Setup-%s.exe" % version()))
    print("installer carries no local paths")


def icon_png(size):
    """The app icon at any size; it is drawn, not stored."""
    import make_icons

    return make_icons.png_bytes(size)


def write(path, data, executable=False):
    """Write a file, making its folder, and mark it runnable when asked."""
    parent = os.path.dirname(path)
    if parent:
        os.makedirs(parent, exist_ok=True)
    with open(path, "wb" if isinstance(data, bytes) else "w") as f:
        f.write(data)
    if executable:
        os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)


def runnable(path):
    os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)


# --------------------------------------------------------------------- macOS

PLIST = """<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>IDM-lite</string>
  <key>CFBundleDisplayName</key><string>IDM-lite</string>
  <key>CFBundleIdentifier</key><string>com.idmlite.app</string>
  <key>CFBundleVersion</key><string>%(version)s</string>
  <key>CFBundleShortVersionString</key><string>%(version)s</string>
  <key>CFBundleExecutable</key><string>IDM-lite</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>10.15</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
"""


def package_macos():
    app = os.path.join(ROOT, "dist", "IDM-lite.app")
    shutil.rmtree(app, ignore_errors=True)
    macos_dir = os.path.join(app, "Contents", "MacOS")
    resources = os.path.join(app, "Contents", "Resources")
    os.makedirs(macos_dir)
    os.makedirs(resources)

    shutil.copy2(EXE, os.path.join(macos_dir, "IDM-lite"))
    shutil.copy2(UPDATER, os.path.join(macos_dir, "update"))
    for name in ("IDM-lite", "update"):
        runnable(os.path.join(macos_dir, name))
    # the extension rides along so "Load unpacked" has somewhere to point
    shutil.copytree(os.path.join(ROOT, "extension"), os.path.join(resources, "extension"),
                    ignore=shutil.ignore_patterns("*.pem"))
    write(os.path.join(app, "Contents", "Info.plist"), PLIST % {"version": version()})

    # iconutil wants an .iconset holding the sizes macOS actually asks for
    iconset = os.path.join(ROOT, "dist", "AppIcon.iconset")
    shutil.rmtree(iconset, ignore_errors=True)
    os.makedirs(iconset)
    for size in (16, 32, 128, 256, 512):
        write(os.path.join(iconset, "icon_%dx%d.png" % (size, size)), icon_png(size))
        write(os.path.join(iconset, "icon_%dx%d@2x.png" % (size, size)), icon_png(size * 2))
    icns = os.path.join(resources, "AppIcon.icns")
    if subprocess.run(["iconutil", "-c", "icns", iconset, "-o", icns]).returncode != 0:
        print("  (iconutil failed; the bundle ships without an icon)")
    shutil.rmtree(iconset, ignore_errors=True)

    archive = os.path.join(ROOT, "dist", "IDM-lite-%s-macos.zip" % version())
    if os.path.exists(archive):
        os.remove(archive)
    # ditto preserves the bundle's permissions and symlinks; plain zip does not
    subprocess.run(["ditto", "-c", "-k", "--keepParent", app, archive], check=True)
    print("bundle   ->", app)
    print("release  ->", archive)

    dmg = os.path.join(ROOT, "dist", "IDM-lite-%s.dmg" % version())
    if os.path.exists(dmg):
        os.remove(dmg)
    staged = os.path.join(ROOT, "dist", "dmg")
    shutil.rmtree(staged, ignore_errors=True)
    os.makedirs(staged)
    subprocess.run(["cp", "-R", app, staged], check=True)
    os.symlink("/Applications", os.path.join(staged, "Applications"))
    made = subprocess.run(["hdiutil", "create", "-volname", "IDM-lite",
                           "-srcfolder", staged, "-ov", "-format", "UDZO", dmg])
    shutil.rmtree(staged, ignore_errors=True)
    if made.returncode == 0:
        print("release  ->", dmg)


# --------------------------------------------------------------------- Linux

DESKTOP = """[Desktop Entry]
Type=Application
Name=IDM-lite
GenericName=Download Manager
Comment=Splits a download across many connections and finishes it
Exec=idm-lite
Icon=idm-lite
Terminal=false
Categories=Network;FileTransfer;
"""

INSTALL_SH = """#!/bin/sh
# Installs for this user only: no root, nothing outside your home.
set -e
here=$(cd "$(dirname "$0")" && pwd)
bin="$HOME/.local/bin"
share="$HOME/.local/share"
mkdir -p "$bin" "$share/idm-lite" "$share/applications" "$share/icons/hicolor/256x256/apps"

install -m 755 "$here/idm-lite" "$bin/idm-lite"
install -m 755 "$here/idm-lite-update" "$bin/idm-lite-update"
rm -rf "$share/idm-lite/extension"
cp -R "$here/extension" "$share/idm-lite/extension"
install -m 644 "$here/idm-lite.desktop" "$share/applications/idm-lite.desktop"
install -m 644 "$here/idm-lite.png" "$share/icons/hicolor/256x256/apps/idm-lite.png"

"$bin/idm-lite" --register || true

echo
echo "Installed to $bin/idm-lite"
echo
echo "Last step, load the browser extension:"
echo "  1) open  chrome://extensions"
echo "  2) switch on Developer mode"
echo "  3) Load unpacked, and choose:"
echo "       $share/idm-lite/extension"
echo
echo "If $bin is not on your PATH, add it."
"""

UNINSTALL_SH = """#!/bin/sh
set -e
"$HOME/.local/bin/idm-lite" --unregister || true
rm -f "$HOME/.local/bin/idm-lite" "$HOME/.local/bin/idm-lite-update"
rm -rf "$HOME/.local/share/idm-lite"
rm -f "$HOME/.local/share/applications/idm-lite.desktop"
rm -f "$HOME/.local/share/icons/hicolor/256x256/apps/idm-lite.png"
echo "Removed. Your downloads were not touched."
"""

DEB_CONTROL = """Package: idm-lite
Version: %(version)s
Section: net
Priority: optional
Architecture: amd64
Depends: libwebkit2gtk-4.1-0, libgtk-3-0, libayatana-appindicator3-1
Maintainer: Tova <tovaisme@users.noreply.github.com>
Homepage: https://tovaisme.github.io/idm-lite/
Description: Download manager that finishes the file
 Splits a download across up to 32 connections, hands a stalled
 connection's work to an idle one, and resumes exactly where it stopped.
 A browser extension captures downloads and passes them over.
"""


def package_linux():
    stage = os.path.join(ROOT, "dist", "idm-lite")
    shutil.rmtree(stage, ignore_errors=True)
    os.makedirs(stage)
    shutil.copy2(EXE, os.path.join(stage, "idm-lite"))
    shutil.copy2(UPDATER, os.path.join(stage, "idm-lite-update"))
    for name in ("idm-lite", "idm-lite-update"):
        runnable(os.path.join(stage, name))
    shutil.copytree(os.path.join(ROOT, "extension"), os.path.join(stage, "extension"),
                    ignore=shutil.ignore_patterns("*.pem"))
    write(os.path.join(stage, "idm-lite.desktop"), DESKTOP)
    write(os.path.join(stage, "idm-lite.png"), icon_png(256))
    write(os.path.join(stage, "install.sh"), INSTALL_SH, executable=True)
    write(os.path.join(stage, "uninstall.sh"), UNINSTALL_SH, executable=True)

    tarball = os.path.join(ROOT, "dist", "idm-lite-%s-linux-x86_64.tar.gz" % version())
    if os.path.exists(tarball):
        os.remove(tarball)
    subprocess.run(["tar", "-czf", tarball, "-C", os.path.join(ROOT, "dist"), "idm-lite"],
                   check=True)
    print("release  ->", tarball)
    build_deb()


def build_deb():
    if not shutil.which("dpkg-deb"):
        print("  (no dpkg-deb; skipping the .deb)")
        return
    root = os.path.join(ROOT, "dist", "deb")
    shutil.rmtree(root, ignore_errors=True)
    write(os.path.join(root, "DEBIAN", "control"), DEB_CONTROL % {"version": version()})

    binaries = os.path.join(root, "usr", "bin")
    os.makedirs(binaries, exist_ok=True)
    shutil.copy2(EXE, os.path.join(binaries, "idm-lite"))
    shutil.copy2(UPDATER, os.path.join(binaries, "idm-lite-update"))
    for name in ("idm-lite", "idm-lite-update"):
        os.chmod(os.path.join(binaries, name), 0o755)

    shutil.copytree(os.path.join(ROOT, "extension"),
                    os.path.join(root, "usr", "share", "idm-lite", "extension"),
                    ignore=shutil.ignore_patterns("*.pem"))
    write(os.path.join(root, "usr", "share", "applications", "idm-lite.desktop"), DESKTOP)
    write(os.path.join(root, "usr", "share", "icons", "hicolor", "256x256", "apps",
                       "idm-lite.png"), icon_png(256))

    deb = os.path.join(ROOT, "dist", "idm-lite_%s_amd64.deb" % version())
    if subprocess.run(["dpkg-deb", "--build", "--root-owner-group", root, deb]).returncode == 0:
        print("release  ->", deb)
    shutil.rmtree(root, ignore_errors=True)


def main():
    build_clean()
    if not os.path.exists(EXE):
        sys.exit("build it first:  cargo build --release")

    # each platform ships something different, and only Windows has an installer
    if not WINDOWS:
        package_macos() if MACOS else package_linux()
        return

    if os.path.isdir(DIST):
        shutil.rmtree(DIST)
    os.makedirs(DIST)
    shutil.copy2(EXE, DIST)
    shutil.copy2(UPDATER, DIST)
    shutil.copytree(os.path.join(ROOT, "extension"), os.path.join(DIST, "extension"),
                    ignore=shutil.ignore_patterns("*.pem"))
    for name, text in (("Install.bat", INSTALL), ("Uninstall.bat", UNINSTALL)):
        with open(os.path.join(DIST, name), "w") as f:
            f.write(text)

    build_installer()

    archive = os.path.join(ROOT, "dist", "IDM-lite-%s-windows-x64" % version())
    shutil.make_archive(archive, "zip", root_dir=os.path.join(ROOT, "dist"),
                        base_dir="IDM-lite")
    print("release  ->", archive + ".zip",
          "(%.1f KB)" % (os.path.getsize(archive + ".zip") / 1024))
    print("packaged ->", DIST)
    total = 0
    for entry in sorted(os.listdir(DIST)):
        full = os.path.join(DIST, entry)
        if os.path.isdir(full):
            size = sum(os.path.getsize(os.path.join(full, f)) for f in os.listdir(full))
            print("  %-18s <dir>  %6.1f KB" % (entry, size / 1024))
        else:
            size = os.path.getsize(full)
            print("  %-18s        %6.1f KB" % (entry, size / 1024))
        total += size
    print("  %-18s        %6.1f KB total" % ("", total / 1024))


if __name__ == "__main__":
    main()
