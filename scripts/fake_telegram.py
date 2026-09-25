#!/usr/bin/env python3
"""A fake Telegram Bot API server for scripts/e2e.sh (section 8).

Just enough of the Bot API for `athena telegram`: getMe, getWebhookInfo,
deleteWebhook, setMyCommands, getUpdates (long polling with offsets),
sendChatAction and sendMessage. Every Bot API call is recorded.

The e2e script drives it over plain HTTP:

    POST /control/message   user=ID&text=TEXT   queue a private text message
    GET  /control/count?method=M&chat=ID        how many calls to M for chat
    GET  /control/text?chat=ID&n=N              text of the Nth sendMessage (1-based)

Usage: fake_telegram.py PORT_FILE. It binds a free port on 127.0.0.1 and
writes the port number to PORT_FILE once it is listening. Standard library
only. tests/telegram/fake_api.rs is the Rust twin the hermetic tests use.
"""

import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

lock = threading.Condition()
updates = []  # unconfirmed updates
calls = []  # (method, body)
next_update = [1]
next_message = [0]

BOT = {"id": 4242, "is_bot": True, "first_name": "athena", "username": "athena_e2e_bot"}


def get_updates(body):
    offset = body.get("offset", 0) or 0
    timeout = body.get("timeout", 0) or 0
    with lock:
        # Asking from an offset confirms every earlier update.
        updates[:] = [u for u in updates if u["update_id"] >= offset]
        lock.wait_for(lambda: updates, timeout=timeout)
        return list(updates)


def api(method, body):
    if method == "getMe":
        return dict(BOT, can_join_groups=False, can_read_all_group_messages=False,
                    supports_inline_queries=False, has_main_web_app=False)
    if method == "getWebhookInfo":
        return {"url": "", "has_custom_certificate": False, "pending_update_count": 0}
    if method == "getUpdates":
        return get_updates(body)
    if method == "sendMessage":
        with lock:
            next_message[0] += 1
            message_id = next_message[0]
        return {"message_id": message_id, "date": 1790000000, "from": BOT,
                "chat": {"id": body["chat_id"], "type": "private", "first_name": "E2E"},
                "text": body["text"]}
    return True


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, value, content_type="application/json"):
        data = value.encode() if isinstance(value, str) else json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def body(self):
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length else b""

    def do_POST(self):
        path = urlparse(self.path).path
        raw = self.body()
        if path == "/control/message":
            form = parse_qs(raw.decode())
            user = int(form["user"][0])
            with lock:
                updates.append({"update_id": next_update[0], "message": {
                    "message_id": next_update[0], "date": 1790000000, "text": form["text"][0],
                    "chat": {"id": user, "type": "private", "first_name": "E2E"},
                    "from": {"id": user, "is_bot": False, "first_name": "E2E"}}})
                next_update[0] += 1
                lock.notify_all()
            return self.reply("ok\n", "text/plain")
        parts = path.strip("/").split("/")
        if len(parts) != 2 or not parts[0].startswith("bot"):
            self.send_error(404)
            return
        # Method names are case-insensitive; teloxide sends `SendMessage`.
        method = parts[1][:1].lower() + parts[1][1:]
        body = json.loads(raw) if raw else {}
        with lock:
            calls.append((method, body))
        self.reply({"ok": True, "result": api(method, body)})

    def do_GET(self):
        url = urlparse(self.path)
        query = {k: v[0] for k, v in parse_qs(url.query).items()}
        chat = int(query.get("chat", 0))
        with lock:
            if url.path == "/control/count":
                n = sum(1 for m, b in calls
                        if m == query["method"] and (not chat or b.get("chat_id") == chat))
                return self.reply(f"{n}\n", "text/plain")
            if url.path == "/control/text":
                texts = [b["text"] for m, b in calls if m == "sendMessage" and b.get("chat_id") == chat]
                return self.reply(texts[int(query["n"]) - 1] + "\n", "text/plain")
        self.send_error(404)


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    with open(sys.argv[1] + ".tmp", "w") as f:
        f.write(str(server.server_address[1]))
    os.replace(sys.argv[1] + ".tmp", sys.argv[1])
    server.serve_forever()


if __name__ == "__main__":
    main()
