#!/usr/bin/env python3
"""Prepare and safely clean WRDP's disposable development directories."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import sys


def configured_paths() -> tuple[Path, dict[str, Path]]:
    # Make resolves this once via project-tmp.sh before replacing child TMPDIR.
    root = Path(os.environ["PROJECT_TMP_ROOT"])
    if not root.is_absolute() or root.name != "wrdp" or ".." in root.parts:
        raise ValueError("PROJECT_TMP_ROOT must be absolute and end in wrdp")
    if root != Path(os.environ["WRDP_TMP_ROOT"]):
        raise ValueError("WRDP_TMP_ROOT must match the resolved PROJECT_TMP_ROOT")
    for ancestor in (root, *root.parents):
        if ancestor != Path("/workspace") and ancestor.is_symlink():
            raise ValueError(f"symlink in scratch root: {ancestor}")
    if root.exists() and root.stat().st_uid != os.geteuid():
        raise PermissionError(f"scratch root is owned by another user: {root}")
    build_id = os.environ.get("WRDP_BUILD_ID", Path.cwd().name)
    if not build_id or build_id.startswith((".", "-")) or any(c not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-" for c in build_id):
        raise ValueError("WRDP_BUILD_ID must be a simple directory name")
    paths = {name: Path(os.environ[name]) for name in (
        "WRDP_RUN_DIR", "WRDP_TMPDIR", "TMPDIR", "TMP", "TEMP", "CARGO_HOME",
        "CARGO_TARGET_DIR", "COMPOSITOR_BUILD_DIR", "GROUND_INIT_BUILD_ROOT",
        "XDG_CACHE_HOME", "PYTHONPYCACHEPREFIX", "PIP_CACHE_DIR", "CCACHE_DIR",
        "SCCACHE_DIR", "BUN_INSTALL_CACHE_DIR", "npm_config_cache", "ASYNC_DIR",
        "TEST_ROOT", "LOG_ROOT",
    )}
    resolved_root = root.resolve()
    checked_components = set()
    for name, path in paths.items():
        category = "cache"
        if name in ("WRDP_RUN_DIR", "WRDP_TMPDIR", "TMPDIR", "TMP", "TEMP", "ASYNC_DIR"):
            category = "runs"
        elif name in ("CARGO_TARGET_DIR", "COMPOSITOR_BUILD_DIR", "GROUND_INIT_BUILD_ROOT"):
            category = "build"
        if name in ("TEST_ROOT", "LOG_ROOT"):
            category = "tests" if name == "TEST_ROOT" else "logs"
            if path != root / category:
                raise ValueError(f"{name} must be {root / category}")
        parent = root / category
        if category == "build":
            parent /= build_id
        if not path.is_absolute() or ".." in path.parts or not path.is_relative_to(parent) or (path == parent and name not in ("TEST_ROOT", "LOG_ROOT")):
            raise ValueError(f"{name} must be below {parent}")
        if category == "runs" and len(path.relative_to(root / "runs").parts) < 2:
            raise ValueError(f"{name} needs a purpose and run id")
        # /workspace itself may be a mount alias. Below the canonical root,
        # reject symlinks even when they happen to point back into the tree.
        for candidate in (root, *path.parents, path):
            if candidate.is_relative_to(root) and candidate not in checked_components:
                if candidate.is_symlink():
                    raise ValueError(f"symlink in scratch path: {candidate}")
                checked_components.add(candidate)
    if paths["ASYNC_DIR"] != root / "runs" / "async" / build_id:
        raise ValueError("ASYNC_DIR must be runs/async/<WRDP_BUILD_ID>")
    if any(paths[name] != paths["WRDP_TMPDIR"] for name in ("TMPDIR", "TMP", "TEMP")):
        raise ValueError("TMPDIR, TMP and TEMP must match WRDP_TMPDIR")
    evidence = Path(os.environ["WRDP_EVIDENCE_DIR"]).resolve()
    if evidence.is_relative_to(resolved_root):
        raise ValueError("retained evidence must be outside disposable scratch")
    return root, paths


def refuse_active_async(paths: dict[str, Path]) -> None:
    for pid_file in paths["ASYNC_DIR"].glob("*.pid"):
        try:
            pid = int(pid_file.read_text().strip())
            if pid <= 1:
                raise ValueError(f"invalid pid in {pid_file}")
            os.kill(pid, 0)
        except ProcessLookupError:
            continue
        raise RuntimeError(f"active or inaccessible async job: {pid_file}; stop and wait before cleanup")


def remove_owned(path: Path) -> None:
    if not path.exists():
        return
    if path.stat().st_uid != os.geteuid():
        raise PermissionError(f"refusing to delete another owner's directory: {path}")
    if not path.is_dir():
        raise ValueError(f"not a directory: {path}")
    shutil.rmtree(path)


def main() -> None:
    action = sys.argv[1]
    root, paths = configured_paths()
    if action == "show":
        print(json.dumps({"root": str(root), "PROJECT_ORIGINAL_TMPDIR": os.environ.get("PROJECT_ORIGINAL_TMPDIR", ""), **{k: str(v) for k, v in paths.items()}}, indent=2))
    elif action == "prepare":
        for path in paths.values():
            path.mkdir(parents=True, exist_ok=True)
    elif action in ("clean-build", "clean-compositor", "clean-async"):
        refuse_active_async(paths)
        names = ["COMPOSITOR_BUILD_DIR"]
        if action != "clean-compositor":
            names += ["CARGO_TARGET_DIR", "GROUND_INIT_BUILD_ROOT"]
        if action == "clean-async":
            names += ["ASYNC_DIR"]
        # Validate every path/owner before making any destructive change.
        for name in names:
            path = paths[name]
            if path.exists() and path.stat().st_uid != os.geteuid():
                raise PermissionError(f"refusing to delete another owner's directory: {path}")
        for name in names:
            remove_owned(paths[name])
    else:
        raise ValueError(f"unknown action: {action}")


if __name__ == "__main__":
    try:
        main()
    except (KeyError, ValueError, OSError, RuntimeError) as error:
        sys.exit(str(error))
