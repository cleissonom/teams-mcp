import contextlib
import importlib.util
import io
import json
from pathlib import Path
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "scripts/smoke-clients.py"
SPEC = importlib.util.spec_from_file_location("smoke_clients", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class SmokeClientReportingTests(unittest.TestCase):
    def test_authentication_errors_print_only_the_error_code(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            MODULE.report("codex", "browser", "teams_status", {
                "success": False,
                "error": {
                    "code": "AUTH_REQUIRED",
                    "message": "synthetic private diagnostic",
                    "recovery": {"account": "person@example.test"},
                },
            })
        result = json.loads(output.getvalue())
        self.assertEqual(result["error"], {"code": "AUTH_REQUIRED"})
        self.assertNotIn("private diagnostic", output.getvalue())
        self.assertNotIn("person@example.test", output.getvalue())


if __name__ == "__main__":
    unittest.main()
