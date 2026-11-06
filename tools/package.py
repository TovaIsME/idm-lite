"""Assemble a shippable folder: exe + extension + installer.

    cargo build --release && python tools/package.py   ->  dist/IDM-lite/
"""
import os
import shutil
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXE = os.path.join(ROOT, "target", "release", "IDM-lite.exe")
DIST = os.path.join(ROOT, "dist", "IDM-lite")

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

    with open(EXE, "rb") as f:
        blob = f.read()
    for leak in (cargo_home, ROOT, os.path.expanduser("~")):
        if leak.encode("ascii", "ignore") in blob:
            sys.exit("refusing to package: %r is still embedded in the exe" % leak)
    print("binary carries no local paths")


def main():
    build_clean()
    if not os.path.exists(EXE):
        sys.exit("build it first:  cargo build --release")
    if os.path.isdir(DIST):
        shutil.rmtree(DIST)
    os.makedirs(DIST)
    shutil.copy2(EXE, DIST)
    shutil.copytree(os.path.join(ROOT, "extension"), os.path.join(DIST, "extension"),
                    ignore=shutil.ignore_patterns("*.pem"))
    for name, text in (("Install.bat", INSTALL), ("Uninstall.bat", UNINSTALL)):
        with open(os.path.join(DIST, name), "w") as f:
            f.write(text)

    version = "1.0.0"
    archive = os.path.join(ROOT, "dist", "IDM-lite-%s-windows-x64" % version)
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
