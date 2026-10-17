"""Unprivileged build-command models; no real Nix, cross compiler or ARM execution."""
import contextlib
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


def tool(directory, name, body):
    path = directory / name
    path.write_text("#!" + shutil.which("bash") + "\nset -euo pipefail\n" + body)
    path.chmod(0o755)


@contextlib.contextmanager
def fixture(script):
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary) / "checkout with spaces"
        tests, tools = root / "tests", root / "tools"
        tests.mkdir(parents=True)
        tools.mkdir()
        shutil.copyfile(ROOT / "tests" / script, tests / script)
        env = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"],
                   MOCK_LOG=str(root / "commands"), MOCK_SOURCE=str(root / "source"),
                   MOCK_ELF=str(root / "elf"))
        (root / "source").write_text("")
        yield root, tools, env, tests / script


def run(script, env, *args):
    return subprocess.run(["bash", str(script), *args], env=env, cwd="/",
                          capture_output=True, text=True, timeout=10)


def commands(env):
    path = Path(env["MOCK_LOG"])
    if not path.exists():
        return []
    return [[part.decode() for part in call.split(b"\0")] for call in path.read_bytes().split(b"\0\0") if call]


class NativeNixGate(unittest.TestCase):
    @contextlib.contextmanager
    def model(self, **overrides):
        with fixture("run-nix-gate.sh") as (root, tools, env, script):
            tool(tools, "nix", '''printf '%s\\0' "$@" >> "$MOCK_LOG"
printf '\\0' >> "$MOCK_LOG"
if [[ " $* " == *' --expr '* ]]; then
  printf '%s' "$MOCK_SYSTEM"; exit "${MOCK_ARCH_EXIT:-0}"
elif [[ " $* " == *' --apply '* ]]; then
  printf '%s' "$MOCK_CHECKS"; exit "${MOCK_INVENTORY_EXIT:-0}"
fi
exit "${MOCK_BUILD_EXIT:-0}"
''')
            env.update(MOCK_SYSTEM="x86_64-linux", MOCK_CHECKS="installed-cli\nfuture-check\nroot-sleep-watch")
            env.update(overrides)
            yield root, env, script

    def test_every_discovered_native_check_is_built_without_a_duplicate_list(self):
        for system in ["x86_64-linux", "aarch64-linux"]:
            with self.subTest(system=system), self.model(MOCK_SYSTEM=system) as (_, env, script):
                result = run(script, env)
                self.assertEqual(result.returncode, 0, result.stderr)
                calls = commands(env)
                self.assertEqual(len(calls), 3)
                self.assertIn(f".#checks.{system}", calls[1])
                build = calls[-1]
                self.assertIn("--no-link", build)
                self.assertIn("--no-write-lock-file", build)
                self.assertEqual([arg for arg in build if arg.startswith(".#checks.")],
                                 [f".#checks.{system}.{name}" for name in env["MOCK_CHECKS"].splitlines()])

    def test_explicit_out_link_is_one_absolute_argument(self):
        with self.model() as (root, env, script):
            path = str(root / "output with spaces")
            result = run(script, env, path)
            self.assertEqual(result.returncode, 0, result.stderr)
            build = commands(env)[-1]
            self.assertEqual(build[build.index("--out-link") + 1], path)
            self.assertNotIn("--no-link", build)
        for args in [("relative",), ("",), ("/one", "/two")]:
            with self.subTest(args=args), self.model() as (_, env, script):
                self.assertEqual(run(script, env, *args).returncode, 2)
                self.assertEqual(commands(env), [])

    def test_empty_duplicate_invalid_or_excessive_inventory_never_builds(self):
        for inventory in ["", "one\none", "one\n", "one\n\ntwo", "../foreign", "one;touch owned",
                          "$(touch owned)", "Upper", "\n".join(f"check-{index}" for index in range(257))]:
            with self.subTest(inventory=inventory), self.model(MOCK_CHECKS=inventory) as (root, env, script):
                result = run(script, env)
                if inventory == "one\n":
                    self.assertEqual(result.returncode, 0, result.stderr)
                else:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(len(commands(env)), 2)
                self.assertFalse((root / "owned").exists())

    def test_foreign_native_system_and_evaluation_failures_never_build(self):
        for changes, code, count in [(dict(MOCK_SYSTEM="riscv64-linux"), 1, 1),
                                     (dict(MOCK_ARCH_EXIT="41"), 41, 1),
                                     (dict(MOCK_INVENTORY_EXIT="42"), 42, 2),
                                     (dict(MOCK_BUILD_EXIT="43"), 43, 3)]:
            with self.subTest(changes=changes), self.model(**changes) as (_, env, script):
                self.assertEqual(run(script, env).returncode, code)
                self.assertEqual(len(commands(env)), count)


ELF = """  Class:                             ELF64
  Data:                              2's complement, little endian
  Machine:                           AArch64
      [Requesting program interpreter: /lib/ld-musl-aarch64.so.1]
 0x0000000000000001 (NEEDED) Shared library: [libc.so]
 0x0000000000000001 (NEEDED) Shared library: [libgcc_s.so.1]
"""


