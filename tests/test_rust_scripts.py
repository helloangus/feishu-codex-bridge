"""Offline wrapper contracts; never invoke the real Rust service or Cargo."""
from pathlib import Path
import os
import pty
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
            cargo = root / "cargo"
            cargo.write_text("#!/bin/bash\nexit 0\n", encoding="utf-8")
            cargo.chmod(0o700)
            cc = root / "cc"
            cc.write_text("#!/bin/bash\nexit 0\n", encoding="utf-8")
            cc.chmod(0o700)
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

    def test_setup_no_start_uses_defaults_without_a_terminal(self):
        with tempfile.TemporaryDirectory(prefix="bridge setup ") as directory:
            root = Path(directory)
            binary = root / "fake bridge"
            binary.write_text('#!/bin/bash\nprintf "%s\\n" "$@"\n', encoding="utf-8")
            binary.chmod(0o700)
            for name in ("cargo", "cc", "rustup", "codex"):
                command = root / name
                command.write_text("#!/bin/bash\nexit 0\n", encoding="utf-8")
                command.chmod(0o700)
            config = root / "bridge.toml"
            env = {
                "PATH": f"{root}:/usr/bin:/bin",
                "BRIDGE_RUST_BIN": str(binary),
                "BRIDGE_RUST_CONFIG": str(config),
                "HOME": str(root),
            }
            result = subprocess.run(
                [str(REPO / "setup-rust.sh"), "--no-start"],
                input="",
                text=True,
                capture_output=True,
                env=env,
                cwd=root,
                timeout=5,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                result.stdout.splitlines(),
                [
                    "config",
                    "init",
                    "--output",
                    str(config),
                    "--root",
                    str(root),
                    "--cwd",
                    str(root),
                    "--state-dir",
                    str(root / ".local/state/feishu-codex-bridge"),
                    "准备完成。启动：./start-rust.sh start；状态：./start-rust.sh status。",
                ],
            )

    def test_setup_starts_with_noninteractive_feishu_credentials(self):
        with tempfile.TemporaryDirectory(prefix="bridge setup start ") as directory:
            root = Path(directory)
            binary = root / "fake bridge"
            binary.write_text('#!/bin/bash\nprintf "%s\\n" "$@"\n', encoding="utf-8")
            binary.chmod(0o700)
            for name in ("cargo", "cc", "rustup", "codex"):
                command = root / name
                command.write_text("#!/bin/bash\nexit 0\n", encoding="utf-8")
                command.chmod(0o700)
            config = root / "bridge.toml"
            env = {
                "PATH": f"{root}:/usr/bin:/bin",
                "HOME": str(root),
                "BRIDGE_RUST_BIN": str(binary),
                "BRIDGE_RUST_CONFIG": str(config),
                "FEISHU_APP_ID": "cli_offline",
                "FEISHU_APP_SECRET": "offline-secret-must-not-be-an-argument",
                "FEISHU_PAIRING_CODE": "a-very-long-offline-pairing-code",
            }
            result = subprocess.run(
                [str(REPO / "setup-rust.sh")],
                input="",
                text=True,
                capture_output=True,
                env=env,
                cwd=root,
                timeout=5,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn(env["FEISHU_APP_SECRET"], result.stdout + result.stderr)
            self.assertEqual(
                result.stdout.splitlines()[-5:],
                [
                    "service",
                    "--config",
                    str(config),
                    "start",
                    "若当前飞书用户尚未授权，请在飞书私聊机器人发送：/pair <刚才输入的配对码>",
                ],
            )

    def test_start_rejects_invalid_nonempty_pairing_code_before_service(self):
        with tempfile.TemporaryDirectory(prefix="bridge invalid pairing ") as directory:
            root = Path(directory)
            binary = root / "fake bridge"
            binary.write_text('#!/bin/bash\nprintf "%s\\n" "$@"\n', encoding="utf-8")
            binary.chmod(0o700)
            config = root / "bridge.toml"
            result = subprocess.run(
                [str(REPO / "start-rust.sh"), "start"],
                text=True,
                capture_output=True,
                env={
                    "PATH": "/usr/bin:/bin",
                    "BRIDGE_RUST_BIN": str(binary),
                    "BRIDGE_RUST_CONFIG": str(config),
                    "FEISHU_PAIRING_CODE": "too short",
                },
                cwd=root,
                timeout=5,
                check=False,
            )
            self.assertEqual(result.returncode, 1)
            self.assertIn("16–256 字节", result.stderr)
            self.assertEqual(result.stdout, "")

    def test_setup_does_not_reprompt_for_an_intentionally_blank_pairing_code(self):
        with tempfile.TemporaryDirectory(prefix="bridge setup terminal ") as directory:
            root = Path(directory)
            binary = root / "fake bridge"
            binary.write_text('#!/bin/bash\nprintf "%s\\n" "$@"\n', encoding="utf-8")
            binary.chmod(0o700)
            for name in ("cargo", "cc", "rustup", "codex"):
                command = root / name
                command.write_text("#!/bin/bash\nexit 0\n", encoding="utf-8")
                command.chmod(0o700)
            config = root / "bridge.toml"
            config.write_text("existing configuration", encoding="utf-8")
            master, slave = pty.openpty()
            env = {
                "PATH": f"{root}:/usr/bin:/bin",
                "HOME": str(root),
                "BRIDGE_RUST_BIN": str(binary),
                "BRIDGE_RUST_CONFIG": str(config),
            }
            process = subprocess.Popen(
                [str(REPO / "setup-rust.sh")],
                stdin=slave,
                stdout=slave,
                stderr=slave,
                env=env,
                cwd=root,
                start_new_session=True,
            )
            os.close(slave)
            try:
                os.write(master, b"cli_offline\noffline-secret\n\n")
                self.assertEqual(process.wait(timeout=5), 0)
                output = bytearray()
                while True:
                    try:
                        chunk = os.read(master, 4096)
                    except OSError:
                        break
                    if not chunk:
                        break
                    output.extend(chunk)
                self.assertIn(b"openssl rand -hex 16", output)
            finally:
                os.close(master)

    def test_setup_explains_noninteractive_toolchain_install(self):
        with tempfile.TemporaryDirectory(prefix="bridge no tools ") as directory:
            root = Path(directory)
            result = subprocess.run(
                ["/bin/bash", str(REPO / "setup-rust.sh")],
                input="",
                text=True,
                capture_output=True,
                env={"PATH": "/bin", "HOME": str(root)},
                cwd=root,
                timeout=5,
                check=False,
            )
            self.assertEqual(result.returncode, 1)
            self.assertIn("BRIDGE_RUST_AUTO_INSTALL=1", result.stderr)
