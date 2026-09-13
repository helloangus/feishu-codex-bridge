"""Validate the Rust-tested wire fixtures against this installed CLI's schema."""
import hashlib
import json
from pathlib import Path
import unittest

try:
    from jsonschema import Draft7Validator
except ImportError:
    Draft7Validator = None

ROOT = Path(__file__).resolve().parents[1]
SCHEMA = ROOT / "schemas/codex/0.153.4"


class RustProtocolTests(unittest.TestCase):
    def test_generated_schema_matches_provenance_manifest(self):
        manifest = json.loads((SCHEMA / "manifest.json").read_text())
        self.assertEqual(manifest["cli_version"], "0.153.4")
        for name, digest in manifest["files"].items():
            self.assertEqual(hashlib.sha256((SCHEMA / name).read_bytes()).hexdigest(), digest)

    @unittest.skipIf(Draft7Validator is None, "install requirements-test.txt for schema validation")
    def test_rust_turn_fixtures_match_versioned_app_server_schema(self):
        validator = Draft7Validator(json.loads((SCHEMA / "TurnStartParams.json").read_text()))
        for name in ("turn-start-plan.json", "turn-start-default.json"):
            with self.subTest(name=name):
                payload = json.loads((ROOT / "fixtures/codex/0.153.4" / name).read_text())
                validator.validate(payload)
                self.assertEqual(payload["approvalPolicy"], "on-request")
                self.assertFalse(payload["sandboxPolicy"]["networkAccess"])
                self.assertEqual(payload["sandboxPolicy"]["writableRoots"], [payload["cwd"]])
                payload["collaborationMode"]["settings"].pop("model")
                self.assertFalse(validator.is_valid(payload))

    @unittest.skipIf(Draft7Validator is None, "install requirements-test.txt for schema validation")
    def test_request_and_reply_fixtures_match_installed_schema(self):
        cases = json.loads((ROOT / "fixtures/codex/0.153.4/server-requests.json").read_text())
        for case in cases:
            with self.subTest(method=case["method"]):
                for suffix, field in (("Params", "params"), ("Response", "reply")):
                    schema = json.loads((SCHEMA / (case["schema"] + suffix + ".json")).read_text())
                    Draft7Validator(schema).validate(case[field])