class Aarch64Build(unittest.TestCase):
    @contextlib.contextmanager
    def model(self, elf=ELF, **overrides):
        with fixture("run-aarch64-build.sh") as (root, tools, env, script):
            tool(tools, "git", '''case $1 in
  rev-parse) printf '%040d\\n' 0 ;;
  diff) cat "$MOCK_SOURCE" ;;
  status) printf '%s' "${MOCK_STATUS:-}" ;;
  *) exit 9 ;;
esac
''')
            tool(tools, "cargo", '''printf '%s\\0' "$@" >> "$MOCK_LOG"; printf '\\0' >> "$MOCK_LOG"
if [[ ${MOCK_MUTATE:-0} == 1 ]]; then echo changed >> "$MOCK_SOURCE"; fi
if [[ ${MOCK_CARGO_EXIT:-0} != 0 ]]; then exit "$MOCK_CARGO_EXIT"; fi
mkdir -p "$CARGO_TARGET_DIR/aarch64-unknown-linux-musl/release"
printf '#!/bin/sh\\nexit 97\\n' > "$CARGO_TARGET_DIR/aarch64-unknown-linux-musl/release/prek"
''')
            tool(tools, "readelf", 'cat "$MOCK_ELF"\n')
            tool(tools, "cat", '''if [[ ${1:-} == */"${MOCK_CAT_FAIL:-never}" ]]; then exit 45; fi
exec ''' + shutil.which("cat") + ''' "$@"
''')
            (root / "elf").write_text(elf)
            env.update(overrides)
            yield root, env, script

    def test_retains_artifact_hash_and_linkage_without_executing_it(self):
        with self.model() as (root, env, script):
            result = run(script, env)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            evidence = (root / "target/cross-aarch64/latest").resolve(strict=True)
            self.assertEqual((evidence / "status.txt").read_text(), "passed\n")
            self.assertIn("execution=not-attempted\n", (evidence / "receipt.txt").read_text())
            self.assertIn("source_dirty=false\n", (evidence / "receipt.txt").read_text())
            self.assertIn("nix_runtime_references=false\n", (evidence / "receipt.txt").read_text())
            self.assertEqual((evidence / "elf.txt").read_text(), ELF)
            wanted = hashlib.sha256((evidence / "prek").read_bytes()).hexdigest()
            self.assertEqual((evidence / "SHA256SUMS").read_text().split(), [wanted, "prek"])
            self.assertEqual(commands(env)[0], ["build", "--locked", "--release", "--bin", "prek", "--target", "aarch64-unknown-linux-musl"])

    def test_dirty_source_is_explicit_and_does_not_claim_native_execution(self):
        with self.model(MOCK_STATUS=" M src/lib.rs\n") as (root, env, script):
            result = run(script, env)
            self.assertEqual(result.returncode, 0, result.stderr)
            receipt = (root / "target/cross-aarch64/latest/receipt.txt").read_text()
            self.assertIn("nix_runtime_references=false\n", receipt)
            self.assertIn("source_dirty=true\n", receipt)

    def test_wrong_architecture_loader_endian_or_missing_dynamic_libc_refuse(self):
        for elf in [ELF.replace("AArch64", "Advanced Micro Devices X86-64"), ELF.replace("ELF64", "ELF32"),
                    ELF.replace("little endian", "big endian"), ELF.replace("/lib/ld-musl-aarch64.so.1", "/nix/store/loader"),
                    ELF.replace("libc.so]", "libc.so.6]"), "",
                    ELF + ' 0x000000000000001d (RUNPATH) Library runpath: [/nix/store/fixture/lib]\n',
                    ELF + ' 0x000000000000000f (RPATH) Library rpath: [/home/fixture/lib]\n',
                    ELF + ' 0x0000000000000001 (NEEDED) Shared library: [unexpected.so]\n',
                    ELF.replace("libgcc_s.so.1]", "libforeign.so]")]:
            with self.subTest(elf=elf), self.model(elf) as (root, env, script):
                self.assertNotEqual(run(script, env).returncode, 0)
                evidence = root / "target/cross-aarch64/latest"
                self.assertEqual((evidence / "status.txt").read_text(), "failed\n")
                self.assertFalse((evidence / "receipt.txt").exists())

    def test_failed_build_or_changed_source_preserves_logs_without_acceptance(self):
        for change in [dict(MOCK_CARGO_EXIT="44"), dict(MOCK_MUTATE="1"),
                       dict(MOCK_CAT_FAIL="receipt.txt"), dict(MOCK_CAT_FAIL="SHA256SUMS")]:
            with self.subTest(change=change), self.model(**change) as (root, env, script):
                result = run(script, env)
                self.assertNotEqual(result.returncode, 0)
                evidence = root / "target/cross-aarch64/latest"
                self.assertEqual((evidence / "status.txt").read_text(), "failed\n")
                self.assertTrue((evidence / "build.log").exists())
                self.assertFalse((evidence / "receipt.txt").exists())


if __name__ == "__main__":
    unittest.main()
