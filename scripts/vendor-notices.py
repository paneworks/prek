#!/usr/bin/env python3
"""Install declared crate licensing metadata and vendored license/notice texts."""

import argparse
import json
from pathlib import Path
import shutil
import tomllib


def metadata(path):
    manifest = tomllib.loads(path.read_text())
    package = manifest["package"]
    shared = manifest.get("workspace", {}).get("package", {})
    return {key: shared[key] if isinstance(value, dict) and value.get("workspace") else value
            for key, value in package.items()}


def collect(vendor, output):
    vendor, output = Path(vendor), Path(output)
    output.mkdir(parents=True, exist_ok=False)
    entries = []
    for crate in sorted(vendor.iterdir()):
        if not (crate / "Cargo.toml").is_file():
            continue
        package = metadata(crate / "Cargo.toml")
        texts = {path.relative_to(crate): path for path in crate.rglob("*")
                 if path.is_file() and path.name.upper().startswith(
                     ("LICENSE", "LICENCE", "UNLICENSE", "COPYING", "COPYRIGHT", "NOTICE", "AUTHORS"))}
        origin = "vendored texts"
        declared = package.get("license-file")
        if declared:
            path = (crate / declared).resolve()
            if not path.is_relative_to(crate.resolve()) or not path.is_file():
                raise ValueError(f"invalid license-file: {crate.name}")
            texts[Path(declared)] = path
        shared = {"luna-util": ("luna", "LICENSE"),
                  "ottavino-gc-arena-derive": ("ottavino-gc-arena", "LICENSE-MIT")}
        if not texts and package["name"] in shared:
            sibling, filename = shared[package["name"]]
            source = vendor / f"{sibling}-{package['version']}"
            owner = metadata(source / "Cargo.toml")
            if (owner.get("repository"), owner.get("license")) != (package.get("repository"), package.get("license")):
                raise ValueError("shared workspace licensing declarations differ")
            texts[Path(filename)] = source / filename
            origin = f"shared workspace: {source.name}/{filename}"
        if not texts and (package["name"], package["version"], package.get("license")) == (
                "sptr", "0.3.2", "MIT OR Apache-2.0"):
            standard = vendor / "serde-1.0.229/LICENSE-APACHE"
            if "Apache License" not in standard.read_text() or "Version 2.0" not in standard.read_text():
                raise ValueError("standard Apache-2.0 license text missing")
            texts[Path("LICENSE-APACHE")] = standard
            texts.update({path.relative_to(crate): path for path in crate.rglob("*") if path.is_file()})
            origin = "Apache-2.0 option: standard terms from serde-1.0.229; complete sptr source preserved"
        if not texts or not (package.get("license") or declared):
            raise ValueError(f"missing license declaration/text: {crate.name}")
        installed = []
        for relative, source in sorted(texts.items()):
            destination = output / crate.name / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, destination)
            installed.append(destination.relative_to(output).as_posix())
        entries.append({"name": package["name"], "version": package["version"],
                        "license": package.get("license"), "license_file": declared,
                        "license_origin": origin,
                        "repository": package.get("repository"), "authors": package.get("authors", []),
                        "texts": installed})
    if not entries:
        raise ValueError("empty vendor directory")
    (output / "manifest.json").write_text(json.dumps(entries, indent=2, sort_keys=True) + "\n")
    return entries


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--vendor", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    collect(args.vendor, args.output)
