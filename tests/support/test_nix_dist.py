import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location("nix_dist", Path(__file__).parents[2] / "scripts/nix-dist.py")
dist = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dist)
ROOT = "/nix/store/" + "0" * 32 + "-prek-0.1.0"
DEP = "/nix/store/" + "1" * 32 + "-library"
HASH = "sha256-" + "A" * 43 + "="

notice_spec = importlib.util.spec_from_file_location("vendor_notices", Path(__file__).parents[2] / "scripts/vendor-notices.py")
notices = importlib.util.module_from_spec(notice_spec)
notice_spec.loader.exec_module(notices)


def manifest():
    return {"schema": 1, "format": "prek-nix-closure", "version": "0.1.0",
            "system": "x86_64-linux", "revision": "a" * 40, "tag": "v0.1.0", "source_dirty": False,
            "store_path": ROOT,
            "closure": {ROOT: {"nar_hash": HASH, "nar_size": 1, "references": [DEP]},
                        DEP: {"nar_hash": HASH, "nar_size": 1, "references": []}}}


def sums(bundle):
    entries = {name: path for name, path in dist.files(bundle).items() if name != "SHA256SUMS"}
    (bundle / "SHA256SUMS").write_text("".join(f"{dist.digest(path)}  {name}\n"
                                              for name, path in entries.items()))


