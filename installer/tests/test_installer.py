import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import petrovich_install as ji


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
        self.assertIn("/data/petrovich/backups/quasar-init/S97quasar.orig", text)
        self.assertIn("/data/petrovich/enabled", text)

    def test_version_validation(self):
        self.assertEqual(ji.validate_version("0.1.0"), "0.1.0")
        for bad in ["../x", "a b", "", "x" * 41]:
            with self.assertRaises(ji.InstallError):
                ji.validate_version(bad)

    def test_supported_models(self):
        self.assertIn("station-mini2", ji.SUPPORTED)

    def test_default_model_is_single_tflite(self):
        model = ji.ROOT / "assets/wakeword/petrovich.tflite"
        uploads = ji.model_uploads(model)
        self.assertEqual(
            uploads,
            [(model, "/data/petrovich/models/petrovich.tflite")],
        )

    def test_byedpi_assets_cover_device_arches(self):
        self.assertEqual(ji.byedpi_asset("aarch64")[1], "ciadpi-aarch64")
        self.assertEqual(ji.byedpi_asset("armv7l")[1], "ciadpi-armv7l")
        self.assertEqual(ji.byedpi_asset("armv8l")[1], "ciadpi-armv7l")
        self.assertEqual(ji.byedpi_asset("armv6l")[1], "ciadpi-armv6")
        with self.assertRaises(ji.InstallError):
            ji.byedpi_asset("x86_64")

    def test_byedpi_init_script_has_pidfile_and_proxy_port(self):
        text = ji.byedpi_init_script(1080, "--disorder 1")
        self.assertIn(ji.MARKER, text)
        self.assertIn("PORT=1080", text)
        self.assertIn("-i 127.0.0.1", text)
        self.assertIn("/run/ciadpi.pid", text)
        self.assertIn("ARGS='--disorder 1'", text)

    def test_set_wakeword_model_only_in_section(self):
        text = (
            '[stt]\nmodel = "whisper"\n\n'
            '[wakeword]\nmodel = "/data/skynet/models/wakeword.json"\n'
            'assets = "/data/petrovich/assets/oww.bin"\n'
        )
        updated = ji._set_wakeword_model(
            text, "/data/petrovich/models/petrovich.tflite"
        )
        self.assertIn('[stt]\nmodel = "whisper"', updated)
        self.assertIn('model = "/data/petrovich/models/petrovich.tflite"', updated)
        self.assertIn('assets = "/data/petrovich/assets/oww.bin"', updated)
        self.assertNotIn("wakeword.json", updated)


if __name__ == "__main__":
    unittest.main()
