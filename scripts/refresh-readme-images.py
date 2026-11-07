#!/usr/bin/env python3
"""Refresh the README WeChat image cache key once per UTC+8 calendar day.

This changes the image URL, not the invite code or the asset server's cache TTL.
Run from any directory: python scripts/refresh-readme-images.py
"""

from datetime import datetime, timedelta, timezone
from pathlib import Path
import re


README = Path(__file__).resolve().parent.parent / "README.md"
WECHAT_URL = "https://assets.bigfisharc.cn/images/support/weixin.png"
DATED_URL = re.compile(re.escape(WECHAT_URL) + r"\?date=\d{4}-\d{2}-\d{2}")
CHINA_TIME = timezone(timedelta(hours=8))


def refresh(readme: Path, day: str) -> bool:
    """Update both the original-image link and its thumbnail, preserving newlines."""
    datetime.strptime(day, "%Y-%m-%d")
    original = readme.read_bytes()
    text = original.decode("utf-8")
    updated, count = DATED_URL.subn(f"{WECHAT_URL}?date={day}", text)
    if count != 2:
        raise ValueError(f"Expected 2 dated WeChat URLs in {readme}, found {count}")
    result = updated.encode("utf-8")
    if result == original:
        return False
    readme.write_bytes(result)
    return True


def main() -> None:
    day = datetime.now(CHINA_TIME).date().isoformat()
    changed = refresh(README, day)
    print(f"README WeChat image date: {day} ({'updated' if changed else 'unchanged'})")


if __name__ == "__main__":
    main()
