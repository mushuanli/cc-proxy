#!/usr/bin/env bash
# Capture the request headers the *real* Codex CLI sends, so they can be diffed
# against what proxy-planx synthesises.
#
#   scripts/capture-cli-headers.sh [prompt]          # non-interactive: codex exec
#   TUI=1 scripts/capture-cli-headers.sh [prompt]    # interactive TUI, driven automatically
#
# Both entrypoints are captured without touching ~/.codex and without spending
# quota. They send *different* identities: the TUI (and the VS Code extension) use
# `codex-tui`, `codex exec` uses `codex_exec` — cc-proxy impersonates the TUI,
# because it serves long-lived interactive sessions.
#
# Nothing leaves the machine and no quota is consumed: the CLI is pointed at a
# local recorder that answers 400 immediately. Your own ~/.codex is never
# touched — the CLI runs against a throwaway CODEX_HOME seeded with a copy of
# auth.json (0600, removed on exit). Authorization values are never printed.
#
# Compare with:  cargo run -p proxy-planx --example identity_headers -- gpt
set -euo pipefail

PORT="${PORT:-18099}"
PROMPT="${1:-hi}"
TMP="$(mktemp -d)"
CAPTURE="$TMP/headers.txt"

cleanup() {
    [[ -n "${REC_PID:-}" ]] && kill -9 "$REC_PID" 2>/dev/null || true
    rm -rf "$TMP"
}
trap cleanup EXIT

# ── isolated codex home ──
chmod 700 "$TMP"
cp "$HOME/.codex/auth.json" "$TMP/auth.json" 2>/dev/null || {
    echo "no ~/.codex/auth.json — run 'codex' once to log in first" >&2; exit 1; }
chmod 600 "$TMP/auth.json"
mkdir -p "$TMP/work"
cat > "$TMP/config.toml" <<EOF
model = "gpt-6.1-sol"
model_reasoning_effort = "medium"
openai_base_url = "http://127.0.0.1:${PORT}/v1"

# The TUI asks before trusting a directory; pre-trust the scratch one.
[projects."$TMP/work"]
trust_level = "trusted"
EOF

# ── local recorder ──
cat > "$TMP/rec.py" <<PY
import http.server, socketserver, json
CAPTURE = "${CAPTURE}"
SECRET = ("authorization", "x-api-key", "cookie")
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _log(self):
        lines = [f"{self.command} {self.path} {self.request_version}"]
        for k, v in self.headers.items():
            if k.lower() in SECRET:
                v = f"<REDACTED len={len(v)}>"
            lines.append(f"  {k}: {v}")
        open(CAPTURE, "a").write("\n".join(lines) + "\n---\n")
    def do_POST(self):
        self._log()
        b = json.dumps({"error": {"message": "recorder", "type": "invalid_request_error"}}).encode()
        self.send_response(400); self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        self._log(); self.send_response(400); self.send_header("content-length", "0"); self.end_headers()
socketserver.TCPServer.allow_reuse_address = True
with socketserver.TCPServer(("127.0.0.1", ${PORT}), H) as s:
    s.serve_forever()
PY
python3 "$TMP/rec.py" >/dev/null 2>&1 &
REC_PID=$!
sleep 1

if [[ "${TUI:-0}" == "1" ]]; then
    echo "==> driving the interactive TUI through a pty (types a prompt, then Ctrl-C)"
    # The TUI needs a terminal: a plain pipe on stdin is ignored. A pty makes the
    # keystrokes below look like real typing, so no manual step is needed.
    python3 - "$TMP" "$PROMPT" <<'PYEOF'
import os, pty, select, signal, subprocess, sys, time

tmp, prompt = sys.argv[1], sys.argv[2]
work = os.path.join(tmp, "work")
master, slave = pty.openpty()
# The TUI refuses to start when TERM is `dumb` (it prints exactly that error),
# so an inherited or missing TERM has to be replaced with a usable one.
term = os.environ.get("TERM", "")
if not term or term == "dumb":
    term = "xterm-256color"
env = dict(os.environ, CODEX_HOME=tmp, TERM=term)
proc = subprocess.Popen(
    ["codex"], stdin=slave, stdout=slave, stderr=slave,
    env=env, cwd=work, start_new_session=True,
)
os.close(slave)
time.sleep(7)                                   # let the TUI come up
os.write(master, b"\r"); time.sleep(1)          # dismiss splash / trust prompt
os.write(master, prompt.encode() + b"\r")       # submit the prompt
time.sleep(14)                                  # let it reach the (recorder) upstream
os.write(master, b"\x03"); time.sleep(1)        # Ctrl-C
os.write(master, b"\x03"); time.sleep(2)

# Keep what the TUI displayed: an empty capture is much easier to explain when
# you can see whether it sat on a dialog or errored out.
screen = bytearray()
try:
    while True:
        ready, _, _ = select.select([master], [], [], 0.4)
        if not ready:
            break
        chunk = os.read(master, 65536)
        if not chunk:
            break
        screen += chunk
except OSError:
    pass
with open(os.path.join(tmp, "tui_screen.txt"), "wb") as handle:
    handle.write(bytes(screen))

try:
    os.killpg(proc.pid, signal.SIGKILL)
except Exception:
    pass
PYEOF
else
    echo "==> running codex exec against the recorder (no traffic leaves this machine)"
    # </dev/null keeps it non-interactive: with a pipe/TTY on stdin the CLI tries
    # to read the prompt from stdin as well and can appear to hang.
    CODEX_HOME="$TMP" timeout 90 \
        codex exec --skip-git-repo-check "$PROMPT" </dev/null >/dev/null 2>&1 || true
fi

if [[ ! -s "$CAPTURE" ]]; then
    if [[ -s "$TMP/tui_screen.txt" ]]; then
        echo "--- what the TUI displayed (last 600 bytes) ---" >&2
        tail -c 600 "$TMP/tui_screen.txt" | tr -d '\000' >&2
        echo >&2
        echo "--- end of TUI screen ---" >&2
    fi
    cat >&2 <<'EOM'
recorder saw no request. Most likely one of:
  * you replaced the command with the bare TUI (`codex`) — it owns the terminal and
    sends nothing until you type; use `TUI=1 scripts/capture-cli-headers.sh` instead;
  * the CLI exited before reaching the network (check `codex --version` and whether
    it still honours `openai_base_url` in config.toml);
  * it reached out over HTTPS and ignored the override — re-run with `bash -x` to see.
EOM
    exit 1
fi
echo
echo "=== headers the real Codex CLI sent (authorization redacted) ==="
cat "$CAPTURE"
echo
echo "=== compare with what proxy-planx would send ==="
echo "    cargo run -p proxy-planx --example identity_headers -- gpt"
