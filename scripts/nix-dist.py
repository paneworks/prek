#!/usr/bin/env python3
"""Export, verify and explicitly import Prek's architecture-specific Nix closures."""

import argparse
import base64
import gzip
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile
from urllib.parse import quote


SYSTEMS = {"x86_64": "x86_64-linux", "aarch64": "aarch64-linux"}
VERSION = r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z.-]+)?"
STORE = r"/nix/store/[0-9abcdfghijklmnpqrsvwxyz]{32}-[0-9A-Za-z+._?=-]+"
NAR = r"nar/[0-9A-Za-z+._-]+"
LIMIT = 4096
NIX = ["nix", "--extra-experimental-features", "nix-command",
       "--option", "substituters", "", "--option", "builders", "",
       "--option", "max-jobs", "0", "--option", "fallback", "false"]


def require(condition, message):
    if not condition:
        raise ValueError(message)


def command(argv, **kwargs):
    return subprocess.run([str(arg) for arg in argv], check=True, capture_output=True,
                          text=True, timeout=600, **kwargs).stdout


def native_system():
    require(platform.system() == "Linux" and platform.machine() in SYSTEMS,
            "expected native x86-64 or ARM64 Linux")
    return SYSTEMS[platform.machine()]


def closure(store_path, store=None):
    args = [*NIX, "path-info", "--recursive", "--json", store_path]
    if store is not None:
        args.extend(["--store", store])
    info = json.loads(command(args))
    if isinstance(info, list):
        info = {entry["path"]: entry for entry in info}
    return {path: {"nar_hash": entry["narHash"], "nar_size": entry["narSize"],
                   "references": sorted(entry["references"])}
            for path, entry in sorted(info.items())}


def validate_manifest(data, system=None):
    require(isinstance(data, dict) and data.get("schema") == 1
            and data.get("format") == "prek-nix-closure", "unsupported bundle manifest")
    version = data.get("version")
    require(isinstance(version, str) and re.fullmatch(VERSION, version), "invalid version")
    require(data.get("system") in SYSTEMS.values(), "invalid system")
    if system is not None:
        require(data["system"] == system, "bundle system differs from native/expected system")
    require(isinstance(data.get("revision"), str)
            and re.fullmatch(r"[0-9a-f]{40}", data["revision"]), "invalid revision")
    require(data.get("tag") in (None, f"v{version}"), "release tag differs from version")
    require(type(data.get("source_dirty")) is bool, "missing source worktree state")
    require(data["tag"] is None or not data["source_dirty"], "tagged exports require a clean worktree")
    root = data.get("store_path")
    require(isinstance(root, str) and re.fullmatch(STORE, root)
            and root.endswith(f"-prek-{version}"), "invalid application store path")
    entries = data.get("closure")
    require(isinstance(entries, dict) and 0 < len(entries) <= LIMIT
            and root in entries, "missing or oversized closure")
    for path, entry in entries.items():
        require(re.fullmatch(STORE, path) is not None and isinstance(entry, dict),
                "invalid closure entry")
        nar_hash = entry.get("nar_hash")
        require(isinstance(nar_hash, str) and nar_hash.startswith("sha256-"), "invalid NAR hash")
        require(len(base64.b64decode(nar_hash[7:], validate=True)) == 32, "invalid NAR hash")
        require(type(entry.get("nar_size")) is int and 0 < entry["nar_size"] <= 8 * 1024**3,
                "invalid NAR size")
        refs = entry.get("references")
        require(isinstance(refs, list) and all(isinstance(ref, str) for ref in refs),
                "invalid references")
        require(refs == sorted(set(refs)) and all(ref in entries for ref in refs),
                "closure contains missing or duplicate references")
    reachable, pending = set(), [root]
    while pending:
        path = pending.pop()
        if path not in reachable:
            reachable.add(path)
            pending.extend(entries[path]["references"])
    require(reachable == set(entries), "closure includes unreachable paths")
    return data


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def files(bundle):
    result = {}
    for path in sorted(bundle.rglob("*")):
        require(not path.is_symlink(), "bundle contains a symlink")
        require(path.is_file() or path.is_dir(), "bundle contains a special file")
        if path.is_file():
            name = path.relative_to(bundle).as_posix()
            require(re.fullmatch(r"[0-9A-Za-z/._+-]+", name) is not None,
                    "bundle contains an unsafe filename")
            result[name] = path
    return result


