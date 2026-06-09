#!/usr/bin/env python3
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


CORE = os.environ.get("STREAMSERVER_196_CORE", "http://172.17.13.196:8080").rstrip("/")
AGENT = os.environ.get("STREAMSERVER_196_AGENT", "http://172.17.13.196:8081").rstrip("/")
ZLM = os.environ.get("STREAMSERVER_196_ZLM", "http://172.17.13.196:80").rstrip("/")
USERNAME = os.environ.get("STREAMSERVER_196_USERNAME", "admin")
PASSWORD = os.environ.get("STREAMSERVER_196_PASSWORD", "")
SAMPLE_MP4 = os.environ.get("STREAMSERVER_E2E_SAMPLE_MP4", "")


class ApiError(Exception):
    def __init__(self, status, body):
        super().__init__(f"HTTP {status}: {body}")
        self.status = status
        self.body = body


def request(method, url, token=None, body=None, headers=None):
    data = None
    merged_headers = dict(headers or {})
    if body is not None:
        data = json.dumps(body).encode()
        merged_headers["Content-Type"] = "application/json"
    if token:
        merged_headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(url, data=data, method=method, headers=merged_headers)
    try:
        with urllib.request.urlopen(req, timeout=20) as resp:
            raw = resp.read()
            if not raw:
                return None
            content_type = resp.headers.get("content-type", "")
            return json.loads(raw.decode()) if "application/json" in content_type else raw.decode()
    except urllib.error.HTTPError as error:
        raw = error.read().decode(errors="replace")
        try:
            parsed = json.loads(raw)
        except json.JSONDecodeError:
            parsed = raw
        raise ApiError(error.code, parsed) from error


def upload_file(url, path, token):
    boundary = f"----streamserver-desktop-{uuid.uuid4().hex}"
    name = os.path.basename(path)
    with open(path, "rb") as fh:
        payload = fh.read()
    body = (
        f"--{boundary}\r\n"
        f'Content-Disposition: form-data; name="file"; filename="{name}"\r\n'
        "Content-Type: video/mp4\r\n\r\n"
    ).encode() + payload + f"\r\n--{boundary}--\r\n".encode()
    headers = {
        "Authorization": f"Bearer {token}",
        "Content-Type": f"multipart/form-data; boundary={boundary}",
    }
    req = urllib.request.Request(url, data=body, method="POST", headers=headers)
    with urllib.request.urlopen(req, timeout=120) as resp:
        return json.loads(resp.read().decode())


def get_value(payload, *keys):
    for key in keys:
        if isinstance(payload, dict) and key in payload:
            return payload[key]
    return None


def wait_task(token, task_id, expected=None, terminal=False, timeout=90):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        detail = request("GET", f"{CORE}/api/v1/tasks/{task_id}", token=token)
        last = detail
        status = task_status(detail)
        if expected and status in expected:
            return detail
        if terminal and status in {"SUCCEEDED", "FAILED", "CANCELED", "LOST"}:
            return detail
        time.sleep(2)
    raise RuntimeError(f"task {task_id} did not reach expected state, last={last}")


def task_status(detail):
    if not isinstance(detail, dict):
        return None
    if "status" in detail:
        return detail.get("status")
    task = detail.get("task")
    if isinstance(task, dict):
        return task.get("status")
    return None


