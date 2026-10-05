import importlib.util
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("ground-init.py")
SPEC = importlib.util.spec_from_file_location("ground_init", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


class GroundInitLocalHandlersTests(unittest.TestCase):
    def test_build_requires_explicit_root(self):
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaisesRegex(ValueError, "explicit root"):
                MODULE.on_build({})

    def test_build_uses_configured_project_root(self):
        with tempfile.TemporaryDirectory() as temporary:
            build = Path(temporary) / "build"
            with patch.dict(os.environ, {"GROUND_INIT_BUILD_ROOT": str(build)}):
                MODULE.on_build({"repositories": []})
            self.assertTrue(build.is_dir())

    def test_environment_expansion(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / "profile.yaml"
            config.write_text("path: ${GROUND_INIT_TEST_HOME}/Desktop\n", encoding="utf-8")
            with patch.dict(os.environ, {"GROUND_INIT_TEST_HOME": "/home/tester"}):
                self.assertEqual(MODULE.get_context(str(config))["path"], "/home/tester/Desktop")

    def test_copy_handlers_are_idempotent(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source_file = root / "source" / "item.txt"
            source_file.parent.mkdir()
            source_file.write_text("first\n", encoding="utf-8")
            source_tree = root / "tree"
            source_tree.mkdir()
            (source_tree / "nested.txt").write_text("nested\n", encoding="utf-8")

            directory = root / "destination" / "directory"
            copied_file = root / "destination" / "item.txt"
            copied_tree = root / "destination" / "tree"
            MODULE.on_directories([{"path": str(directory), "permissions": "0750"}])
            MODULE.on_copy_files(
                [{"source": str(source_file), "destination": str(copied_file), "permissions": "0640"}]
            )
            MODULE.on_copy_trees(
                [{"source": str(source_tree), "destination": str(copied_tree)}]
            )

            source_file.write_text("second\n", encoding="utf-8")
            (source_tree / "nested.txt").write_text("updated\n", encoding="utf-8")
            MODULE.on_directories([{"path": str(directory), "permissions": "0750"}])
            MODULE.on_copy_files(
                [{"source": str(source_file), "destination": str(copied_file), "permissions": "0640"}]
            )
            MODULE.on_copy_trees(
                [{"source": str(source_tree), "destination": str(copied_tree)}]
            )

            self.assertEqual(copied_file.read_text(encoding="utf-8"), "second\n")
            self.assertEqual((copied_tree / "nested.txt").read_text(encoding="utf-8"), "updated\n")
            self.assertEqual(directory.stat().st_mode & 0o777, 0o750)
            self.assertEqual(copied_file.stat().st_mode & 0o777, 0o640)
            self.assertEqual(copied_file.stat().st_uid, os.geteuid())
            self.assertEqual(copied_file.stat().st_gid, os.getegid())
            self.assertEqual(copied_tree.stat().st_uid, os.geteuid())
            self.assertEqual((copied_tree / "nested.txt").stat().st_uid, os.geteuid())


if __name__ == "__main__":
    unittest.main()
