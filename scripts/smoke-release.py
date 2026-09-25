#!/usr/bin/env python3
"""Extract and verify an archive, then run offline startup/authentication checks."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tarfile
import tempfile
import time
import uuid
import urllib.error
import urllib.request
import zipfile

p = argparse.ArgumentParser()
p.add_argument("archive", type=Path)
a = p.parse_args()
with tempfile.TemporaryDirectory() as tmp:
    dest = Path(tmp)
    if a.archive.suffix == ".zip":
        with zipfile.ZipFile(a.archive) as z:
            z.extractall(dest)
    else:
        with tarfile.open(a.archive) as t:
            t.extractall(dest, filter="data")
    folders = list(dest.iterdir())
    assert len(folders) == 1 and folders[0].is_dir()
    root = folders[0]
    m = json.loads((root / "release-manifest.json").read_text())
    actual = {f.relative_to(root).as_posix() for f in root.rglob("*") if f.is_file()}
    assert actual == set(m["files"]) | {"release-manifest.json"}
    for f, digest in m["files"].items():
        assert hashlib.sha256((root / f).read_bytes()).hexdigest() == digest, f
    assert not any(f.startswith(("downloads/", "exports/", "tests/", ".git/")) for f in actual)
    exe = root / (m["name"] + (".exe" if os.name == "nt" else ""))
    env = os.environ.copy()
    for k in list(env):
        if k.startswith("SIRIUS_"):
            del env[k]
    env.update(SIRIUS_API_TOKEN="smoke-public-token", SIRIUS_INTERNAL_TOKEN="smoke-internal-token",
               SIRIUS_CDN_USERNAME="smoke-user", SIRIUS_CDN_CREDENTIAL="smoke-password")
    version = subprocess.run([str(exe), "--version"], cwd=root, env=env, check=True, capture_output=True, timeout=15)
    assert version.stdout.decode().strip() == m["name"] + " " + m["version"]
    if m["name"] == "sirius-asset-updater":
        (root / "sirius-asset-config.yaml").write_text((root / "sirius-asset-config.example.yaml").read_text())
        subprocess.run([str(exe), "--help"], cwd=root, env=env, check=True, capture_output=True, timeout=15)
        check = subprocess.run([str(exe), "check"], cwd=root, env=env, check=True, capture_output=True, timeout=15)
        assert json.loads(check.stdout)["ready"]
    else:
        # Offline recovery must preserve known work rather than submit another export.
        state = root / "smoke-dispatch-state"
        state.mkdir()
        identity = dict(destination_sha256="a" * 64,
                        request=dict(region="jp", profile="full", operation="update"),
                        profile_revision="1", environment="release", platform="iOS",
                        resource_version="r1", platform_hash="h1", require_full_catalog=True,
                        require_full_export=True, require_publication=False)
        key = "sirius-" + hashlib.sha256(json.dumps([1, identity], separators=(",", ":")).encode()).hexdigest()
        (state / "outbox.json").write_text(json.dumps(dict(schema_version=1, entries={key: dict(
            identity=identity, state=dict(state="failed", job_id=None, code="submission_ambiguous"))})))
        def dispatch(*args):
            result = subprocess.run([str(exe), *args], cwd=root, env=env, check=True,
                                    capture_output=True, timeout=15)
            return json.loads(result.stdout)
        assert dispatch("asset-dispatch-status", str(state))[key]["state"]["code"] == "submission_ambiguous"
        job_id = str(uuid.uuid4())
        assert dispatch("asset-dispatch-adopt", str(state), key, job_id)["state"] == dict(state="submitted", job_id=job_id)
        assert dispatch("asset-dispatch-status", str(state))[key]["state"]["job_id"] == job_id
        refused = subprocess.run([str(exe), "asset-dispatch-adopt", str(state), key, str(uuid.uuid4())],
                                 cwd=root, env=env, capture_output=True, timeout=15)
        assert refused.returncode != 0
        # Completed history can be compacted without discarding its permanent identity.
        ledger = json.loads((state / "outbox.json").read_text())
        ledger["entries"][key]["state"] = dict(state="completed", job_id=job_id,
                                               catalog_sha256="b" * 64, publication_id=None)
        (state / "outbox.json").write_text(json.dumps(ledger))
        assert dispatch("asset-dispatch-archive", str(state), key, job_id)["state"]["state"] == "completed"
        assert dispatch("asset-dispatch-status", str(state)) == {}
        assert dispatch("asset-dispatch-entry", str(state), key)["state"]["job_id"] == job_id
        assert dispatch("asset-dispatch-archive", str(state), key, job_id)["state"]["state"] == "completed"
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        config = (root / "sirius-api-config.example.yaml").read_text().replace("127.0.0.1:9999", f"127.0.0.1:{port}")
        (root / "sirius-api-config.yaml").write_text(config)
        proc = subprocess.Popen([str(exe)], cwd=root, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        def request(path, token=None):
            req = urllib.request.Request(f"http://127.0.0.1:{port}" + path)
            if token:
                req.add_header("Authorization", "Bearer " + token)
            try:
                with urllib.request.urlopen(req, timeout=2) as response:
                    return response.status, json.load(response)
            except urllib.error.HTTPError as error:
                return error.code, None
        try:
            for _ in range(100):
                if proc.poll() is not None:
                    raise RuntimeError(proc.stderr.read().decode())
                try:
                    code, health = request("/health")
                    break
                except (OSError, urllib.error.URLError):
                    time.sleep(0.1)
            else:
                raise RuntimeError("Packaged server did not start")
            assert code == 200 and health["version"] == m["version"]
            assert request("/internal/v1/protocol")[0] == 401
            assert request("/internal/v1/protocol", env["SIRIUS_API_TOKEN"])[0] == 401
            code, protocol = request("/internal/v1/protocol", env["SIRIUS_INTERNAL_TOKEN"])
            assert code == 200 and protocol["codec"] == "native"
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
    if m["name"] == "sirius-api-proxy":
        global_config = json.loads((root / "docs/examples/en.yaml").read_text().split("\n", 1)[1])
        global_config.update(listen=f"127.0.0.1:{port}", api_token_env="SIRIUS_API_TOKEN", internal_token_env="SIRIUS_INTERNAL_TOKEN")
        (root / "sirius-api-config.yaml").write_text(json.dumps(global_config))
        proc = subprocess.Popen([str(exe)], cwd=root, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        try:
            for _ in range(100):
                if proc.poll() is not None:
                    raise RuntimeError(proc.stderr.read().decode())
                try:
                    code, protocol = request("/internal/v1/protocol", env["SIRIUS_INTERNAL_TOKEN"])
                    break
                except (OSError, urllib.error.URLError):
                    time.sleep(0.1)
            else:
                raise RuntimeError("Global packaged server did not start")
            assert code == 200 and protocol["codec"] == "native" and protocol["family"] == "global"
            code, regions = request("/api/v1/regions", env["SIRIUS_API_TOKEN"])
            assert code == 200 and regions["selected"] == "en"
            assert next(r for r in regions["regions"] if r["region"] == "cn")["reserved"]
            assert request("/api/v1/regions")[0] == 401
            assert request("/api/v1/players/by-profile-id/1", env["SIRIUS_API_TOKEN"])[0] == 501
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
        global_config["region"] = "cn"
        (root / "sirius-api-config.yaml").write_text(json.dumps(global_config))
        result = subprocess.run([str(exe)], cwd=root, env=env, capture_output=True, timeout=15)
        assert result.returncode != 0 and b"cn is reserved" in result.stderr
    print(f"Archive hashes, runtime files and offline startup passed: {m['name']} {m['version']} ({m['target']})")
