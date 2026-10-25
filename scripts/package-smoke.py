"""Exercise the packaged executable over SDK v2; public GitHub checks are opt-in."""
import hashlib
import json
import os
import re
import select
import struct
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

package = Path(sys.argv[1]).resolve()
continuous = "--continuous" in sys.argv[2:]
modeltrace = "--modeltrace" in sys.argv[2:]
updates_live = "--updates-live" in sys.argv[2:]
updates = "--updates" in sys.argv[2:] or updates_live


def assert_same(actual, expected, path="run"):
    if isinstance(actual, dict) and isinstance(expected, dict):
        assert actual.keys() == expected.keys(), path
        for key in actual:
            assert_same(actual[key], expected[key], f"{path}.{key}")
    elif isinstance(actual, list) and isinstance(expected, list):
        assert len(actual) == len(expected), path
        for i, (a, b) in enumerate(zip(actual, expected)):
            assert_same(a, b, f"{path}[{i}]")
    else:
        assert actual == expected, f"{path}: restored={actual!r}; original={expected!r}"

with tempfile.TemporaryDirectory(prefix="model-health-smoke-") as directory:
    root = Path(directory).resolve()
    with tarfile.open(package) as archive:
        for member in archive.getmembers():
            path = root / member.name
            assert member.isfile() and path.resolve().is_relative_to(root)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(archive.extractfile(member).read())
            path.chmod(0o755 if member.mode & 0o111 else 0o644)
    manifest = json.loads((root / "plugin.json").read_text())
    for path, digest in manifest["package"]["files"].items():
        assert hashlib.sha256((root / path).read_bytes()).hexdigest() == digest, path
    assert (root / "bin/plugin").read_bytes()[:4] == b"\x7fELF"
    html = (root / "web/index.html").read_text()
    assert '<script defer src="./app.js"></script>' in html and 'type="module"' not in html
    javascript = (root / "web/app.js").read_text()
    assert "preview-request-" not in javascript and "demo-a" not in javascript
    # 按宿主 PreparedPackage / ProcessSpec 合同准备：私有可写包目录、非 root、空环境。
    # 只读根文件系统与 /tmp:noexec 保持实际部署条件，不能为了测试放开 /tmp。
    root.chmod(0o755)
    container = f"model-health-smoke-{os.getpid()}"
    command = ["docker", "run", "--rm", "--name", container, "-i", "--platform", "linux/amd64",
               "--network", "none", "--read-only", "--user", "10001:10001",
               "--tmpfs", "/tmp:rw,noexec,nosuid,nodev,size=64m",
               "--tmpfs", "/package:rw,exec,nosuid,nodev,size=256m,uid=10001,gid=10001,mode=0700",
               "-v", f"{root}:/artifact:ro", "-w", "/package", "--entrypoint", "/bin/sh",
               os.environ.get("SMOKE_IMAGE", "debian:bookworm-slim"),
               "-ec", 'test "$(id -u)" = 10001; grep -q " /tmp .*noexec" /proc/mounts; '
               'cp -R /artifact/. /package/; chmod -R go-rwx /package; chmod 700 bin/plugin; '
               'exec /usr/bin/env -i ./bin/plugin']
    process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)

    # RPC v5：业务 JSON（配置、参数、结果）不进入控制元数据，而是放在载荷前缀，元数据只记录 data_bytes
    def data_slot(message):
        kind = message["type"]
        if kind == "hello":
            return message["handshake"], "configuration"
        if kind in ("call", "callback"):
            return message, "params"
        if kind == "result":
            return message, "result"
        return None, None

    def send(message, payload=b""):
        message = json.loads(json.dumps(message))
        holder, key = data_slot(message)
        data = b""
        if holder is not None:
            data = json.dumps(holder.pop(key), separators=(",", ":")).encode()
            message["data_bytes"] = len(data)
        body = json.dumps(message, separators=(",", ":")).encode()
        process.stdin.write(struct.pack(">IQ", len(body), len(data) + len(payload)) + body + data + payload)
        process.stdin.flush()

    def read_exact(length):
        result = b""
        while len(result) < length:
            if not select.select([process.stdout], [], [], 40)[0]:
                raise TimeoutError("packaged plugin did not respond")
            chunk = process.stdout.read(length - len(result))
            if not chunk:
                raise RuntimeError("packaged plugin exited: " + process.stderr.read().decode())
            result += chunk
        return result

    def receive():
        meta, payload = struct.unpack(">IQ", read_exact(12))
        assert 0 < meta <= 65536 and payload < 2 * 1024 * 1024
        message = json.loads(read_exact(meta))
        assert not message.pop("request_context_bytes", 0)
        data_bytes = message.pop("data_bytes", 0)
        body = read_exact(payload)
        holder, key = data_slot(message)
        assert (holder is not None) == (data_bytes > 0), message
        if holder is not None:
            holder[key] = json.loads(body[:data_bytes])
        return message, body[data_bytes:]

    host = {"executions": 0, "state": {}, "callbacks": [], "numeric": False, "http_body": None}

    def runtime_account():
        return {"account_id": "smoke-account", "provider_id": "openai", "credential_revision": 1 + host["executions"],
                "name": "Offline fixture", "email": "  smoke@example.com  ", "upstream_user_id": "fixture-user", "upstream_account_id": "fixture-workspace",
                "plan_type": "pro", "authentication_kind": "oauth", "enabled": True, "credential_state": "ready",
                "has_refresh_token": True, "access_token_expires_at_ms": None, "next_refresh_at_ms": None}

    def callback(method, params, payload):
        host["callbacks"].append(method)
        if method == "host.http.do_stream":
            assert updates and params["method"] == "GET" and not payload
            assert host["http_body"] is None
            url = params["url"]
            pattern = r"https://api\.github\.com/repos/(chen-006/meow-llm-detector|xqy2006/ModelTrace)/(commits\?per_page=1|compare/[0-9a-f]{40}\.\.\.[0-9a-f]{40}\?per_page=1&page=2)"
            assert re.fullmatch(pattern, url), url
            headers = {name: bytes(value).decode() for name, value in params["headers"]}
            assert set(headers) == {"User-Agent", "Accept", "X-GitHub-Api-Version"}
            assert headers["User-Agent"] == f'codex-model-health/{manifest["version"]}'
            status = 200
            if updates_live:
                class NoRedirect(urllib.request.HTTPRedirectHandler):
                    def redirect_request(self, *args):
                        return None
                opener = urllib.request.build_opener(NoRedirect)
                try:
                    with opener.open(urllib.request.Request(url, headers=headers), timeout=10) as response:
                        status = response.status
                        body = response.read(262145)
                except urllib.error.HTTPError as error:
                    status, body = error.code, b""
                    error.close()
            else:
                if "/compare/" in url:
                    data = {"status": "ahead", "ahead_by": 2, "behind_by": 0}
                else:
                    source = next(s for s in boot["sources"] if s["url"].removeprefix("https://github.com/") in url)
                    data = [{"sha": source["bundled_commit"] if source["method"] == "meow" else "a" * 40}]
                body = json.dumps(data).encode()
            host["http_body"] = body
            return {"status": status, "http_version": "HTTP/1.1", "headers": [], "stream": "github", "trailers": []}, b""
        if method == "host.http.stream_read":
            assert params["stream"] == "github" and not payload
            body = host["http_body"]
            if not body:
                host["http_body"] = None
                return {"eof": True}, b""
            length = min(len(body), params["maximum_bytes"])
            host["http_body"] = body[length:]
            return {"eof": False}, body[:length]
        if method == "host.http.stream_close":
            assert params["stream"] == "github" and not payload
            host["http_body"] = None
            return {}, b""
        if method == "host.auth.list":
            assert json.loads(payload)["provider_id"] == "openai"
            return {}, json.dumps({"accounts": [runtime_account()], "next_cursor": None}).encode()
        if method == "host.auth.get_runtime":
            assert json.loads(payload)["account_id"] == "smoke-account"
            return {}, json.dumps(runtime_account()).encode()
        if method == "host.keys.list":
            return {"keys": [{"id": "smoke-key", "name": "Offline key", "enabled": True}], "next_cursor": None}, b""
        if method == "host.models.list":
            assert params["client_key_id"] == "smoke-key" and params["protocol"] == "openai"
            return {"models": ["gpt-6-astra"]}, b""
        if method == "host.state.get":
            assert params["namespace"] == "runs"
            return {"record": host["state"].get(params["key"])}, b""
        if method == "host.state.put":
            assert params["namespace"] == "runs"
            previous = host["state"].get(params["key"])
            assert params["expected_version"] == (previous["version"] if previous else None)
            version = previous["version"] + 1 if previous else 1
            host["state"][params["key"]] = {"value": params["value"], "version": version, "schema_version": 1}
            return {"version": version}, b""
        if method == "host.model.execute_stream":
            assert continuous or modeltrace, "bootstrap must not execute a model"
            assert params["account_id"] == "smoke-account" and params["client_key_id"] == "smoke-key"
            assert params["provider"] == "openai" and params["protocol"] == "openai" and params["model"] == "gpt-6-astra"
            assert any(r["value"]["inflight"] and r["value"]["attempts"][-1]["status"] == "pending" for r in host["state"].values())
            body = json.loads(payload)
            assert body["stream"] is True and body["store"] is False
            if host["numeric"]:
                assert "reasoning" not in body and "max_output_tokens" not in body and "tools" not in body
                pending = next(r["value"] for r in host["state"].values() if r["value"]["inflight"])
                job = next(j for j in pending["plan"]["jobs"] if j["job_id"] == pending["inflight"])
                assert body["input"] == [{"role": "user", "content": job["prompt"]}]
            host["executions"] += 1
            return {"request_id": f'fixture-request-{host["executions"]}', "stream": "fixture-stream"}, b""
        if method == "host.model.stream_read":
            answer = ','.join(str(1 + (i * 37 + 13) % 355) for i in range(332)) if host["numeric"] else "3"
            data = {"type": "response.completed", "response": {"id": "fixture-response", "status": "completed",
                    "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": answer}]}],
                    "usage": {"input_tokens": 10, "output_tokens": 1}}}
            metadata = json.dumps({"wire": {"protocol": "openai", "payload": {"kind": "json", "event": None, "data": data}}}).encode()
            event = b"GPE2" + struct.pack(">III", len(metadata), 0, 0) + metadata
            return {"events": 1, "end": True}, b"HME1" + struct.pack(">II", 1, len(event)) + event
        raise AssertionError("unexpected callback: " + method)

    def api(path, body=None):
        context["call_id"] += 2
        identifier = context["call_id"]
        send({"type": "call", "id": identifier, "method": "management.handle", "context": context,
              "params": {"method": "GET" if body is None else "POST", "path": path, "query": "",
                         "content_type": None if body is None else "application/json", "headers": []}},
             b"" if body is None else json.dumps(body).encode())
        while True:
            message, payload = receive()
            if message["type"] == "result":
                assert message["id"] == identifier
                return message["result"]["status"], json.loads(payload)
            assert message["type"] == "callback", message
            assert message["parent_id"] == identifier
            result, response = callback(message["method"], message["params"], payload)
            send({"type": "result", "id": message["id"], "result": result}, response)

    try:
        send({"type": "hello", "handshake": {"protocol_version": 5, "artifact_sha256": hashlib.sha256(package.read_bytes()).hexdigest(), "plugin_id": "local.model-health", "instance_id": "smoke", "generation": 1, "incarnation": "smoke", "configuration": {}, "contributes": manifest["contributes"]}})
        ready, _ = receive()
        assert ready["type"] == "ready"
        context = {"call_id": 1, "instance_id": "smoke", "generation": 1, "incarnation": "smoke", "stage": "management", "timeout_ms": 120000, "resource_stream": False, "resource_scope_id": "smoke", "request_id": None, "attempt_id": None, "account_id": None, "credential_revision": None}
        status, boot = api("api/bootstrap")
        assert status == 200, boot
        catalogs = boot["catalog"].get("methods", [{"method": "meow", **boot["catalog"]}])
        meow = next(c for c in catalogs if c["method"] == "meow")
        assert meow["engine_version"] == "4.5.4"
        assert sorted(t["count"] for t in meow["tiers"]) == [36, 72, 108]
        assert len(meow["models"]) == 4
        assert boot["accounts"][0]["id"] == "smoke-account" and boot["keys"][0]["id"] == "smoke-key"
        assert boot["accounts"][0]["email"] == "  smoke@example.com  "
        assert host["callbacks"] == ["host.auth.list", "host.keys.list"]
        status, error = api("api/models", {})
        assert status == 400 and error["error"]["message"] == "请求参数格式错误", error
        if continuous:
            status, result = api("api/runs/start", {"account_id": "smoke-account", "key_id": "smoke-key",
                                "request_model": "gpt-6-astra", "claimed_model": "gpt-6-astra", "tier": "low"})
            assert status == 200, result
            assert result["run"]["account_id"] == "smoke-account"
            assert result["run"]["account_name"] == "smoke@example.com"
            run_id = result["run"]["id"]
            for completed in range(36):
                status, result = api("api/runs/step", {"id": run_id})
                if status != 200:
                    print(f"CONTINUOUS FAIL: completed={completed}/36; revision_changed=YES; real_model_calls=0", flush=True)
                    print(result["error"]["message"], file=sys.stderr)
                    sys.exit(1)
                assert result["run"]["attempts"][-1]["status"] == "ok", result
                if completed == 1:
                    for action in ("pause", "resume"):
                        status, response = api("api/runs/control", {"id": run_id, "action": action})
                        assert status == 200, response
            assert result["run"]["status"] == "completed" and result["run"]["report"]["valid_samples"] == 36
            attempts = result["run"]["attempts"]
            assert len({a["job_id"] for a in attempts}) == 36
            assert [a["request_id"] for a in attempts] == [f"fixture-request-{i}" for i in range(1, 37)]
            assert host["executions"] == 36 and runtime_account()["credential_revision"] == 37
            status, restored = api("api/runs/get", {"id": run_id})
            assert status == 200
            assert_same(restored["run"], result["run"])
            print("CONTINUOUS PASS: completed=36/36; revision=1->37; pause_resume=PASS; duplicate_requests=0; real_model_calls=0", flush=True)
        if modeltrace:
            if not any(c["method"] == "modeltrace" for c in catalogs):
                print("MODELTRACE ABSENT: methods=meow; real_model_calls=0", flush=True)
                sys.exit(1)
            trace = next(c for c in catalogs if c["method"] == "modeltrace")
            assert len(trace["models"]) == 17 and trace["family_count"] == 2
            status, result = api("api/runs/start", {"method": "modeltrace", "account_id": "smoke-account", "key_id": "smoke-key",
                                "request_model": "gpt-6-astra", "claimed_model": "unified", "tier": "fixed"})
            assert status == 200, result
            assert result["run"]["account_id"] == "smoke-account"
            assert result["run"]["account_name"] == "smoke@example.com"
            run_id = result["run"]["id"]
            plan = result["run"]["plan"]
            host["numeric"] = True
            before = host["executions"]
            for completed in range(3):
                status, result = api("api/runs/step", {"id": run_id})
                assert status == 200, result
                assert result["run"]["plan"] == plan
                assert result["run"]["report"]["valid_samples"] == completed + 1
                if completed == 0:
                    for action in ("pause", "resume"):
                        status, response = api("api/runs/control", {"id": run_id, "action": action})
                        assert status == 200, response
            assert result["run"]["status"] == "completed" and result["run"]["report"]["verdict"] == "attributed"
            assert len(result["run"]["report"]["analysis"]["results"]) == 17
            assert len({a["job_id"] for a in result["run"]["attempts"]}) == 3
            assert host["executions"] == before + 3
            status, restored = api("api/runs/get", {"id": run_id})
            assert status == 200
            assert_same(restored["run"], result["run"])
            status, history = api("api/runs")
            assert status == 200 and history["runs"][0]["method"] == "modeltrace"
            assert history["runs"][0]["report"] == result["run"]["report"]
            print("MODELTRACE PASS: completed=3/3; candidates=17; pinned_account_key=PASS; frozen_plan=PASS; pause_resume=PASS; history=PASS; real_model_calls=0", flush=True)
        if updates:
            if "sources" not in boot:
                print("UPDATES ABSENT: sources=0; model_methods=2; real_model_calls=0", flush=True)
                sys.exit(1)
            assert [s["method"] for s in boot["sources"]] == ["meow", "modeltrace"]
            before_calls = len(host["callbacks"])
            before_state = json.dumps(host["state"], sort_keys=True)
            before_executions = host["executions"]
            for source in boot["sources"]:
                assert len(source["bundled_commit"]) == 40
                status, result = api("api/upstreams/check", {"method": source["method"]})
                assert status == 200, result
                check = result["check"]
                assert check["method"] == source["method"]
                assert check["status"] in ["identical", "ahead", "behind", "diverged"]
                if not updates_live:
                    assert check["status"] == ("identical" if source["method"] == "meow" else "ahead")
                count = len(host["callbacks"])
                status, cached = api("api/upstreams/check", {"method": source["method"]})
                assert status == 200 and cached == result and len(host["callbacks"]) == count
                assert host["http_body"] is None
                if updates_live:
                    print(f'LIVE_GITHUB {source["label"]}: {check["status"]}; bundled={source["bundled_commit"]}; latest={check["latest_commit"]}', flush=True)
            assert json.dumps(host["state"], sort_keys=True) == before_state
            assert host["executions"] == before_executions
            assert all(m.startswith("host.http.") for m in host["callbacks"][before_calls:])
            print("UPDATES PASS: sources=2; per_source_cache=PASS; account_key_calls=0; state_changes=0; real_model_calls=0", flush=True)
        send({"type": "shutdown"})
        process.wait(timeout=15)
        assert process.returncode == 0, process.stderr.read().decode()
        print("PACKAGE PASS: hashes=PASS; tmp_noexec=PASS; nonroot=PASS; empty_env=PASS; readonly_root=PASS; sdk_rpc_v5=PASS; engine=4.5.4; accounts=1; keys=1; error_http=400; real_model_calls=0")
    finally:
        if process.poll() is None:
            subprocess.run(["docker", "rm", "-f", container], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
            process.wait(timeout=15)
