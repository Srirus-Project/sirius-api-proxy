#!/usr/bin/env python3
"""Offline container smoke checks. Only containers created here are removed."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile
import time
import tomllib

p = argparse.ArgumentParser()
p.add_argument("image")
a = p.parse_args()
root = Path(__file__).resolve().parents[1]
meta = tomllib.loads((root / "Cargo.toml").read_text())["package"]
name, version = meta["name"], meta["version"]
def docker(*args):
    return subprocess.check_output(["docker", *args], text=True, stderr=subprocess.PIPE, timeout=30).strip()
assert docker("run", "--rm", "--network", "none", a.image, "--version") == name + " " + version
uid = docker("run", "--rm", "--network", "none", "--entrypoint", "id", a.image, "-u")
assert uid != "0"
docker("run", "--rm", "--network", "none", "--entrypoint", "sh", a.image,
       "-c", f"test -f /usr/share/licenses/{name}/LICENSE")
with tempfile.TemporaryDirectory() as tmp:
    config_name = "sirius-api-config" if name == "sirius-api-proxy" else "sirius-asset-config"
    config = Path(tmp) / (config_name + ".yaml")
    config.write_text((root / (config_name + ".example.yaml")).read_text())
    config.chmod(0o644)
    options = ["--network", "none", "-v", f"{config}:/app/{config.name}:ro",
               "-e", "SIRIUS_API_TOKEN=smoke-public-token", "-e", "SIRIUS_INTERNAL_TOKEN=smoke-internal-token",
               "-e", "SIRIUS_CDN_USERNAME=smoke-user", "-e", "SIRIUS_CDN_CREDENTIAL=smoke-password"]
    if name == "sirius-asset-updater":
        assert json.loads(docker("run", "--rm", *options, a.image, "check"))["ready"]
        docker("run", "--rm", "--network", "none", "--entrypoint", "ffmpeg", a.image, "-version")
        docker("run", "--rm", "--network", "none", "--entrypoint", "sh", a.image,
               "-c", "test -w /app/downloads && test -w /app/exports")
    else:
        # master_git runs the git executable inside the container.
        docker("run", "--rm", "--network", "none", "--entrypoint", "git", a.image, "--version")
        # SSH commit signing (master_git.signing.format: ssh) needs ssh-keygen: sign and verify.
        docker("run", "--rm", "--network", "none", "--entrypoint", "sh", a.image, "-c",
               "set -e; cd /tmp; ssh-keygen -q -t ed25519 -N '' -f k; git init -q r; cd r; "
               "git -c user.name=smoke -c user.email=smoke@example.invalid -c gpg.format=ssh "
               "-c user.signingkey=/tmp/k.pub commit -q --allow-empty -S -m smoke; "
               "echo \"smoke@example.invalid $(cat /tmp/k.pub)\" > /tmp/allowed; "
               "git -c gpg.ssh.allowedSignersFile=/tmp/allowed verify-commit HEAD")
        container = docker("run", "-d", *options, a.image)
        try:
            for _ in range(60):
                try:
                    health = json.loads(docker("exec", container, "wget", "-qO-", "http://127.0.0.1:9999/health"))
                    break
                except subprocess.CalledProcessError:
                    time.sleep(0.2)
            else:
                raise RuntimeError("Container failed to become healthy")
            assert health["version"] == version
            protocol = json.loads(docker("exec", container, "wget", "-qO-", "--header",
                "Authorization: Bearer smoke-internal-token", "http://127.0.0.1:9999/internal/v1/protocol"))
            assert protocol["codec"] == "native"
        finally:
            docker("rm", "-f", container)
print(f"Offline container startup, version, non-root user and runtime dependencies passed: {name} {version}")
