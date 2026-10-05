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
        env["TEST_ROOT"] = str(self.root / "tests")
        env["LOG_ROOT"] = str(self.root / "logs")
        self.env = env
        # An outer explicit base must not conflict with this isolated fixture root.
        inherited = {key: value for key, value in os.environ.items() if key != "PROJECT_TMP_BASE"}
        self.environment = patch.dict(os.environ, {**inherited, **env}, clear=True)
        self.environment.start()
        self.addCleanup(self.environment.stop)

    def test_resolver_explicit_invalid_does_not_fall_back(self):
        for value in ("", "relative/wrdp", str(self.parent / "other")):
            with patch.dict(os.environ, {"PROJECT_TMP_ROOT": value}):
                process = subprocess.run(["bash", "scripts/project-tmp.sh", "paths"],
                                         env={**os.environ, "PROJECT": "wrdp"}, capture_output=True, text=True)
                self.assertNotEqual(process.returncode, 0)

    def resolver_env(self):
        omitted = {"PROJECT_TMP_ROOT", "PROJECT_TMP_BASE", "PROJECT_ORIGINAL_TMPDIR", "RUNNER_TEMP", "TMPDIR",
                   "CI", "GITHUB_ACTIONS", "GITLAB_CI", "TF_BUILD", "CIRCLECI"}
        return {key: value for key, value in os.environ.items() if key not in omitted}

    def resolve(self, environment, workspace=None, script=None):
        command = ["bash", "-c", script or 'source scripts/project-tmp.sh; project_tmp_resolve wrdp "$1"',
                   "bash", str(workspace or self.parent / "workspace")]
        return subprocess.run(command, env=environment, capture_output=True, text=True)

    def test_resolver_ci_prefers_runner_even_with_usable_workspace(self):
        workspace = self.parent / "workspace"
        runner = self.parent / "runner"
        original = self.parent / "original"
        for path in (workspace, runner, original):
            path.mkdir()
        env = {**self.resolver_env(), "CI": "true", "RUNNER_TEMP": str(runner), "TMPDIR": str(original)}
        process = self.resolve(env, workspace)
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(process.stdout.strip(), str(runner / "wrdp"))
        # An unusable runner hint is skipped, not used as a project root.
        runner_file = self.parent / "runner-file"
        runner_file.write_text("not a directory")
        env["RUNNER_TEMP"] = str(runner_file)
        process = self.resolve(env, workspace)
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(process.stdout.strip(), str(original / "wrdp"))
        env.pop("RUNNER_TEMP")
        self.assertEqual(self.resolve(env, workspace).stdout.strip(), str(original / "wrdp"))
        env.pop("TMPDIR")
        process = self.resolve(env, workspace)
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(process.stdout.strip(), "/tmp/wrdp")

    def test_resolver_local_ignores_runner_and_original_tmp(self):
        workspace = self.parent / "workspace"
        workspace.mkdir()
        env = {**self.resolver_env(), "CI": "false", "RUNNER_TEMP": str(self.parent / "runner"),
               "TMPDIR": str(self.parent / "original")}
        self.assertEqual(self.resolve(env, workspace).stdout.strip(), str(workspace / "wrdp"))
        process = self.resolve(env, self.parent / "missing/workspace/tmp")
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(process.stdout.strip(), "/tmp/wrdp")

    def test_resolver_ci_provider_flag_and_original_snapshot(self):
        original = self.parent / "original"
        original.mkdir()
        env = {**self.resolver_env(), "GITHUB_ACTIONS": "true", "TMPDIR": str(original)}
        script = 'source scripts/project-tmp.sh; export TMPDIR="$1/runs/child"; source scripts/project-tmp.sh; project_tmp_resolve wrdp "$1"'
        process = self.resolve(env, script=script)
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(process.stdout.strip(), str(original / "wrdp"))

    def test_resolver_base_root_compatibility(self):
        base = self.parent / "base with spaces"
        env = {**self.resolver_env(), "PROJECT_TMP_BASE": str(base)}
        for ci in ("true", "false"):
            env["CI"] = ci
            process = self.resolve(env)
            self.assertEqual(process.returncode, 0, process.stderr)
            self.assertEqual(process.stdout.strip(), str(base / "wrdp"))
        env["PROJECT_TMP_ROOT"] = str(base / "wrdp")
        self.assertEqual(self.resolve(env).returncode, 0)
        env["PROJECT_TMP_ROOT"] = str(self.root)
        self.assertNotEqual(self.resolve(env).returncode, 0)
        env.pop("PROJECT_TMP_ROOT")
        symlink = self.parent / "base-link"
        symlink.symlink_to(self.parent, target_is_directory=True)
        for bad in ("", "relative", str(self.parent / ".." / "elsewhere"), str(symlink)):
            env["PROJECT_TMP_BASE"] = bad
            self.assertNotEqual(self.resolve(env).returncode, 0)
        file_base = self.parent / "base-file"
        file_base.write_text("occupied")
        env["PROJECT_TMP_BASE"] = str(file_base)
        self.assertNotEqual(self.resolve(env).returncode, 0)

    def test_make_command_line_base_and_snapshot_survive_child(self):
        original = self.parent / "original"
        env = self.resolver_env()
        env.pop("MAKEFLAGS", None)
        env.pop("MAKELEVEL", None)
        # Defaults unrelated to resolver are fixture-owned; root comes from Make CLI.
        process = subprocess.run(["make", "--no-print-directory", "paths",
                                  f"PROJECT_TMP_BASE={self.parent}", f"TMPDIR={original}"],
                                 env=env, capture_output=True, text=True)
        # CLI TMPDIR intentionally conflicts with required run temp and must fail validation.
        self.assertNotEqual(process.returncode, 0)
        env["TMPDIR"] = str(original)
        result = json.loads(subprocess.check_output(["make", "--no-print-directory", "paths",
                                                    f"PROJECT_TMP_BASE={self.parent}"], env=env, text=True))
        self.assertEqual(result["root"], str(self.root))
        self.assertEqual(result["PROJECT_ORIGINAL_TMPDIR"], str(original))
        child_env = {**env, "PROJECT_TMP_ROOT": result["root"], "PROJECT_ORIGINAL_TMPDIR": result["PROJECT_ORIGINAL_TMPDIR"],
                     "TMPDIR": result["TMPDIR"]}
        child = json.loads(subprocess.check_output(["make", "--no-print-directory", "paths"], env=child_env, text=True))
        self.assertEqual(child["root"], str(self.root))
        self.assertEqual(child["PROJECT_ORIGINAL_TMPDIR"], str(original))
        for args in (["PROJECT_TMP_BASE=relative"], [f"PROJECT_TMP_BASE={self.parent / 'other'}", f"PROJECT_TMP_ROOT={self.root}"]):
            process = subprocess.run(["make", "--no-print-directory", "paths", *args], env=env, capture_output=True, text=True)
            self.assertNotEqual(process.returncode, 0)

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
        self.assertEqual(values["PROJECT_ORIGINAL_TMPDIR"], env.get("PROJECT_ORIGINAL_TMPDIR", env["TMPDIR"]))
        for category in ("cache", "build", "tests", "logs", "runs"):
            self.assertTrue((self.root / category).is_dir())

    def test_ci_base_override_is_propagated(self):
        env_file = self.parent / "github-env"
        env = {**self.resolver_env(), "PROJECT_TMP_BASE": str(self.parent), "CI": "true",
               "GITHUB_ENV": str(env_file), "WRDP_RUN_ID": "base-ci", "TMPDIR": str(self.parent / "original")}
        subprocess.run(["bash", "scripts/ci-paths.sh"], env=env, check=True)
        values = dict(line.split("=", 1) for line in env_file.read_text().splitlines())
        self.assertEqual(values["PROJECT_TMP_BASE"], str(self.parent))
        self.assertEqual(values["PROJECT_TMP_ROOT"], str(self.root))
        self.assertEqual(values["PROJECT_ORIGINAL_TMPDIR"], env["TMPDIR"])
        self.assertEqual(values["TEST_ROOT"], str(self.root / "tests"))
        self.assertEqual(values["LOG_ROOT"], str(self.root / "logs"))
        # A recursive resolver keeps the exported root rather than nesting wrdp.
        child = self.resolve({**env, **values})
        self.assertEqual(child.returncode, 0, child.stderr)
        self.assertEqual(child.stdout.strip(), str(self.root))

    def test_make_ci_selection_and_original_tmp(self):
        runner = self.parent / "runner"
        original = self.parent / "original"
        for path in (runner, original):
            path.mkdir()
        # Clear child-path values from the outer Make/fixture; this is a fresh job.
        env = {"PATH": os.environ["PATH"], "HOME": os.environ["HOME"],
               "PYTHONDONTWRITEBYTECODE": "1", "CI": "true", "RUNNER_TEMP": str(runner),
               "TMPDIR": str(original)}
        result = json.loads(subprocess.check_output(["make", "--no-print-directory", "paths"], env=env, text=True))
        self.assertEqual(result["root"], str(runner / "wrdp"))
        self.assertEqual(result["PROJECT_ORIGINAL_TMPDIR"], str(original))
        env.pop("RUNNER_TEMP")
        result = json.loads(subprocess.check_output(["make", "--no-print-directory", "paths"], env=env, text=True))
        self.assertEqual(result["root"], str(original / "wrdp"))

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
