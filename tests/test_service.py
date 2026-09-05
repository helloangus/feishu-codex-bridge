import tempfile
import unittest
from pathlib import Path

import service


class ServiceTests(unittest.TestCase):
    def test_health_state_round_trip(self):
        with tempfile.TemporaryDirectory() as directory:
            original = service.HEALTH
            try:
                service.HEALTH = Path(directory) / "health.json"
                service.write_health("connected", pid=42)
                state = service.read_health()
            finally:
                service.HEALTH = original
        self.assertEqual(state["phase"], "connected")
        self.assertEqual(state["pid"], 42)
        self.assertTrue(service.age_text(state["updated_at"]).endswith("秒前"))


if __name__ == "__main__":
    unittest.main()