class Manifests(unittest.TestCase):
    def test_exact_closed_reachable_native_manifest(self):
        self.assertEqual(dist.validate_manifest(manifest(), "x86_64-linux"), manifest())

    def test_wrong_system_version_revision_path_and_tag(self):
        changes = [("schema", 2), ("format", "portable"), ("version", "0.1.0; touch bad"),
                   ("version", "01.1.0"), ("revision", "a" * 39), ("revision", "a" * 40 + "\n"),
                   ("system", "riscv64-linux"), ("tag", "main"), ("tag", "v0.2.0"),
                   ("store_path", ROOT + "/bin/prek"), ("store_path", "/tmp/prek-0.1.0")]
        for key, value in changes:
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                dist.validate_manifest({**manifest(), key: value})
        with self.assertRaisesRegex(ValueError, "system differs"):
            dist.validate_manifest(manifest(), "aarch64-linux")

    def test_missing_extra_duplicate_and_unreachable_dependencies(self):
        for refs in (["/nix/store/missing"], [DEP, DEP]):
            data = manifest()
            data["closure"][ROOT]["references"] = refs
            with self.subTest(refs=refs), self.assertRaises(ValueError):
                dist.validate_manifest(data)
        data = manifest()
        data["closure"][ROOT]["references"] = []
        with self.assertRaisesRegex(ValueError, "unreachable"):
            dist.validate_manifest(data)
        for entries in ({}, {DEP: manifest()["closure"][DEP]}):
            with self.assertRaises(ValueError):
                dist.validate_manifest({**manifest(), "closure": entries})

    def test_self_references_are_valid(self):
        data = manifest()
        data["closure"][DEP]["references"] = [DEP]
        dist.validate_manifest(data)

    def test_dirty_snapshots_cannot_claim_a_release_tag(self):
        data = {**manifest(), "source_dirty": True}
        with self.assertRaisesRegex(ValueError, "clean worktree"):
            dist.validate_manifest(data)
        data["tag"] = None
        dist.validate_manifest(data)

    def test_hash_sizes_and_reference_types_are_checked(self):
        for key, value in (("nar_hash", "sha256-bad"), ("nar_size", 0), ("nar_size", True),
                           ("nar_size", 9 * 1024**3), ("references", None), ("references", [1])):
            data = manifest()
            data["closure"][ROOT][key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                dist.validate_manifest(data)


class Bundles(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="prek-dist-model-")
        self.addCleanup(self.temp.cleanup)
        self.bundle = Path(self.temp.name)
        self.data = manifest()
        for name in ("install.py", "README.md", "cache/nix-cache-info"):
            path = self.bundle / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("fixture\n")
        for index, path in enumerate(self.data["closure"]):
            info = self.bundle / f"cache/{Path(path).name[:32]}.narinfo"
            info.write_text(f"StorePath: {path}\nURL: nar/{index}.nar.xz\n")
            nar = self.bundle / f"cache/nar/{index}.nar.xz"
            nar.parent.mkdir(exist_ok=True)
            nar.write_bytes(b"fixture")
        self.write_manifest()
        self.query = patch.object(dist, "closure", return_value=copy.deepcopy(self.data["closure"])).start()
        self.run = patch.object(dist, "command", return_value="").start()
        self.addCleanup(patch.stopall)

    def write_manifest(self):
        (self.bundle / "manifest.json").write_text(json.dumps(self.data))
        sums(self.bundle)

    def test_verified_cache_uses_no_builders_substitutes_or_trust_claim(self):
        self.assertEqual(dist.verify(self.bundle, "x86_64-linux"), self.data)
        args = self.run.call_args.args[0]
        self.assertIn("--no-trust", args)
        for option, value in (("builders", ""), ("substituters", ""), ("max-jobs", "0"), ("fallback", "false")):
            self.assertIn(["--option", option, value], [args[i:i + 3] for i in range(len(args))])
        self.assertIn((self.bundle / "cache").as_uri(), args)

    def test_mutation_is_refused_before_any_nix_command(self):
        (self.bundle / "cache/nar/0.nar.xz").write_bytes(b"changed")
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            dist.verify(self.bundle)
        self.query.assert_not_called()
        self.run.assert_not_called()

    def test_missing_and_unlisted_files_are_refused(self):
        (self.bundle / "README.md").unlink()
        with self.assertRaisesRegex(ValueError, "inventory"):
            dist.verify(self.bundle)
        (self.bundle / "README.md").write_text("fixture\n")
        (self.bundle / "extra").write_text("unexpected")
        with self.assertRaisesRegex(ValueError, "inventory"):
            dist.verify(self.bundle)

    def test_rehashed_extra_cache_path_is_still_refused(self):
        (self.bundle / "cache/extra.narinfo").write_text("not in closure")
        sums(self.bundle)
        with self.assertRaisesRegex(ValueError, "unexpected"):
            dist.verify(self.bundle)

    def test_symlinks_and_special_files_are_refused(self):
        (self.bundle / "link").symlink_to(self.bundle / "manifest.json")
        with self.assertRaisesRegex(ValueError, "symlink"):
            dist.verify(self.bundle)
        (self.bundle / "link").unlink()
        import os
        os.mkfifo(self.bundle / "fifo")
        with self.assertRaisesRegex(ValueError, "special"):
            dist.verify(self.bundle)

    def test_duplicate_and_traversal_checksum_entries_are_refused(self):
        checksums = self.bundle / "SHA256SUMS"
        original = checksums.read_text()
        for suffix in (original.splitlines()[0] + "\n", "0" * 64 + "  ../outside\n"):
            checksums.write_text(original + suffix)
            with self.assertRaises(ValueError):
                dist.verify(self.bundle)

    def test_remote_absolute_and_traversal_nar_urls_are_refused_before_nix(self):
        info = self.bundle / f"cache/{Path(ROOT).name[:32]}.narinfo"
        for url in ("https://example.invalid/a", "/tmp/nar", "../nar", "nar/../../outside", "nar/a?query"):
            info.write_text(f"StorePath: {ROOT}\nURL: {url}\n")
            sums(self.bundle)
            with self.subTest(url=url), self.assertRaisesRegex(ValueError, "local cache file"):
                dist.verify(self.bundle)
        self.query.assert_not_called()

    def test_missing_nar_and_foreign_narinfo_are_refused(self):
        nar = self.bundle / "cache/nar/0.nar.xz"
        nar.unlink()
        sums(self.bundle)
        with self.assertRaises(ValueError):
            dist.verify(self.bundle)
        nar.write_bytes(b"fixture")
        info = self.bundle / f"cache/{Path(ROOT).name[:32]}.narinfo"
        info.write_text(f"StorePath: {DEP}\nURL: nar/0.nar.xz\n")
        sums(self.bundle)
        with self.assertRaisesRegex(ValueError, "path differs"):
            dist.verify(self.bundle)

    def test_lying_manifest_or_corrupt_nar_cannot_pass_nix_verification(self):
        self.query.return_value = {}
        with self.assertRaisesRegex(ValueError, "cache closure differs"):
            dist.verify(self.bundle)
        self.query.return_value = self.data["closure"]
        self.run.side_effect = dist.subprocess.CalledProcessError(1, "nix", stderr="corrupt NAR")
        with self.assertRaises(dist.subprocess.CalledProcessError):
            dist.verify(self.bundle)

    def test_expected_revision_and_tag_are_independent_checks(self):
        for kwargs in ({"revision": "b" * 40}, {"tag": "v0.2.0"}):
            with self.assertRaisesRegex(ValueError, "differs"):
                dist.verify(self.bundle, **kwargs)

    def test_unsigned_import_requires_explicit_consent_before_verification(self):
        with self.assertRaisesRegex(ValueError, "trust-unsigned"):
            dist.import_bundle(self.bundle, False)
        self.query.assert_not_called()
        self.run.assert_not_called()

    def test_cross_architecture_import_is_refused(self):
        with patch.object(dist, "native_system", return_value="aarch64-linux"):
            with self.assertRaisesRegex(ValueError, "system differs"):
                dist.import_bundle(self.bundle, True)
        self.query.assert_not_called()

    def test_already_present_application_fails_freshness_gate(self):
        root = self.bundle / "destination"
        (root / ROOT.lstrip("/")).mkdir(parents=True)
        with patch.object(dist, "verify", return_value=self.data), patch.object(dist, "native_system"):
            with self.assertRaisesRegex(ValueError, "already exists"):
                dist.import_bundle(self.bundle, True, root, True)
        self.run.assert_not_called()


class Notices(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="prek-notices-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.vendor = self.root / "vendor"
        self.vendor.mkdir()

    def crate(self, name, text=None, extra=""):
        root = self.vendor / f"{name}-0.5.1"
        root.mkdir()
        (root / "Cargo.toml").write_text(f'[package]\nname="{name}"\nversion="0.5.1"\nlicense="MIT"\n'
                                         f'repository="https://github.com/onix-os/luna"\n{extra}')
        if text is not None:
            (root / "LICENSE").write_text(text)
        return root

    def test_luna_util_keeps_the_exact_shared_copyright(self):
        self.crate("luna", "Luna and Piccolo copyright fixture")
        self.crate("luna-util")
        output = self.root / "notices"
        result = notices.collect(self.vendor, output)
        self.assertEqual({entry["name"] for entry in result}, {"luna", "luna-util"})
        self.assertEqual((output / "luna-util-0.5.1/LICENSE").read_text(), "Luna and Piccolo copyright fixture")

    def test_unknown_missing_text_cannot_silently_ship(self):
        self.crate("other")
        with self.assertRaisesRegex(ValueError, "missing license"):
            notices.collect(self.vendor, self.root / "notices")

    def test_git_workspace_inherited_declarations_are_resolved(self):
        root = self.crate("luna", "shared copyright")
        (root / "Cargo.toml").write_text('[workspace.package]\nversion="0.5.1"\nlicense="MIT"\n'
                                        'repository="https://github.com/onix-os/luna"\n'
                                        '[package]\nname="luna"\nversion.workspace=true\n'
                                        'license.workspace=true\nrepository.workspace=true\n')
        self.crate("luna-util")
        result = notices.collect(self.vendor, self.root / "notices")
        self.assertEqual(result[0]["version"], "0.5.1")
        self.assertEqual(result[0]["license"], "MIT")

    def test_license_file_must_stay_inside_its_crate(self):
        self.crate("other", "fixture", 'license-file="../outside"\n')
        (self.vendor / "outside").write_text("not this crate")
        with self.assertRaisesRegex(ValueError, "invalid license-file"):
            notices.collect(self.vendor, self.root / "notices")

    def test_nested_notices_and_alternate_license_files_are_preserved(self):
        root = self.crate("other", "fixture", 'license-file="terms.txt"\n')
        (root / "terms.txt").write_text("declared license")
        (root / "subdir").mkdir()
        (root / "subdir/NOTICE").write_text("notice")
        output = self.root / "notices"
        result = notices.collect(self.vendor, output)
        self.assertEqual(len(result[0]["texts"]), 3)
        self.assertEqual((output / "other-0.5.1/subdir/NOTICE").read_text(), "notice")

    def test_exact_sptr_exception_keeps_standard_terms_and_original_source(self):
        sptr = self.vendor / "sptr-0.3.2"
        sptr.mkdir()
        (sptr / "Cargo.toml").write_text('[package]\nname="sptr"\nversion="0.3.2"\nlicense="MIT OR Apache-2.0"\n')
        (sptr / "original.rs").write_text("original source fixture")
        serde = self.vendor / "serde-1.0.229"
        serde.mkdir()
        (serde / "LICENSE-APACHE").write_text("Apache License\nVersion 2.0\nfixture")
        output = self.root / "notices"
        result = notices.collect(self.vendor, output)
        self.assertIn("Apache-2.0 option", result[0]["license_origin"])
        self.assertEqual((output / "sptr-0.3.2/original.rs").read_text(), "original source fixture")


if __name__ == "__main__":
    unittest.main()
