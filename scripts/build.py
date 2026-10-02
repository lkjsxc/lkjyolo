#!/usr/bin/env python3
"""Build a stdlib-only zipapp; no package installation required at runtime."""
import pathlib
import shutil
import tempfile
import zipapp

root = pathlib.Path(__file__).resolve().parents[1]
out = root / "dist/lkjyolo.pyz"
out.parent.mkdir(exist_ok=True)
with tempfile.TemporaryDirectory() as tmp:
    shutil.copytree(root / "lkjyolo", pathlib.Path(tmp) / "lkjyolo", ignore=shutil.ignore_patterns("__pycache__"))
    zipapp.create_archive(tmp, target=out, interpreter="/usr/bin/env python3", main="lkjyolo.cli:main", compressed=True)
out.chmod(0o755)
print(out)