def verify(bundle, system=None, revision=None, tag=None):
    bundle = Path(bundle).resolve()
    inventory = files(bundle)
    require("SHA256SUMS" in inventory and inventory["SHA256SUMS"].stat().st_size < 1024**2,
            "missing or oversized checksums")
    expected = {}
    for line in inventory["SHA256SUMS"].read_text().splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  ([0-9A-Za-z/._+-]+)", line)
        require(match is not None, "invalid checksum entry")
        value, name = match.groups()
        require(name not in expected and name != "SHA256SUMS", "duplicate checksum entry")
        expected[name] = value
    require(set(expected) == set(inventory) - {"SHA256SUMS"}, "bundle file inventory differs")
    for name, value in expected.items():
        require(digest(inventory[name]) == value, f"checksum mismatch: {name}")
    require("manifest.json" in inventory and inventory["manifest.json"].stat().st_size < 4 * 1024**2,
            "missing or oversized manifest")
    data = validate_manifest(json.loads(inventory["manifest.json"].read_text()), system)
    require(revision is None or data["revision"] == revision, "bundle revision differs")
    require(tag is None or data["tag"] == tag, "bundle release tag differs")
    allowed = {"manifest.json", "install.py", "README.md", "cache/nix-cache-info", "SHA256SUMS"}
    for path in data["closure"]:
        name = f"cache/{Path(path).name[:32]}.narinfo"
        require(name in inventory and inventory[name].stat().st_size < 65536,
                "missing or oversized NAR info")
        fields = {}
        for line in inventory[name].read_text().splitlines():
            key, separator, value = line.partition(": ")
            require(separator and (key not in fields or key == "Sig"), "invalid NAR info")
            fields[key] = value
        url = fields.get("URL", "")
        require(re.fullmatch(NAR, url) is not None, "NAR URL must be a local cache file")
        require(fields.get("StorePath") == path, "NAR info path differs")
        allowed.update([name, f"cache/{url}"])
    require(set(inventory) == allowed, "unexpected or missing bundle files")
    cache = (bundle / "cache").as_uri()
    require(closure(data["store_path"], cache) == data["closure"], "cache closure differs from manifest")
    command([*NIX, "store", "verify", "--store", cache, "--recursive", "--no-trust", data["store_path"]])
    return data


def smoke(package, version=None):
    package = Path(package).resolve()
    binary = package / "bin/prek"
    with tempfile.TemporaryDirectory(prefix="prek-installed-") as directory:
        work = Path(directory)
        env = {"PATH": os.environ.get("PATH", ""), "HOME": directory,
               "XDG_CONFIG_HOME": str(work / "config"), "LANG": "C.UTF-8"}

        def run(*args):
            return command([binary, *args], cwd=work, env=env)

        reported = run("--version").strip()
        if version is not None:
            require(reported == f"prek {version}", "installed binary version differs")
        run("--help")
        docs = package / "share/doc/prek"
        require(json.loads(run("api", "--json")) == json.loads((docs / "lua-api.json").read_text()),
                "installed API JSON differs")
        require(run("api", "--lua") == (package / "share/prek/lua/prek.lua").read_text(),
                "installed Lua definitions differ")
        for preset in ("minimal", "desktop", "phone"):
            checked = json.loads(run("check", "--config", package / f"share/prek/presets/{preset}.lua", "--json"))
            require(checked.get("status") == "valid", "installed preset is invalid")
            run("init", "--preset", preset, "--output", work / f"{preset}.lua")
            require(json.loads(run("check", "--config", work / f"{preset}.lua", "--json"))["status"] == "valid",
                    "generated preset is invalid")
        entries = sorted((package / "share/prek/examples/lua").rglob("*.lua"))
        require(entries, "installed Lua examples missing")
        for entry in entries:
            if "modules" not in entry.relative_to(package / "share/prek/examples/lua").parts:
                require(json.loads(run("check", "--config", entry, "--json"))["status"] == "valid",
                        "installed Lua example is invalid")
        for name in ("nixos.md", "root-routing.md", "evdev-boundary.md", "distribution.md"):
            require((docs / name).is_file(), f"installed documentation missing: {name}")
        for name in ("share/prek/nixos/module.nix", "lib/udev/rules.d/70-prek.rules",
                     "share/doc/prek/LICENSE"):
            require((package / name).is_file(), f"installed resource missing: {name}")
        notices = docs / "dependency-notices"
        dependencies = json.loads((notices / "manifest.json").read_text())
        require({"luna", "luna-util"} <= {entry["name"] for entry in dependencies}, "Luna notices missing")
        for entry in dependencies:
            require(entry["texts"] and all((notices / path).is_file() for path in entry["texts"]),
                    "dependency notice text missing")
    return {"version": reported, "status": "passed", "physical_input_opened": False}


