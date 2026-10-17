#!/usr/bin/env python3
"""Package the browser add-on (browser-addon/) three ways.

    python3 packaging/browser/build_addon.py <version>

dist/addon/chrome/   Chrome and Edge, loaded by hand ("developer mode").
                     Keeps the manifest's "key", so its ID is the same on
                     every computer (the Anywhere app trusts that ID).
dist/Anywhere-<version>-browser-addon.zip
                     The same without "key", for uploading to the Chrome
                     Web Store and Edge Add-ons (both refuse a "key").
dist/addon/safari/   Safari's variant, turned into a Safari extension inside
                     the Mac app by packaging/macos/build-safari-ext.sh.
"""

import json
import pathlib
import shutil
import sys
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
SRC = ROOT / "browser-addon"
OUT = ROOT / "dist" / "addon"


def copy_tree(dest: pathlib.Path) -> dict:
    if dest.exists():
        shutil.rmtree(dest)
    shutil.copytree(SRC, dest)
    return json.loads((SRC / "manifest.json").read_text())


def write_manifest(dest: pathlib.Path, manifest: dict) -> None:
    (dest / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


def main() -> None:
    version = sys.argv[1] if len(sys.argv) > 1 else None
    OUT.mkdir(parents=True, exist_ok=True)

    # Chrome / Edge, developer mode.
    chrome = OUT / "chrome"
    m = copy_tree(chrome)
    if version:
        m["version"] = version
    write_manifest(chrome, m)

    # Store upload: no "key".
    store = dict(m)
    store.pop("key", None)
    zip_name = f"Anywhere-{version or m['version']}-browser-addon.zip"
    zip_path = ROOT / "dist" / zip_name
    with zipfile.ZipFile(zip_path, "w", zipfile.ZIP_DEFLATED) as z:
        for f in sorted(chrome.rglob("*")):
            if f.is_file() and f.name != "manifest.json":
                z.write(f, f.relative_to(chrome).as_posix())
        z.writestr("manifest.json", json.dumps(store, indent=2) + "\n")

    # Safari: no "world": "MAIN" content scripts (relay.js adds page.js to
    # the page itself) and a non-persistent background page.
    safari = OUT / "safari"
    s = copy_tree(safari)
    if version:
        s["version"] = version
    s.pop("key", None)
    s["background"] = {"scripts": ["background.js"], "persistent": False}
    s["content_scripts"] = [c for c in s["content_scripts"] if c.get("world") != "MAIN"]
    write_manifest(safari, s)

    print(f"built {chrome}, {zip_path}, {safari}")


if __name__ == "__main__":
    main()
