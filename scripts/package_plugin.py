#!/usr/bin/env python3
"""Package the existing Home Coder identity, including compatibility metadata."""
import json
from pathlib import Path
import zipfile

root = Path(__file__).resolve().parents[1]
source = root / "plugins/home-coder"
manifest = json.loads((source / "plugin.json").read_text())
legacy = json.loads((source / ".codex-plugin/plugin.json").read_text())
interface = manifest["extensions"]["com.openai"]["interface"]
assert manifest["name"] == legacy["name"] == "home-coder"
assert manifest["version"] == legacy["version"]
assert interface == legacy["interface"]
assert len(interface["shortDescription"]) <= 30
assert manifest["extensions"]["com.openai"]["apps"] == "./.app.json"
assert json.loads((source / ".app.json").read_text())["apps"]["connector"]["id"] == "asdk_app_6ab5d7fa83288191aa3f95f47f065380"
mcp = json.loads((source / "mcp.json").read_text())["mcpServers"]
assert mcp["home-coder"]["url"] == "https://coder.lkjsxc.com/api/experimental/mcp/http"
out = root / "dist" / f"home-coder-{manifest['version']}.zip"
out.parent.mkdir(exist_ok=True)
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as archive:
    for file in sorted(source.rglob("*")):
        if file.is_symlink():
            raise RuntimeError(f"Unexpected symlink: {file}")
        if file.is_file():
            archive.write(file, "home-coder/" + file.relative_to(source).as_posix())
print(out)