def export(package, output, version, revision, system, tag=None, dirty=False, replace=False):
    package, output = Path(package).resolve(), Path(output).absolute()
    require(system == native_system(), "export must run on the native architecture")
    data = validate_manifest({"schema": 1, "format": "prek-nix-closure", "version": version,
                              "revision": revision, "tag": tag, "system": system, "source_dirty": dirty,
                              "store_path": str(package), "closure": closure(str(package))}, system)
    require(replace or (not output.exists() and not output.with_suffix(".sha256").exists()),
            "output already exists (use --replace to rebuild)")
    smoke(package, version)
    output.parent.mkdir(parents=True, exist_ok=True)
    name = f"prek-{version}-{system}-nix"
    with tempfile.TemporaryDirectory(prefix=".prek-dist-", dir=output.parent) as directory:
        bundle = Path(directory) / name
        bundle.mkdir()
        command([*NIX, "copy", "--to", (bundle / "cache").as_uri(), str(package)])
        (bundle / "manifest.json").write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")
        shutil.copyfile(__file__, bundle / "install.py")
        shutil.copyfile(package / "share/doc/prek/distribution.md", bundle / "README.md")
        (bundle / "SHA256SUMS").write_text("".join(f"{digest(path)}  {name}\n"
                                                   for name, path in files(bundle).items()))
        verify(bundle, system)
        archive = Path(directory) / "bundle.tar.gz"
        with archive.open("wb") as stream, gzip.GzipFile(fileobj=stream, mode="wb", mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as tar:
                for path in [bundle, *sorted(bundle.rglob("*"))]:
                    info = tar.gettarinfo(str(path), arcname=str(path.relative_to(bundle.parent)))
                    info.uid = info.gid = info.mtime = 0
                    info.uname = info.gname = ""
                    info.mode = 0o755 if path.is_dir() else 0o644
                    if path.is_file():
                        with path.open("rb") as source:
                            tar.addfile(info, source)
                    else:
                        tar.addfile(info)
        if replace:
            os.replace(archive, output)
        else:
            with output.open("xb") as destination, archive.open("rb") as source:
                shutil.copyfileobj(source, destination)
    output.with_suffix(".sha256").write_text(f"{digest(output)}  {output.name}\n")
    return data


def import_bundle(bundle, trust_unsigned, store_root=None, require_missing=False):
    require(trust_unsigned, "import requires --trust-unsigned after verifying the release's origin/checksum")
    data = verify(bundle, native_system())
    root = Path(store_root).resolve() if store_root is not None else None
    destination = f"local?root={quote(str(root), safe='/')}" if root else None
    installed = (root / data["store_path"].lstrip("/")) if root else Path(data["store_path"])
    require(not require_missing or not installed.exists(), "application already exists; not a fresh import")
    args = [*NIX, "copy", "--from", (Path(bundle).resolve() / "cache").as_uri(), "--no-check-sigs"]
    if root:
        require(root != Path("/"), "isolated store root must not be /")
        args.extend(["--to", destination])
    command([*args, data["store_path"]])
    require(closure(data["store_path"], destination) == data["closure"],
            "imported closure differs")
    command([*NIX, "store", "verify", "--recursive", "--no-trust",
             *(["--store", destination] if root else []), data["store_path"]])
    require(installed.is_dir(), "application did not arrive in destination")
    return {"status": "passed", "store_path": data["store_path"], "isolated_store": str(root) if root else None,
            "execution": "not attempted" if root else smoke(installed, data["version"])}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    create = sub.add_parser("export")
    create.add_argument("--package", required=True)
    create.add_argument("--output", required=True)
    create.add_argument("--system", required=True, choices=SYSTEMS.values())
    create.add_argument("--version", required=True)
    create.add_argument("--revision", required=True)
    create.add_argument("--tag")
    create.add_argument("--dirty", action="store_true")
    create.add_argument("--replace", action="store_true")
    check = sub.add_parser("verify")
    check.add_argument("--bundle", default=".")
    check.add_argument("--system", choices=SYSTEMS.values())
    check.add_argument("--revision")
    check.add_argument("--tag")
    install = sub.add_parser("import")
    install.add_argument("--bundle", default=".")
    install.add_argument("--trust-unsigned", action="store_true")
    install.add_argument("--store-root")
    install.add_argument("--require-missing", action="store_true")
    probe = sub.add_parser("smoke")
    probe.add_argument("--package", required=True)
    probe.add_argument("--version")
    args = vars(parser.parse_args())
    action = args.pop("action")
    try:
        operation = {"export": export, "verify": verify, "import": import_bundle, "smoke": smoke}[action]
        print(json.dumps(operation(**args), indent=2, sort_keys=True))
    except (ValueError, KeyError, OSError, subprocess.SubprocessError) as error:
        parser.exit(1, f"{error}\n{getattr(error, 'stderr', '') or ''}")


if __name__ == "__main__":
    main()
