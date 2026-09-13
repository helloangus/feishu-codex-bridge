"""Offline wrapper contracts; never invoke the real Rust service or Cargo."""
from pathlib import Path
import subprocess
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[1]


class RustScriptTests(unittest.TestCase):
    def invoke(self, script, action):
        with tempfile.TemporaryDirectory(prefix="bridge wrappers ") as directory:
            root = Path(directory)
            binary = root / "fake bridge"
            binary.write_text('#!/bin/bash\nprintf "%s\\n" "$@"\n', encoding="utf-8")
            binary.chmod(0o700)
            codex = root / "codex"
            codex.write_text("#!/bin/bash\nexit 0\n", encoding="utf-8")
            codex.chmod(0o700)
            config = root / "private config.toml"
            env = {
                "PATH": f"{root}:/usr/bin:/bin",
                "BRIDGE_RUST_BIN": str(binary),
                "BRIDGE_RUST_CONFIG": str(config),
                "FEISHU_APP_SECRET": "offline-secret-must-not-be-an-argument",
            }
            result = subprocess.run(
                ["bash", str(REPO / script), action],
                input="", text=True, capture_output=True, env=env,
                cwd=root, timeout=5, check=False,
            )
            self.assertNotIn(env["FEISHU_APP_SECRET"], result.stdout + result.stderr)
            return result, str(config)

    def test_service_actions_preserve_config_argument(self):
        for action in ("status", "stop", "start", "restart"):
            with self.subTest(action=action):
                result, config = self.invoke("start-rust.sh", action)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.splitlines(), ["service", "--config", config, action])

    def test_foreground_uses_outer_guard(self):
        result, config = self.invoke("start-rust.sh", "foreground")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), ["guard", "--config", config])

    def test_unknown_action_does_not_invoke_binary(self):
        result, _ = self.invoke("start-rust.sh", "invalid")
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stdout, "")

    def test_setup_check_only_checks_config(self):
        result, config = self.invoke("setup-rust.sh", "--check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), ["config", "check", "--file", config])
