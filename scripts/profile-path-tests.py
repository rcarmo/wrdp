#!/usr/bin/env python3
"""Profile focused development-path tests; retain evidence outside disposable scratch."""
import cProfile
import hashlib
import json
import os
from pathlib import Path
import pstats
import subprocess
import sys
import time
import tracemalloc
import unittest


def main():
    evidence = Path(os.environ["WRDP_EVIDENCE_DIR"]) / "path-tests" / os.environ["WRDP_RUN_ID"]
    evidence.mkdir(parents=True, exist_ok=False)
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    subprocess.run(["git", "diff", "--binary"], stdout=(evidence / "changes.patch").open("w"), check=True)
    # Retain exact helper/test sources, including new untracked scripts.
    sources = {}
    for path in Path("scripts").glob("*.py"):
        data = path.read_bytes()
        destination = evidence / "sources" / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)
        sources[str(path)] = hashlib.sha256(data).hexdigest()
    for path in (Path("scripts/project-tmp.sh"), Path("scripts/ci-paths.sh"), Path("Makefile"), Path("vendor/ground-init/ground-init.py"), Path("vendor/ground-init/test_ground_init.py")):
        data = path.read_bytes()
        destination = evidence / "sources" / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)
        sources[str(path)] = hashlib.sha256(data).hexdigest()
    (evidence / "metadata.json").write_text(json.dumps({
        "revision": revision, "python": sys.version, "command": sys.argv, "source_sha256": sources,
        "patterns": ["test_dev_paths.py", "test_provision_user.py", "test_ground_init.py"],
        "TMPDIR": os.environ["TMPDIR"], "PROJECT_TMP_ROOT": os.environ["PROJECT_TMP_ROOT"],
        "cpu_capture": "cProfile with process_time timer: process CPU/call timing, including test harness",
        "heap_capture": "tracemalloc: one-frame live allocation snapshot and peak bytes; not cumulative allocation counts",
        "limitations": "native allocator and subprocess CPU/heap are not captured; no performance conclusion from this smoke suite",
    }, indent=2))
    profiler = cProfile.Profile(timer=time.process_time)
    tracemalloc.start()
    started = time.monotonic()
    result = None
    try:
        profiler.enable()
        suite = unittest.TestSuite()
        for pattern in ("test_dev_paths.py", "test_provision_user.py"):
            suite.addTests(unittest.TestLoader().discover("scripts", pattern=pattern))
        suite.addTests(unittest.TestLoader().discover("vendor/ground-init", pattern="test_ground_init.py"))
        with (evidence / "tests.log").open("w") as log:
            result = unittest.TextTestRunner(stream=log, verbosity=2).run(suite)
    finally:
        profiler.disable()
        profiler.dump_stats(str(evidence / "cpu.prof"))
        snapshot = tracemalloc.take_snapshot()
        snapshot.dump(str(evidence / "heap.snapshot"))
        current, peak = tracemalloc.get_traced_memory()
        with (evidence / "profile-summary.txt").open("w") as output:
            output.write(f"Elapsed {time.monotonic() - started:.4f}s; live={current} bytes; peak={peak} bytes\n")
            pstats.Stats(profiler, stream=output).strip_dirs().sort_stats("cumulative").print_stats(25)
            output.write("\nLive allocations by source (not cumulative allocations):\n")
            for stat in snapshot.statistics("lineno")[:25]:
                output.write(f"{stat}\n")
        tracemalloc.stop()
    if (evidence / "tests.log").exists():
        print((evidence / "tests.log").read_text())
    print(f"Retained CPU/heap/log evidence: {evidence}")
    return 0 if result and result.wasSuccessful() else 1


if __name__ == "__main__":
    sys.exit(main())
