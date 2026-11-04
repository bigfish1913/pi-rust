#!/usr/bin/env python3
"""Offline regressions for the daily README image refresh and support layout."""

from datetime import datetime, timezone
import importlib.util
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().with_name("refresh-readme-images.py")
spec = importlib.util.spec_from_file_location("refresh_readme_images", SCRIPT)
images = importlib.util.module_from_spec(spec)
spec.loader.exec_module(images)


class RefreshTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.readme = Path(temporary.name) / "README.md"
        url = f"{images.WECHAT_URL}?date=2026-10-31"
        self.original = (
            f'交流群\r\n<a href="{url}"><img src="{url}"></a>\r\n'
            'https://assets.bigfisharc.cn/images/support/wechat.png\r\n'
        ).encode("utf-8")
        self.readme.write_bytes(self.original)

    def test_refreshes_both_urls_and_preserves_other_bytes(self):
        self.assertTrue(images.refresh(self.readme, "2026-11-01"))
        self.assertEqual(
            self.readme.read_bytes(),
            self.original.replace(b"2026-10-31", b"2026-11-01"),
        )

    def test_same_day_is_noop(self):
        self.assertFalse(images.refresh(self.readme, "2026-10-31"))
        self.assertEqual(self.readme.read_bytes(), self.original)

    def test_missing_or_extra_urls_fail_without_writing(self):
        for content in (b"no dated image", self.original + self.original):
            with self.subTest(content=content):
                self.readme.write_bytes(content)
                with self.assertRaises(ValueError):
                    images.refresh(self.readme, "2026-11-01")
                self.assertEqual(self.readme.read_bytes(), content)

    def test_invalid_date_fails_without_writing(self):
        with self.assertRaises(ValueError):
            images.refresh(self.readme, "2026-02-30")
        self.assertEqual(self.readme.read_bytes(), self.original)

    def test_daily_boundary_is_utc_plus_eight(self):
        before = datetime(2026, 10, 31, 15, 59, tzinfo=timezone.utc)
        after = datetime(2026, 10, 31, 16, 0, tzinfo=timezone.utc)
        self.assertEqual(before.astimezone(images.CHINA_TIME).date().isoformat(), "2026-10-31")
        self.assertEqual(after.astimezone(images.CHINA_TIME).date().isoformat(), "2026-11-01")

    def test_repository_readme_layout_and_image_sources(self):
        text = images.README.read_text(encoding="utf-8")
        self.assertLess(text.index("## Support /"), text.index("## Community /"))
        self.assertLess(text.index("## Community /"), text.index("## Star history"))
        self.assertLess(text.index("## Star history"), text.index("## Contributors"))
        self.assertEqual(text.rsplit("\n## ", 1)[1].splitlines()[0], "Contributors")
        for name in ("alipay.jpg", "wechat.png", "binance.png"):
            self.assertEqual(text.count(f"https://assets.bigfisharc.cn/images/support/{name}"), 2)
        self.assertEqual(len(images.DATED_URL.findall(text)), 2)
        self.assertNotIn("{date}", text)
        self.assertIn("https://contrib.rocks/image?repo=bigfish1913/pi-rust", text)
        self.assertIn("https://github.com/bigfish1913/pi-rust/graphs/contributors", text)


if __name__ == "__main__":
    unittest.main()
