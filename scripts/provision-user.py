#!/usr/bin/env python3
"""Run WRDP's user ground-init profile under one validated local account."""

from __future__ import annotations

import os
import pwd
import re
import subprocess
import tempfile
from contextlib import contextmanager
from pathlib import Path


VALID_USER = re.compile(r"[a-z_][a-z0-9_.-]{0,31}\Z")


@contextmanager
def user_scratch(account):
    """Keep subprocess temporary writes isolated and owned by the target user."""
    root = Path(os.environ.get("PROJECT_TMP_ROOT", ""))
    run_dir = Path(os.environ.get("WRDP_RUN_DIR", ""))
    if (not root.is_absolute() or root.name != "wrdp" or not run_dir.is_absolute()
            or not run_dir.is_relative_to(root / "runs") or run_dir == root / "runs"
            or ".." in run_dir.parts or not run_dir.resolve().is_relative_to((root / "runs").resolve())):
        raise SystemExit("provision-user requires a project-owned WRDP_RUN_DIR (use make)")
    for path in (run_dir, *run_dir.parents):
        if path != Path("/workspace") and path.is_symlink():
            raise SystemExit(f"symlink in provisioning scratch path: {path}")
    if not run_dir.is_dir() or run_dir.stat().st_uid != os.geteuid():
        raise SystemExit("provisioning scratch parent must be owned by the invoking user")
    # The parent is created by Make; the randomly named child remains isolated
    # from real account state and is removed only after runuser has finished.
    with tempfile.TemporaryDirectory(prefix="provision-user-", dir=run_dir) as directory:
        os.chown(directory, account.pw_uid, account.pw_gid)
        yield directory


def main() -> int:
    if os.geteuid() != 0:
        raise SystemExit("provision-user must run as root")
    username = os.environ.get("PROVISION_USER", "")
    if not VALID_USER.fullmatch(username):
        raise SystemExit("PROVISION_USER must be a simple local account name")
    try:
        account = pwd.getpwnam(username)
    except KeyError as error:
        raise SystemExit(f"unknown local account: {username}") from error
    home = Path(account.pw_dir)
    if not home.is_absolute() or home == Path("/"):
        raise SystemExit(f"unsafe home directory for {username}: {home}")
    source = Path(os.environ.get("WRDP_SOURCE_DIR", "")).resolve()
    runner = source / "vendor/ground-init/ground-init.py"
    profile = source / "ground-init.user.yaml"
    if not runner.is_file() or not profile.is_file():
        raise SystemExit("WRDP_SOURCE_DIR does not contain the user profile")

    environment = {
        "HOME": str(home),
        "LANG": os.environ.get("LANG", "C.UTF-8"),
        "PATH": "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    }
    with user_scratch(account) as directory:
        environment.update(
            TMPDIR=directory, TMP=directory, TEMP=directory,
            PYTHONDONTWRITEBYTECODE="1",
        )
        completed = subprocess.run(
            [
                "/usr/sbin/runuser",
                "-u",
                username,
                "--",
                "/usr/bin/python3",
                str(runner),
                str(profile),
            ],
            env=environment,
            check=False,
        )
    return completed.returncode


if __name__ == "__main__":
    raise SystemExit(main())
