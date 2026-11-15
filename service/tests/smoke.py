#!/usr/bin/env python3
"""Exercise the actual HTTP process, either locally or inside its Docker image."""

import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid


TOKEN = "smoke-test-secret-" + "a" * 48


def command(*args):
    return subprocess.check_output(args, text=True).strip()


class Server:
    def __init__(self, binary, image):
        self.binary = binary
        self.image = image
        self.temp = tempfile.TemporaryDirectory(prefix="shrunk-service-")
        self.process = None
        self.name = "shrunk-smoke-" + uuid.uuid4().hex[:12]
        self.volume = self.name + "-data"
        self.port = 0
        self.environment = {}
        if image:
            command("docker", "volume", "create", self.volume)

    def start(self):
        if self.image:
            settings = [item for key, value in self.environment.items() for item in ("-e", key + "=" + value)]
            command(
                "docker", "run", "--detach", "--name", self.name,
                "--read-only", "--cap-drop=ALL", "--security-opt=no-new-privileges",
                "--memory=512m", "--cpus=2", "--pids-limit=64",
                "-p", "127.0.0.1::8080", "-v", self.volume + ":/data",
                "-e", "SHRUNK_TOKEN=" + TOKEN, *settings, self.image,
            )
            self.port = int(command("docker", "port", self.name, "8080/tcp").rsplit(":", 1)[1])
        else:
            # Ask the kernel for a free port; only the local test uses this small
            # close/rebind window. Docker allocates its own host port above.
            with socket.socket() as sock:
                sock.bind(("127.0.0.1", 0))
                self.port = sock.getsockname()[1]
            env = dict(os.environ, SHRUNK_TOKEN=TOKEN,
                       SHRUNK_LISTEN=f"127.0.0.1:{self.port}", SHRUNK_DATA_DIR=self.temp.name)
            env.pop("SHRUNK_TOKEN_FILE", None)
            env.pop("SHRUNK_RETENTION_MS", None)
            env.pop("SHRUNK_RETENTION_INTERVAL_MS", None)
            env.update(self.environment)
            self.log = open(Path(self.temp.name) / "server.log", "ab")
            self.process = subprocess.Popen([self.binary], env=env, stdout=self.log, stderr=self.log)
        for _ in range(200):
            try:
                self.request("/healthz", auth=False)
                return
            except (OSError, AssertionError):
                time.sleep(0.05)
        raise AssertionError("server did not become healthy")

    def stop(self, crash=False):
        if self.image:
            if crash:
                command("docker", "kill", self.name)
            else:
                command("docker", "stop", "--timeout=60", self.name)
                assert command("docker", "inspect", "--format={{.State.ExitCode}}", self.name) == "0"
            command("docker", "rm", self.name)
        elif self.process:
            self.process.kill() if crash else self.process.terminate()
            self.process.wait(timeout=60)
            if not crash:
                assert self.process.returncode == 0, "graceful shutdown failed"
            self.process = None
            self.log.close()

    def close(self):
        if self.image:
            subprocess.run(["docker", "rm", "-f", self.name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            command("docker", "volume", "rm", self.volume)
        elif self.process:
            self.stop()
        self.temp.cleanup()

    def request(self, path, body=None, status=200, auth=True, raw=None, content_type="application/json", method=None):
        data = raw if raw is not None else (json.dumps(body).encode() if body is not None else None)
        request = urllib.request.Request(f"http://127.0.0.1:{self.port}{path}", data=data, method=method)
        if auth:
            request.add_header("Authorization", "Bearer " + TOKEN)
        if data is not None:
            request.add_header("Content-Type", content_type)
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                actual = response.status
                payload = response.read()
        except urllib.error.HTTPError as error:
            actual = error.code
            payload = error.read()
        assert actual == status, (path, actual, status, payload)
        return json.loads(payload)


def exercise(server):
    server.start()
    server.request("/v1/series", auth=False, status=401)
    server.request("/v1/checkpoint", {}, auth=False, status=401)
    server.request("/v1/query", raw=b"{", status=400)
    server.request("/v1/query", {}, content_type="text/plain", status=415)
    server.request("/v1/write", raw=b" " * (1024 * 1024 + 1), status=413)
    server.request("/v1/checkpoint", raw=b" " * (1024 * 1024 + 1), status=413)
    server.request("/v1/checkpoint", raw=b"{", status=400)
    server.request("/v1/checkpoint", {"unexpected": True}, status=400)
    server.request("/v1/checkpoint", [], status=400)
    server.request("/v1/checkpoint", {}, content_type="text/plain", status=415)
    assert server.request("/v1/checkpoint", method="POST") == {"checkpointed": True}
    server.request("/v1/query", {"series": "absent"}, status=404)
    points = [{"ts": i, "value": i * 0.5} for i in range(1, 7)]
    assert server.request("/v1/write", {"series": "cpu", "points": points}) == {"written": 6}
    result = server.request("/v1/query", {"series": "cpu", "limit": 2})
    assert result["points"] == points[:2] and result["truncated"]
    assert server.request("/v1/query", {"series": "cpu", "from": 3})["points"] == points[2:]
    assert server.request("/v1/query", {"series": "cpu", "filter": {"op": "gt", "value": 2}})["points"] == points[4:]
    assert server.request("/v1/aggregate", {"series": "cpu", "agg": "sum"})["value"] == 10.5
    # The second point conflicts; the first must not leak from a rejected batch.
    server.request("/v1/write", {"series": "cpu", "points": [{"ts": 7, "value": 7}, {"ts": 7, "value": 8}]}, status=409)
    server.request("/v1/write", {"series": "cpu", "points": [{"ts": 6, "value": 6}]}, status=409)
    server.request("/v1/query", {"series": "cpu", "from": 3, "to": 2}, status=400)
    server.request("/v1/query", {"series": "cpu", "limit": 100001}, status=400)
    server.request("/v1/write", {"series": "", "points": points}, status=400)
    server.request("/v1/query", {"series": "cpu", "typo": True}, status=400)
    assert server.request("/v1/query", {"series": "cpu"})["points"] == points
    # Crash before any checkpoint: all acknowledged writes must replay from WAL.
    server.stop(crash=True)
    server.start()
    assert server.request("/v1/query", {"series": "cpu"})["points"] == points
    assert server.request("/v1/checkpoint", {}) == {"checkpointed": True}
    tail = {"ts": 7, "value": 3.5}
    server.request("/v1/write", {"series": "cpu", "points": [tail]})
    server.stop(crash=True)
    server.start()
    assert server.request("/v1/query", {"series": "cpu"})["points"] == points + [tail]
    # Independent clients share one writer and cannot lose each other's writes.
    def write_series(i):
        return server.request("/v1/write", {"series": f"worker-{i}", "points": points})
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        assert all(result == {"written": 6} for result in pool.map(write_series, range(8)))
    assert server.request("/v1/series")["points"] == 55
    if server.image:
        assert command("docker", "inspect", "--format={{.Config.User}}", server.name) == "10001:10001"
        command("docker", "exec", server.name, "/usr/local/bin/shrunk-server", "healthcheck")
    server.stop()
    server.start()
    assert server.request("/v1/series")["points"] == 55
    server.stop()


def exercise_lifecycle(server):
    server.start()
    routes = ["delete", "delete-series", "retain", "downsample", "rollup"]
    for route in routes:
        server.request("/v1/" + route, {}, auth=False, status=401)
        server.request("/v1/" + route, {"unexpected": True}, status=400)
        server.request("/v1/" + route, {}, content_type="text/plain", status=415)
    # More than two storage blocks; bucket boundaries cut through the blocks.
    points = [{"ts": ts, "value": float(ts)} for ts in range(2500)]
    server.request("/v1/write", {"series": "raw", "points": points})
    server.request("/v1/delete", {"series": "raw"}, status=400)
    assert server.request("/v1/aggregate", {"series": "raw", "agg": "count"})["value"] == 2500
    expected = []
    for start in range(0, 2500, 700):
        values = [p["value"] for p in points if max(137, start) <= p["ts"] <= min(2400, start + 699)]
        expected.append({"ts": start, "value": sum(values) / len(values)})
    request = {"series": "raw", "from": 137, "to": 2400, "interval_ms": 700, "agg": "avg"}
    result = server.request("/v1/downsample", request)
    assert result["points"] == expected and not result["truncated"]
    assert result["stats"]["blocks_decoded"] > 0
    first = server.request("/v1/downsample", dict(request, limit=2))
    assert first["points"] == expected[:2] and first["truncated"] and first["next_from"] == 1400
    second = server.request("/v1/downsample", dict(request, limit=2, **{"from": first["next_from"]}))
    assert second["points"] == expected[2:] and not second["truncated"] and second["next_from"] is None
    for extra in ({"interval_ms": 0}, {"limit": 0}, {"limit": 100001}, {"agg": "bad"}, {"filter": {"op": "gt", "value": 1}}):
        server.request("/v1/downsample", dict(request, **extra), status=400)
    server.request("/v1/downsample", dict(request, series="absent"), status=404)
    rollup = dict(request, target="raw.avg")
    server.request("/v1/rollup", dict(rollup, max_buckets=3), status=413)
    server.request("/v1/query", {"series": "raw.avg"}, status=404)
    assert server.request("/v1/rollup", dict(rollup, max_buckets=4)) == {"series": "raw.avg", "written": 4}
    assert server.request("/v1/query", {"series": "raw.avg"})["points"] == expected
    server.request("/v1/rollup", rollup, status=409)
    server.request("/v1/rollup", dict(rollup, target="raw"), status=409)
    summary = {item["name"]: item for item in server.request("/v1/series")["series"]}
    assert summary["raw"]["blocks"] == 3 and summary["raw.avg"]["blocks"] == 1
    assert summary["raw"]["stored_bytes"] < summary["raw"]["raw_bytes"]
    deleted = server.request("/v1/delete", {"series": "raw", "from": 400, "to": 2200})
    assert deleted == {"points_deleted": 1801, "blocks_dropped": 1, "blocks_rewritten": 2}
    assert server.request("/v1/query", {"series": "raw"})["points"] == points[:400] + points[2201:]
    assert server.request("/v1/delete", {"series": "raw", "from": 2201})["points_deleted"] == 299
    tail = {"ts": 400, "value": -999.0}
    server.request("/v1/write", {"series": "raw", "points": [tail]})
    server.stop(crash=True)
    server.start()
    assert server.request("/v1/query", {"series": "raw"})["points"] == points[:400] + [tail]
    assert server.request("/v1/query", {"series": "raw.avg"})["points"] == expected
    assert server.request("/v1/delete-series", {"series": "raw"}) == {"series_deleted": True, "points_deleted": 401}
    server.request("/v1/query", {"series": "raw"}, status=404)
    server.request("/v1/delete-series", {"series": "raw"}, status=404)
    server.request("/v1/delete", {"series": "raw.avg", "from": 2, "to": 1}, status=400)
    server.request("/v1/write", {"series": "raw", "points": [{"ts": -1, "value": 5}]})
    retained = [{"ts": ts, "value": float(ts)} for ts in [-10, -1, 0, 5, 6, 10]]
    server.request("/v1/write", {"series": "retained", "points": retained})
    response = server.request("/v1/retain", {"now": 10, "window_ms": 5})
    assert response["cutoff"] == 5 and response["points_deleted"] == 5
    assert server.request("/v1/query", {"series": "retained"})["points"] == retained[3:]
    assert server.request("/v1/retain", {"cutoff": 5})["points_deleted"] == 0
    for invalid in ({}, {"now": 10}, {"window_ms": 5}, {"now": 10, "window_ms": 0}, {"cutoff": 0, "now": 10, "window_ms": 5}):
        server.request("/v1/retain", invalid, status=400)
    # Negative epochs, sparse empty buckets, and i64 boundaries are valid JSON.
    extremes = [{"ts": ts, "value": 1} for ts in [-(2**63), -11, -1, 0, 2**63 - 1]]
    server.request("/v1/write", {"series": "extreme", "points": extremes})
    extreme_request = {"series": "extreme", "interval_ms": 2**63 - 1, "agg": "count", "limit": 1}
    page = server.request("/v1/downsample", extreme_request)
    assert page["points"] == [{"ts": -(2**63), "value": 1}] and page["next_from"] == -(2**63) + 1
    server.stop()
    server.start()
    assert server.request("/v1/query", {"series": "retained"})["points"] == retained[3:]
    assert server.request("/v1/query", {"series": "raw"})["points"] == []
    server.stop()


def exercise_retention_policy(server):
    # Policy is disabled by default. Expired values first recover from the WAL.
    server.start()
    now = time.time_ns() // 1_000_000
    future = {"ts": now + 60000, "value": 2.0}
    server.request("/v1/write", {"series": "policy", "points": [{"ts": now - 20000, "value": 1.0}, future]})
    server.stop(crash=True)
    server.environment = {"SHRUNK_RETENTION_MS": "1200", "SHRUNK_RETENTION_INTERVAL_MS": "100"}
    server.start()
    assert server.request("/v1/query", {"series": "policy"})["points"] == [future]
    fresh = {"ts": time.time_ns() // 1_000_000, "value": 3.0}
    server.request("/v1/write", {"series": "aging", "points": [fresh]})
    assert server.request("/v1/query", {"series": "aging"})["points"] == [fresh]
    deadline = time.monotonic() + 10
    while server.request("/v1/query", {"series": "aging"})["points"]:
        assert time.monotonic() < deadline, "periodic retention did not expire the sample"
        time.sleep(0.1)
    assert server.request("/v1/query", {"series": "policy"})["points"] == [future]
    if not server.image:
        snapshot = Path(server.temp.name) / "shrunk.db"
        before = snapshot.stat()
        time.sleep(0.35)
        after = snapshot.stat()
        assert (before.st_ino, before.st_mtime_ns) == (after.st_ino, after.st_mtime_ns), "no-op policy rewrote snapshot"
    server.stop(crash=True)
    server.start()
    assert server.request("/v1/query", {"series": "aging"})["points"] == []
    assert server.request("/v1/query", {"series": "policy"})["points"] == [future]
    server.stop()


def main():
    parser = argparse.ArgumentParser()
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--binary", type=lambda p: str(Path(p).resolve()))
    source.add_argument("--image")
    parser.add_argument("--scenario", choices=["all", "base", "lifecycle", "retention"], default="all")
    args = parser.parse_args()
    scenarios = {"base": exercise, "lifecycle": exercise_lifecycle, "retention": exercise_retention_policy}
    for name, scenario in scenarios.items():
        if args.scenario not in ("all", name):
            continue
        server = Server(args.binary, args.image)
        try:
            scenario(server)
            print("PASS: " + name + " HTTP process, durability, and validation smoke")
        except BaseException:
            if args.image:
                subprocess.run(["docker", "logs", server.name], check=False)
            else:
                log = Path(server.temp.name) / "server.log"
                if log.exists():
                    print(log.read_text())
            raise
        finally:
            server.close()


if __name__ == "__main__":
    main()
