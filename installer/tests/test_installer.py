import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import jarvis_install as ji


class Tests(unittest.TestCase):
    def test_parse_compatible(self):
        self.assertEqual(ji.parse_compatible("yandex,station-mini2\x00amlogic,axg"), "station-mini2")
        self.assertEqual(ji.parse_compatible("amlogic,axg"), "")
        self.assertEqual(ji.parse_compatible("yandex,station-mini2"), "station-mini2")

    def test_previous_release_semver(self):
        self.assertEqual(ji.previous_release(["0.9.0", "0.10.0"], "0.10.1"), "0.10.0")
        self.assertEqual(ji.previous_release(["0.9.0", "0.10.0"], "0.10.0"), "0.9.0")
        self.assertIsNone(ji.previous_release(["1.0.0"], "1.0.0"))

    def test_shim_keeps_original_and_marker(self):
        text = ji.shim_for("S97quasar")
        self.assertIn(ji.MARKER, text)
        self.assertIn("/data/jarvis/backups/quasar-init/S97quasar.orig", text)
        self.assertIn("/data/jarvis/enabled", text)

    def test_version_validation(self):
        self.assertEqual(ji.validate_version("0.1.0"), "0.1.0")
        for bad in ["../x", "a b", "", "x" * 41]:
            with self.assertRaises(ji.InstallError):
                ji.validate_version(bad)

    def test_supported_models(self):
        self.assertIn("station-mini2", ji.SUPPORTED)


if __name__ == "__main__":
    unittest.main()
