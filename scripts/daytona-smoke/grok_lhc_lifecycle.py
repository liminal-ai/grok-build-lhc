#!/usr/bin/env python3
"""Run one deterministic headless Grok turn and verify durable LHC capture."""

import http.server
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import threading
from http.server import ThreadingHTTPServer

PROMPT = "GROK_LHC_RELEASE_PROMPT"
RESPONSE = "GROK_LHC_RELEASE_RESPONSE"


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, _format, *_args):
        return

    def do_GET(self):
        if self.path.startswith("/v1/models"):
            self._json({"object": "list", "data": [{"id": "test-model", "object": "model", "apiBackend": "responses"}]})
        elif self.path.startswith("/v1/settings"):
            self._json({"allow_access": True})
        elif self.path.startswith("/v1/user"):
            self._json({"subscriptionTier": "pro"})
        else:
            self.send_error(404)

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        self.rfile.read(length)
        if self.path.startswith("/v1/responses"):
            events = [
                {"type": "response.created", "sequence_number": 0, "response": {"id": "resp_release", "object": "response", "created_at": 1, "model": "test-model", "status": "in_progress", "output": []}},
                {"type": "response.output_text.delta", "sequence_number": 1, "item_id": "item_release", "output_index": 0, "content_index": 0, "delta": RESPONSE},
                {"type": "response.completed", "sequence_number": 2, "response": {"id": "resp_release", "object": "response", "created_at": 1, "model": "test-model", "status": "completed", "output": [{"type": "message", "id": "msg_release", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": RESPONSE, "annotations": []}]}], "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15, "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}}}},
            ]
            body = "".join(f"data: {json.dumps(event)}\n\n" for event in events) + "data: [DONE]\n\n"
            encoded = body.encode()
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("content-length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)
        else:
            self._json({})

    def do_PUT(self):
        length = int(self.headers.get("content-length", "0"))
        self.rfile.read(length)
        self._json({"codingDataRetentionOptOut": False})

    def _json(self, payload):
        encoded = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)


def verify_archive(lhc_root, manifest_path):
    expected_schema = json.loads(Path(manifest_path).read_text())["lhc_thread_schema"]
    databases = list((Path(lhc_root) / "threads").glob("*.sqlite"))
    if len(databases) != 1:
        raise SystemExit(f"expected one LHC thread database, found {databases}")
    with sqlite3.connect(databases[0]) as db:
        version = db.execute("pragma user_version").fetchone()[0]
        if version != expected_schema:
            raise SystemExit(f"expected schema v{expected_schema}, got {version}")
        if db.execute("pragma quick_check").fetchone()[0] != "ok":
            raise SystemExit("LHC database quick_check failed")
        payloads = [row[0] for row in db.execute("select payload from event order by event_order")]
        kinds = [row[0] for row in db.execute("select event_kind from event order by event_order")]
        if not any(PROMPT in payload for payload in payloads):
            raise SystemExit("submitted prompt was not durably captured")
        if not any(RESPONSE in payload for payload in payloads):
            raise SystemExit("assistant response was not durably captured")
        if "turn_end" not in kinds:
            raise SystemExit(f"completed turn_end missing: {kinds}")
        completed = db.execute("select count(*) from turns where status='closed' and outcome='completed' and closed_at_event_order is not null").fetchone()[0]
        if completed < 1:
            raise SystemExit("no closed completed turn persisted")
    print(f"LHC_PERSISTENCE_PASS {databases[0]}")


def main():
    if len(sys.argv) == 4 and sys.argv[1] == "--verify-only":
        verify_archive(sys.argv[2], sys.argv[3])
        return
    if len(sys.argv) != 5:
        raise SystemExit("usage: grok_lhc_lifecycle.py BINARY HOME LHC_ROOT MANIFEST")
    binary, home, lhc_root, manifest_path = sys.argv[1:]
    Path(home).mkdir(parents=True, exist_ok=True)
    Path(lhc_root).mkdir(parents=True, exist_ok=True)
    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        base = f"http://127.0.0.1:{server.server_address[1]}/v1"
        env = os.environ | {
            "HOME": home,
            "GROK_LHC_ROOT": lhc_root,
            "GROK_MODELS_BASE_URL": base,
            "GROK_XAI_API_BASE_URL": base,
            "GROK_CLI_CHAT_PROXY_BASE_URL": base,
            "XAI_API_KEY": "release-smoke-key",
            "GROK_DISABLE_AUTOUPDATER": "1",
            "GROK_PROMPT_SUGGESTIONS": "false",
            "GROK_TURN_SUMMARY": "0",
            "GROK_TELEMETRY_ENABLED": "false",
            "OTEL_SDK_DISABLED": "true",
            "NO_PROXY": "127.0.0.1,localhost",
        }
        result = subprocess.run(
            [binary, "--single", PROMPT, "--verbatim", "--always-approve", "--model", "test-model", "--max-turns", "1", "--output-format", "plain"],
            env=env,
            text=True,
            capture_output=True,
            timeout=60,
        )
        server.shutdown()
    if result.returncode != 0:
        raise SystemExit(f"headless turn failed ({result.returncode}): {result.stderr}")
    if RESPONSE not in result.stdout:
        raise SystemExit(f"assistant response missing from stdout: {result.stdout!r}")
    verify_archive(lhc_root, manifest_path)


if __name__ == "__main__":
    main()