def make_sample_mp4(path):
    ffmpeg = os.environ.get("FFMPEG", "ffmpeg")
    subprocess.run(
        [
            ffmpeg,
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x180:rate=15",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=44100",
            "-t",
            "4",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            path,
        ],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def main():
    if not PASSWORD:
        print("STREAMSERVER_196_PASSWORD is required", file=sys.stderr)
        return 2

    prefix = f"desktop-e2e-{int(time.time())}"
    created_tasks = []
    upload_id = None
    token = None
    print(f"[e2e] core={CORE}")
    print(f"[e2e] prefix={prefix}")

    try:
        print("[e2e] health")
        print(request("GET", f"{CORE}/health/live"))
        print(request("GET", f"{CORE}/health/ready"))
        print(request("GET", f"{AGENT}/health/metadata"))

        print("[e2e] login")
        tokens = request(
            "POST",
            f"{CORE}/api/v1/auth/login",
            body={"username": USERNAME, "password": PASSWORD},
        )
        token = tokens["access_token"]
        session = request("GET", f"{CORE}/api/v1/me", token=token)
        print({"subject": session.get("subject"), "role": session.get("role")})

        print("[e2e] zlm reachable")
        try:
            print(request("GET", f"{ZLM}/index/api/getMediaList"))
        except Exception as error:
            print(f"[e2e] zlm probe warning: {error}")

        with tempfile.TemporaryDirectory() as tmp:
            sample = os.path.join(tmp, f"{prefix}.mp4")
            if SAMPLE_MP4:
                with open(SAMPLE_MP4, "rb") as source, open(sample, "wb") as target:
                    target.write(source.read())
            else:
                make_sample_mp4(sample)
            print("[e2e] upload")
            uploaded = upload_file(f"{CORE}/api/v1/uploads/media", sample, token)
            upload_id = get_value(uploaded, "id")
            source_url = get_value(uploaded, "sourceUrl", "source_url")
            http_url = get_value(uploaded, "httpUrl", "http_url")
            node_id = get_value(uploaded, "node_id", "nodeId") or node_id_from_source(source_url)
            print({"upload_id": upload_id, "source_url": source_url, "http_url": http_url})
            if not source_url:
                raise RuntimeError(f"upload response did not contain sourceUrl: {uploaded}")

        print("[e2e] file_transcode")
        transcode = request(
            "POST",
            f"{CORE}/api/v1/tasks",
            token=token,
            headers={"Idempotency-Key": str(uuid.uuid4())},
            body={
                "name": f"{prefix}-transcode",
                "type": "file_transcode",
                "common": {"created_by": "desktop-e2e"},
                "input": {"kind": "file", "source_mode": "vod", "url": source_url},
                "publish": {"kind": "file", "format": "mp4"},
                "schedule": {"start_mode": "immediate"},
            },
        )
        transcode_id = transcode["id"]
        created_tasks.append(transcode_id)
        transcode_detail = wait_task(token, transcode_id, terminal=True, timeout=120)
        print({"transcode_id": transcode_id, "status": task_status(transcode_detail)})
        artifacts = request(
            "GET",
            f"{CORE}/api/v1/file-artifacts?task_id={urllib.parse.quote(transcode_id)}&page_size=20",
            token=token,
        )
        print({"artifacts": len(artifacts.get("items", []))})

        print("[e2e] stream_ingest")
        stream_name = f"{prefix}-live"
        ingest = request(
            "POST",
            f"{CORE}/api/v1/tasks",
            token=token,
            headers={"Idempotency-Key": str(uuid.uuid4())},
            body={
                "name": f"{prefix}-ingest",
                "type": "stream_ingest",
                "common": {"created_by": "desktop-e2e"},
                "input": {
                    "kind": "file",
                    "source_mode": "vod",
                    "loop_enabled": True,
                    "url": source_url,
                },
                "stream": {"app": "desktop-e2e", "name": stream_name},
                "expose": {
                    "enable_rtsp": True,
                    "enable_rtmp": True,
                    "enable_http_ts": True,
                    "enable_http_fmp4": True,
                    "enable_hls": True,
                },
                "record": {"enabled": False},
                "schedule": {"start_mode": "immediate"},
                "recovery": {"policy": "never"},
            },
        )
        ingest_id = ingest["id"]
        created_tasks.append(ingest_id)
        wait_task(token, ingest_id, expected={"RUNNING"}, timeout=90)
        streams = request(
            "GET",
            f"{CORE}/api/v1/streams?task_id={urllib.parse.quote(ingest_id)}",
            token=token,
        )
        stream_items = streams if isinstance(streams, list) else streams.get("value", streams.get("items", []))
        play_urls = []
        for item in stream_items:
            play_urls.extend(item.get("play_urls") or [])
        print({"ingest_id": ingest_id, "streams": len(stream_items), "play_urls": play_urls[:8]})

        print("[e2e] runtime recording")
        request(
            "POST",
            f"{CORE}/api/v1/tasks/{ingest_id}/recording/start",
            token=token,
            body={"format": "mp4", "segment_sec": 5},
        )
        time.sleep(6)
        request(
            "POST",
            f"{CORE}/api/v1/tasks/{ingest_id}/recording/stop",
            token=token,
            body={"reason": "desktop-e2e"},
        )
        time.sleep(6)
        records = request(
            "GET",
            f"{CORE}/api/v1/records?task_id={urllib.parse.quote(ingest_id)}&page_size=20",
            token=token,
        )
        print({"records": len(records.get("items", []))})

        print("[e2e] debug")
        request("GET", f"{CORE}/api/v1/debug/zlm/media?node_id={urllib.parse.quote(node_id)}", token=token)
        request("GET", f"{CORE}/api/v1/debug/zlm/sessions?node_id={urllib.parse.quote(node_id)}", token=token)
        if stream_items:
            first_stream = stream_items[0]
            player_query = urllib.parse.urlencode(
                {
                    "node_id": node_id,
                    "schema": first_stream.get("schema", "rtsp"),
                    "vhost": first_stream.get("vhost", "__defaultVhost__"),
                    "app": first_stream.get("app", "desktop-e2e"),
                    "stream": first_stream.get("stream", stream_name),
                }
            )
            try:
                request("GET", f"{CORE}/api/v1/debug/zlm/players?{player_query}", token=token)
            except ApiError as error:
                print(f"[e2e] players warning: {error}")
        print("[e2e] ok")
        return 0
    finally:
        if token:
            print("[e2e] cleanup")
            for task_id in reversed(created_tasks):
                try:
                    request("POST", f"{CORE}/api/v1/tasks/{task_id}/stop", token=token)
                except Exception:
                    pass
                time.sleep(1)
                try:
                    request("DELETE", f"{CORE}/api/v1/tasks/{task_id}", token=token)
                except Exception as error:
                    print(f"[e2e] cleanup task warning {task_id}: {error}")
            if upload_id:
                try:
                    request(
                        "DELETE",
                        f"{CORE}/api/v1/uploads/media/{upload_id}?delete_file=true",
                        token=token,
                    )
                except Exception as error:
                    print(f"[e2e] cleanup upload warning {upload_id}: {error}")


def node_id_from_source(source_url):
    if not source_url:
        return ""
    parts = source_url.split("/")
    if len(parts) >= 2 and parts[0] == "uploads":
        return parts[1]
    return ""


if __name__ == "__main__":
    raise SystemExit(main())
