"""Black-box test of the built IDM-lite.exe: browser frame -> app -> file on disk.

    python tests\\smoke.py [path\\to\\IDM-lite.exe]

Uses a throwaway IDMLITE_DATA and undoes the registration it triggers, so it
leaves nothing behind.
"""
import http.server
import json
import os
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXE = os.path.abspath(
    sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target", "release", "IDM-lite.exe"))
EXT_ID = "ffmenbaflpdmmlfigjodjcnajmlgndgl"
KEY = r"Software\Google\Chrome\NativeMessagingHosts\com.idmlite.host"
ALL_KEYS = [
    r"Software\Google\Chrome\NativeMessagingHosts\com.idmlite.host",
    r"Software\Microsoft\Edge\NativeMessagingHosts\com.idmlite.host",
    r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.idmlite.host",
    r"Software\Chromium\NativeMessagingHosts\com.idmlite.host",
]


# Registry access goes through reg.exe on purpose. Microsoft Store Python
# runs in an app container whose registry writes are virtualized: winreg would
# happily read back its own sandboxed copy while the real hive said otherwise,
# which makes every assertion here a lie. reg.exe is a plain native binary.
def reg_get(key):
    r = subprocess.run(["reg", "query", "HKCU\\" + key, "/ve"],
                       capture_output=True, text=True)
    if r.returncode != 0:
        return None
    for line in r.stdout.splitlines():
        if "REG_SZ" in line:
            return line.split("REG_SZ", 1)[1].strip()
    return None


def reg_set(key, value):
    subprocess.run(["reg", "add", "HKCU\\" + key, "/ve", "/t", "REG_SZ",
                    "/d", value, "/f"], capture_output=True)


def reg_delete(key):
    subprocess.run(["reg", "delete", "HKCU\\" + key, "/f"], capture_output=True)


def snapshot_registration():
    """This test registers and unregisters the host, which would otherwise
    wipe a real installation. Remember what was there so it can be put back."""
    return {key: reg_get(key) for key in ALL_KEYS}


def restore_registration(saved):
    for key, value in saved.items():
        if value is None:
            reg_delete(key)
        else:
            reg_set(key, value)
BLOB = os.urandom(5 << 20)
seen = {}


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def do_GET(self):
        seen.update({h: self.headers.get(h) for h in ("Cookie", "Referer", "User-Agent")})
        rng = self.headers.get("Range")
        if rng:
            lo, _, hi = rng.split("=", 1)[1].partition("-")
            lo, hi = int(lo), int(hi) if hi else len(BLOB) - 1
            body = BLOB[lo:hi + 1]
            self.send_response(206)
            self.send_header("Content-Range", "bytes %d-%d/%d" % (lo, hi, len(BLOB)))
        else:
            body = BLOB
            self.send_response(200)
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Disposition", 'attachment; filename="setup bundle.bin"')
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Server(http.server.ThreadingHTTPServer):
    # socketserver defaults to a backlog of 5; 8 parallel segments plus the
    # probe overflow it and the kernel resets the extra connections.
    request_queue_size = 64

    def handle_error(self, request, client_address):
        pass  # the client closes probe connections early; not a failure


def framed(obj):
    data = json.dumps(obj).encode()
    return struct.pack("<I", len(data)) + data


def main():
    assert os.path.exists(EXE), "build it first: cargo build --release\nmissing " + EXE
    data = tempfile.mkdtemp(prefix="idmlite-smoke-")
    saves = os.path.join(data, "files")
    os.makedirs(saves)
    with open(os.path.join(data, "config.json"), "w") as f:
        json.dump({"dir": saves, "connections": 8, "parallel": 3}, f)
    env = dict(os.environ, IDMLITE_DATA=data)

    srv = Server(("127.0.0.1", 0), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    url = "http://127.0.0.1:%d/get?id=99" % srv.server_address[1]

    print("1. feeding the native-messaging stub a captured download")
    msg = framed({"url": url, "cookie": "sid=abc", "referrer": "http://page/",
                  "ua": "Chrome-test/1", "filename": ""})
    host = subprocess.Popen([EXE, "chrome-extension://%s/" % EXT_ID], env=env,
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    host.stdin.write(msg)
    host.stdin.flush()
    head = host.stdout.read(4)
    assert len(head) == 4, "stub never replied (stdin broken under windows subsystem?)"
    reply = json.loads(host.stdout.read(struct.unpack("<I", head)[0]))
    print("   stub reply:", reply)
    assert reply == {"accepted": True, "url": url}, reply

    print("2. waiting for the app it launched to finish the download")
    target = os.path.join(saves, "setup bundle.bin")
    deadline = time.time() + 120
    while time.time() < deadline:
        if os.path.exists(target) and os.path.getsize(target) == len(BLOB) \
                and not os.path.exists(target + ".idmstate"):
            break
        time.sleep(0.5)
    with open(target, "rb") as f:
        assert f.read() == BLOB, "downloaded bytes differ from what the server sent"
    print("   got %r, %d bytes, intact" % (os.path.basename(target), len(BLOB)))
    assert seen == {"Cookie": "sid=abc", "Referer": "http://page/",
                    "User-Agent": "Chrome-test/1"}, seen
    print("   browser identity forwarded:", seen)

    print("3. loopback port rejects callers without the token")
    info = json.load(open(os.path.join(data, "ipc.json")))
    for bad in ({"job": {"url": url}}, {"token": "wrong", "job": {"url": url}}):
        with socket.create_connection(("127.0.0.1", info["port"]), 3) as s:
            s.sendall(json.dumps(bad).encode())
            s.shutdown(socket.SHUT_WR)
            assert s.recv(16) == b"", "unauthenticated caller was accepted"
    with socket.create_connection(("127.0.0.1", info["port"]), 3) as s:
        s.sendall(json.dumps({"token": info["token"], "job": {}}).encode())
        s.shutdown(socket.SHUT_WR)
        assert s.recv(16) == b"ok"
    print("   two unauthenticated callers refused, valid token accepted")

    print("4. second launch hands off to the running instance instead of duplicating")
    second = subprocess.run([EXE], env=env, capture_output=True, timeout=30)
    assert second.returncode == 0, second
    assert len(json.load(open(os.path.join(data, "downloads.json")))) == 1

    state = json.load(open(os.path.join(data, "downloads.json")))
    assert state[0]["status"] == "done" and state[0]["name"] == "setup bundle.bin", state
    print("5. app persisted its list:", [(j["name"], j["status"]) for j in state])

    manifest = None
    for _ in range(40):          # a GUI-subsystem exe does not settle at once
        manifest = reg_get(KEY)
        if manifest:
            break
        time.sleep(0.25)
    assert manifest, "app never registered itself in the real registry"
    rec = json.load(open(manifest))
    assert rec["path"] == EXE, rec
    assert rec["allowed_origins"] == ["chrome-extension://%s/" % EXT_ID], rec
    print("6. self-registered, manifest points at the exe and pins the extension id")

    # the extension's pinned key must still hash to the id the binary allows,
    # or the browser would talk to a host that refuses it
    import base64, hashlib
    key = json.load(open(os.path.join(ROOT, "extension", "manifest.json")))["key"]
    digest = hashlib.sha256(base64.b64decode(key)).hexdigest()[:32]
    derived = "".join(chr(97 + int(c, 16)) for c in digest)
    assert derived == EXT_ID, "manifest key hashes to %s, host allows %s" % (derived, EXT_ID)
    print("7. extension key still derives the allowed id", derived)

    host.stdin.close()
    host.wait(10)
    subprocess.run(["taskkill", "/IM", os.path.basename(EXE), "/F"], capture_output=True)
    subprocess.run([EXE, "--unregister"], env=env, capture_output=True)
    for _ in range(40):
        if reg_get(KEY) is None:
            print("8. --unregister cleaned the registry key back out")
            break
        time.sleep(0.25)
    else:
        raise SystemExit("FAIL: registry key survived --unregister")
    srv.shutdown()
    print("\nok")


if __name__ == "__main__":
    previous = snapshot_registration()
    try:
        main()
    finally:
        # let any in-flight register/unregister land before putting things back,
        # or a late write would clobber the restore
        time.sleep(2.0)
        restore_registration(previous)
        if any(previous.values()):
            ok = snapshot_registration() == previous
            print("(restored your existing host registration)" if ok
                  else "WARNING: could not fully restore your host registration")

