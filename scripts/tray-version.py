#!/usr/bin/env python3
"""Give the tray the workspace's version: tray/Cargo.toml, tray/tauri.conf.json, and the entries of
cairn-tray and cairn-protocol in tray/Cargo.lock, which `cargo build --locked` in tray/ checks, since the
tray is a workspace of its own that no build here updates. Run by release.sh, from the repo or with it as
the argument. `--test` tries it on a copy, for ci.sh. Python 3.9, as on a Mac."""

import json
import pathlib
import re
import shutil
import sys
import tempfile

FILES = ["Cargo.toml", "tray/Cargo.toml", "tray/tauri.conf.json", "tray/Cargo.lock"]
LOCKED = ("cairn-tray", "cairn-protocol")


def section(text, header):
    """The lines of one TOML table, from its header to the next."""
    m = re.search(r"(?m)^" + re.escape(header) + r"[ \t]*\n((?:(?!\[).*\n?)*)", text)
    if not m:
        raise SystemExit(f"no {header} table")
    return m


def toml_version(text, header):
    m = re.search(r'(?m)^version = "([^"]*)"', section(text, header).group(1))
    if not m:
        raise SystemExit(f"no version in {header}")
    return m.group(1)


def workspace_version(root):
    return toml_version((root / "Cargo.toml").read_text(), "[workspace.package]")


def locked_versions(lock):
    """Each package's version in a Cargo.lock, by name."""
    found = {}
    for m in re.finditer(r'(?m)^\[\[package\]\]\nname = "([^"]*)"\nversion = "([^"]*)"', lock):
        found.setdefault(m.group(1), []).append(m.group(2))
    return found


def read_tray(root):
    """The tray's versions as each file has it."""
    tray = root / "tray"
    out = {"tray/Cargo.toml": toml_version((tray / "Cargo.toml").read_text(), "[package]")}
    out["tray/tauri.conf.json"] = json.loads((tray / "tauri.conf.json").read_text())["version"]
    locked = locked_versions((tray / "Cargo.lock").read_text())
    for name in LOCKED:
        versions = locked.get(name, [])
        if len(versions) != 1:
            raise SystemExit(f"tray/Cargo.lock has {len(versions)} entries for {name}, not one")
        out[f"tray/Cargo.lock {name}"] = versions[0]
    return out


def bump(root):
    version = workspace_version(root)
    tray = root / "tray"

    path = tray / "Cargo.toml"
    text = path.read_text()
    m = section(text, "[package]")
    body = re.sub(r'(?m)^version = "[^"]*"', f'version = "{version}"', m.group(1), count=1)
    path.write_text(text[: m.start(1)] + body + text[m.end(1) :])

    # The first "version" key at the top level: two spaces in, as the file is laid out.
    path = tray / "tauri.conf.json"
    text = path.read_text()
    path.write_text(
        re.sub(r'(?m)^  "version": "[^"]*"', f'  "version": "{version}"', text, count=1)
    )

    path = tray / "Cargo.lock"
    text = path.read_text()
    for name in LOCKED:
        text = re.sub(
            r'(?m)^(\[\[package\]\]\nname = "' + re.escape(name) + r'"\nversion = ")[^"]*"',
            lambda m: m.group(1) + version + '"',
            text,
        )
    path.write_text(text)

    wrong = {k: v for k, v in read_tray(root).items() if v != version}
    if wrong:
        raise SystemExit(f"the tray is not at {version} after bumping it: {wrong}")
    return version


def test():
    """On a copy: from the workspace's version to another, every place changes and nothing else does."""
    here = pathlib.Path(__file__).resolve().parent.parent
    with tempfile.TemporaryDirectory() as scratch:
        root = pathlib.Path(scratch)
        for f in FILES:
            (root / f).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(here / f, root / f)
        before = {f: (root / f).read_text() for f in FILES}
        old = workspace_version(root)
        new = "98.76.54321"
        cargo = root / "Cargo.toml"
        cargo.write_text(cargo.read_text().replace(f'version = "{old}"', f'version = "{new}"', 1))
        assert workspace_version(root) == new
        assert bump(root) == new
        assert set(read_tray(root).values()) == {new}, read_tray(root)
        # Only the version lines moved, whatever the tray was at before: after a bump by hand and
        # before release.sh, as ci.sh sees it, the tray is behind the workspace.
        for f, lines in [("tray/Cargo.toml", 1), ("tray/tauri.conf.json", 1), ("tray/Cargo.lock", 2)]:
            was, now = before[f].splitlines(), (root / f).read_text().splitlines()
            assert len(was) == len(now), f
            changed = [b for a, b in zip(was, now) if a != b]
            assert len(changed) == lines and all(new in b for b in changed), (f, changed)
        # Twice is the same as once.
        snapshot = {f: (root / f).read_text() for f in FILES}
        bump(root)
        assert snapshot == {f: (root / f).read_text() for f in FILES}
    print("tray-version: ok")


if __name__ == "__main__":
    if sys.argv[1:] == ["--test"]:
        test()
    else:
        root = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else pathlib.Path(__file__).resolve().parent.parent
        print(f"the tray is at {bump(root)}")
