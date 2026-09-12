#!/usr/bin/env python3
"""Merge per-platform updater JSON files into latest.json.

Reads every latest-*.json in the target directory and merges their platform
entries into a single latest.json (Windows-only project: the Windows manifests
are the only inputs).
"""

from __future__ import annotations

import json
import sys
from pathlib import Path


def load_manifests(directory: Path) -> list[tuple[Path, dict]]:
    items: list[tuple[Path, dict]] = []
    for path in sorted(directory.glob("latest-*.json")):
        if path.name == "latest.json":
            continue
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if not isinstance(data, dict):
            continue
        items.append((path, data))
    return items


def merge_manifests(items: list[tuple[Path, dict]]) -> dict | None:
    if not items:
        return None
    merged: dict = {
        "version": "",
        "notes": "",
        "pub_date": "",
        "platforms": {},
    }
    for _, data in items:
        version = str(data.get("version") or "").strip()
        if version:
            merged["version"] = version
        notes = data.get("notes")
        if isinstance(notes, str) and notes:
            merged["notes"] = notes
        pub_date = str(data.get("pub_date") or "")
        if pub_date > str(merged.get("pub_date") or ""):
            merged["pub_date"] = pub_date
        platforms = data.get("platforms")
        if isinstance(platforms, dict):
            merged["platforms"].update(platforms)
    if not merged["version"] or not merged["platforms"]:
        return None
    return merged


def merge_directory(directory: Path) -> Path | None:
    items = load_manifests(directory)
    merged = merge_manifests(items)
    if merged is None:
        return None
    latest = directory / "latest.json"
    latest.write_text(json.dumps(merged, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    return latest


def _self_test() -> None:
    import tempfile

    with tempfile.TemporaryDirectory() as raw:
        root = Path(raw)
        (root / "latest-windows-x86_64.json").write_text(
            json.dumps(
                {
                    "version": "0.1.19",
                    "notes": "",
                    "pub_date": "2026-08-21T00:00:02Z",
                    "platforms": {
                        "windows-x86_64-nsis": {
                            "signature": "win",
                            "url": "https://example/win-setup.exe",
                        },
                        "windows-x86_64": {
                            "signature": "win",
                            "url": "https://example/win-setup.exe",
                        },
                    },
                }
            ),
            encoding="utf-8",
        )
        latest = merge_directory(root)
        assert latest is not None
        merged = json.loads(latest.read_text(encoding="utf-8"))
        assert merged["version"] == "0.1.19"
        assert set(merged["platforms"]) == {
            "windows-x86_64-nsis",
            "windows-x86_64",
        }
        print("merge-update-manifests: self-test ok")


def main() -> int:
    if len(sys.argv) == 2 and sys.argv[1] == "--self-test":
        _self_test()
        return 0
    directory = Path(sys.argv[1] if len(sys.argv) > 1 else ".")
    latest = merge_directory(directory)
    if latest is None:
        print("merge-update-manifests: 未找到可合并的 latest-*.json", file=sys.stderr)
        return 1
    print(f"merge-update-manifests: 已生成 {latest}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
