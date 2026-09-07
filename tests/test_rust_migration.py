"""Cross-language state regression; uses only short-lived CLI and temp files."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

os.environ.setdefault("FEISHU_APP_ID", "test-app")
os.environ.setdefault("FEISHU_APP_SECRET", "test-secret")
import bridge

RUST_BRIDGE = Path(__file__).resolve().parents[1] / "target/debug/bridge"


@unittest.skipUnless(RUST_BRIDGE.is_file(), "run cargo test --workspace first")
class RustMigrationTests(unittest.TestCase):
    def run_cli(self, *args):
        return subprocess.run(
            [str(RUST_BRIDGE), *map(str, args)],
            capture_output=True, text=True, timeout=10, check=True,
        )

    def test_rust_export_is_readable_by_existing_python_bridge(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "python"
            source.mkdir()
            key = "user:/tmp/project"
            (source / ".feishu-codex-session").write_text(json.dumps({key: "thread"}))
            (source / ".feishu-codex-settings").write_text(json.dumps({
                "models": {key: "model"}, "directories": {"user": "/tmp/project"},
                "plan_modes": {key: True},
            }))
            destination = root / "rust"
            exported = root / "exported"
            self.run_cli("migrate", "import-python", "--source", source,
                         "--destination", destination, "--dry-run")
            self.assertFalse(destination.exists())
            self.run_cli("migrate", "import-python", "--source", source,
                         "--destination", destination)
            self.run_cli("migrate", "export-python", "--state-dir", destination,
                         "--output", exported)
            item = bridge.Bridge.__new__(bridge.Bridge)
            with patch.object(bridge, "SESSION_FILE", exported / ".feishu-codex-session"), \
                    patch.object(bridge, "SETTINGS_FILE", exported / ".feishu-codex-settings"):
                self.assertEqual(item.load_session(key), "thread")
                models, directories, plans = item.load_settings()
                self.assertEqual(models[key], "model")
                self.assertEqual(directories["user"], "/tmp/project")
                self.assertTrue(plans[key])
