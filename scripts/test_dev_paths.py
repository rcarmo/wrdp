"""Development path guards, using isolated fixtures rather than real project state."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import subprocess
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("dev_paths", Path(__file__).with_name("dev-paths.py"))
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class DevelopmentPathsTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.parent = Path(self.temporary.name)
        self.root = self.parent / "wrdp"
        self.root.mkdir()
        env = {"PROJECT_TMP_ROOT": str(self.root), "WRDP_TMP_ROOT": str(self.root),
               "WRDP_BUILD_ID": "fixture", "WRDP_EVIDENCE_DIR": str(self.parent / "evidence")}
        for name in ("WRDP_RUN_DIR", "WRDP_TMPDIR", "TMPDIR", "TMP", "TEMP"):
            env[name] = str(self.root / "runs/tests/fixture/tmp")
        for name in ("CARGO_TARGET_DIR", "COMPOSITOR_BUILD_DIR", "GROUND_INIT_BUILD_ROOT"):
            env[name] = str(self.root / "build/fixture" / name.lower())
        for name in ("CARGO_HOME", "XDG_CACHE_HOME", "PYTHONPYCACHEPREFIX", "PIP_CACHE_DIR",
                     "CCACHE_DIR", "SCCACHE_DIR", "BUN_INSTALL_CACHE_DIR", "npm_config_cache"):
            env[name] = str(self.root / "cache" / name.lower())
        env["ASYNC_DIR"] = str(self.root / "runs/async/fixture")
        self.env = env
        self.environment = patch.dict(os.environ, env)
        self.environment.start()
        self.addCleanup(self.environment.stop)

    def test_resolver_explicit_invalid_does_not_fall_back(self):
        for value in ("", "relative/wrdp", str(self.parent / "other")):
            with patch.dict(os.environ, {"PROJECT_TMP_ROOT": value}):
                process = subprocess.run(["bash", "scripts/project-tmp.sh", "paths"],
                                         env={**os.environ, "PROJECT": "wrdp"}, capture_output=True, text=True)
                self.assertNotEqual(process.returncode, 0)

    def test_resolver_runner_and_original_tmp_fallback(self):
        missing_workspace = self.parent / "missing/workspace/tmp"
        runner = self.parent / "runner"
        original_tmp = self.parent / "original"
        runner.mkdir()
        original_tmp.mkdir()
        env = {key: value for key, value in os.environ.items() if key != "PROJECT_TMP_ROOT"}
        env.update(RUNNER_TEMP=str(runner), TMPDIR=str(original_tmp))
        command = ["bash", "-c", 'source scripts/project-tmp.sh; project_tmp_resolve wrdp "$1"', "bash", str(missing_workspace)]
        self.assertEqual(subprocess.check_output(command, env=env, text=True).strip(), str(runner / "wrdp"))
        env.pop("RUNNER_TEMP")
        self.assertEqual(subprocess.check_output(command, env=env, text=True).strip(), str(original_tmp / "wrdp"))

    def test_make_preserves_explicit_root_and_child_tmp(self):
        output = subprocess.check_output(["make", "--no-print-directory", "paths"], text=True)
        result = json.loads(output)
        self.assertEqual(result["root"], str(self.root))
        self.assertTrue(Path(result["TMPDIR"]).is_relative_to(self.root / "runs"))
        self.assertEqual(result["TMPDIR"], result["TEMP"])
        process = subprocess.run(["make", "--no-print-directory", "paths", "PROJECT_TMP_ROOT=relative/wrdp"],
                                 capture_output=True, text=True)
        self.assertNotEqual(process.returncode, 0)

    def test_ci_exports_resolved_paths(self):
        env_file = self.parent / "github-env"
        env = {**os.environ, "GITHUB_ENV": str(env_file), "WRDP_RUN_ID": "fixture-ci"}
        subprocess.run(["bash", "scripts/ci-paths.sh"], env=env, check=True)
        values = dict(line.split("=", 1) for line in env_file.read_text().splitlines())
        self.assertEqual(values["PROJECT_TMP_ROOT"], str(self.root))
        self.assertEqual(values["TMPDIR"], str(self.root / "runs/ci/fixture-ci/tmp"))
        self.assertEqual(values["CARGO_HOME"], str(self.root / "cache/cargo"))
        self.assertTrue(Path(values["TMPDIR"]).is_dir())

    def test_async_start_does_not_overwrite_active_job(self):
        async_dir = Path(self.env["ASYNC_DIR"])
        async_dir.mkdir(parents=True)
        (async_dir / "check.pid").write_text(str(os.getpid()))
        pointer = async_dir / "check.log-path"
        pointer.write_text("existing-log")
        output = subprocess.check_output(["make", "--no-print-directory", "async-start", "ASYNC_TARGET=check"], text=True)
        self.assertIn("already running", output)
        self.assertEqual(pointer.read_text(), "existing-log")
        self.assertFalse((async_dir / "check.status").exists())

    def test_async_dry_run_does_not_start_a_job(self):
        output = subprocess.check_output(["make", "--no-print-directory", "-n", "async-start", "ASYNC_TARGET=check"], text=True)
        self.assertIn("dry run: would start", output)
        self.assertFalse((Path(self.env["ASYNC_DIR"]) / "check.pid").exists())

    def test_valid_project_root(self):
        root, paths = MODULE.configured_paths()
        self.assertEqual(root, self.root)
        self.assertTrue(paths["CARGO_TARGET_DIR"].is_relative_to(root / "build/fixture"))

    def test_rejects_target_outside_checkout(self):
        for target in (self.parent, self.root / "build/other/cargo", self.root / "build/fixture"):
            with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(target)}):
                with self.assertRaises(ValueError):
                    MODULE.configured_paths()

    def test_rejects_symlink(self):
        link = self.root / "cache"
        link.symlink_to(self.parent, target_is_directory=True)
        with self.assertRaises(ValueError):
            MODULE.configured_paths()

    def test_rejects_evidence_in_scratch(self):
        with patch.dict(os.environ, {"WRDP_EVIDENCE_DIR": str(self.root / "runs/evidence")}):
            with self.assertRaises(ValueError):
                MODULE.configured_paths()

    def test_active_async_blocks_cleanup(self):
        _, paths = MODULE.configured_paths()
        paths["ASYNC_DIR"].mkdir(parents=True)
        (paths["ASYNC_DIR"] / "test.pid").write_text(str(os.getpid()))
        with self.assertRaises(RuntimeError):
            MODULE.refuse_active_async(paths)

    def test_clean_preserves_evidence_and_cache(self):
        _, paths = MODULE.configured_paths()
        target = paths["COMPOSITOR_BUILD_DIR"]
        target.mkdir(parents=True)
        (target / "build.ninja").write_text("fixture")
        evidence = Path(self.env["WRDP_EVIDENCE_DIR"])
        evidence.mkdir()
        paths["CARGO_HOME"].mkdir(parents=True)
        MODULE.remove_owned(target)
        self.assertFalse(target.exists())
        self.assertTrue(evidence.exists())
        self.assertTrue(paths["CARGO_HOME"].exists())


if __name__ == "__main__":
    unittest.main()
