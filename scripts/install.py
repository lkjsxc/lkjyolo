#!/usr/bin/env python3
"""Install only into this user's ~/.local; pin and hash-check upstream engine."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
VERSION = "0.1.0"
ENGINE_VERSION = "0.2.1"
SHA256 = "790fc21733c51373d9967931a834325b2e9f49581e3e9187cf7fb54cb62435f6"
BASE = f"https://github.com/CeraCharlesCC/chatgpt-exec-mcp/releases/download/v{ENGINE_VERSION}/"


def main():
    os.umask(0o077)
    p = argparse.ArgumentParser()
    p.add_argument("--workspace", default=str(Path.home() / "workspace"))
    args = p.parse_args()
    if platform.system() != "Linux" or platform.machine() not in ("x86_64", "amd64"):
        p.error("Pinned binary installer supports Linux x86_64. See README for source builds.")
    release = Path.home() / ".local/share/lkjyolo/releases" / VERSION
    engine_dir = Path.home() / ".local/share/lkjyolo/engines" / ENGINE_VERSION
    release.mkdir(parents=True, exist_ok=True)
    engine_dir.mkdir(parents=True, exist_ok=True)
    binary = engine_dir / "chatgpt-exec-mcp"
    if not binary.exists() or hashlib.sha256(binary.read_bytes()).hexdigest() != SHA256:
        with urllib.request.urlopen(BASE + "chatgpt-exec-mcp-linux-x86_64", timeout=60) as response:
            payload = response.read(100 * 1024 * 1024)
        if hashlib.sha256(payload).hexdigest() != SHA256:
            raise RuntimeError("Upstream engine checksum mismatch; nothing installed")
        with tempfile.NamedTemporaryFile(dir=engine_dir, delete=False) as f:
            f.write(payload)
            temporary = Path(f.name)
        temporary.chmod(0o755)
        os.replace(temporary, binary)
    for name in ("LICENSE", "NOTICE", "THIRD_PARTY_NOTICES.md"):
        with urllib.request.urlopen(BASE + name, timeout=30) as response:
            (engine_dir / name).write_bytes(response.read(1024 * 1024))
    subprocess.run([sys.executable, str(ROOT / "scripts/build.py")], check=True, stdout=subprocess.DEVNULL)
    artifact = release / "lkjyolo.pyz"
    # Atomic replacement keeps concurrent callers on a complete zipapp.
    with tempfile.NamedTemporaryFile(dir=release, delete=False) as f:
        f.write((ROOT / "dist/lkjyolo.pyz").read_bytes())
        temporary = Path(f.name)
    temporary.chmod(0o755)
    os.replace(temporary, artifact)
    commands = Path.home() / ".local/bin"
    commands.mkdir(parents=True, exist_ok=True)
    link = commands / "lkjyolo"
    if link.exists() and not link.is_symlink():
        raise RuntimeError(f"Refusing to replace an existing non-symlink: {link}")
    staged = commands / (".lkjyolo-" + str(os.getpid()))
    staged.symlink_to(artifact)
    os.replace(staged, link)
    subprocess.run([str(link), "setup", "--engine", str(binary), "--sha256", SHA256,
                    "--workspace", args.workspace], check=True)
    print(json.dumps({"installed": str(link), "engine": str(binary), "engine_sha256": SHA256}))


if __name__ == "__main__":
    main()
