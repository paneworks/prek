# Nix application bundles

Prek's release artifact is `prek-VERSION-SYSTEM-nix.tar.gz`, for
`x86_64-linux` or `aarch64-linux`. It contains an application runtime closure
in Nix's file binary-cache format, not a source `.crate` or a portable ELF.
It requires Linux, Nix with the `nix-command` feature, and Python 3.11 or newer.
The ELF interpreter and runtime libraries retain their `/nix/store` paths.
The bundle does not provide Cage, Hyprland, Morf or a working login session.
Broker use still requires the explicitly configured host libsystemd,
compositor, device permissions and session policy documented in
[nixos.md](nixos.md). These are not implied by a successful CLI smoke check.

## Verify and import

Obtain the architecture-matching archive and `SHA256SUMS` from the same trusted
release. Check their origin before using an unsigned bundle. Checksums detect
corruption; they are not publisher signatures. Do not import an archive from
an untrusted source, even if its self-contained checksums pass.

```sh
sha256sum --check SHA256SUMS
tar -xzf prek-VERSION-SYSTEM-nix.tar.gz
cd prek-VERSION-SYSTEM-nix
python3 install.py verify --system x86_64-linux # or aarch64-linux
python3 install.py import --trust-unsigned
```

`verify` checks the exact file inventory, checksums, local-only NAR URLs,
dependency references and NAR contents. `import` also requires the native
architecture. It copies from the bundled cache with no substitutes, no remote
builders, zero local build jobs and no build fallback. It validates the
imported closure and runs only non-device CLI/configuration checks in a fresh
temporary directory with a private HOME. It never starts a broker, opens
physical input, installs a service or changes a system/user profile.

The explicit trust flag passes Nix's `--no-check-sigs` for this copy, not a
permanent `require-sigs = false` setting. A multi-user Nix daemon may reject
unsigned imports by an untrusted user. Ask the machine administrator to
approve/import the verified bundle; this tool does not escalate privileges.
No signing key, public hosting account or external cache is assumed.

The command prints the exact `store_path`. Run that path's `bin/prek --help`
or `bin/prek init --preset minimal --output ./init.lua`. The installation has
presets in `share/prek/presets`, Lua examples/editor definitions in
`share/prek`, a NixOS module in `share/prek/nixos`, a udev rule in
`lib/udev/rules.d`, and documentation/license in `share/doc/prek`.
`share/doc/prek/dependency-notices` includes vendored crate license/notice
texts and declarations, including Luna's shared Piccolo/Luna copyrights.
Retain a GC root/profile for long-term use; an unrooted imported closure can
be collected. For normal NixOS installations prefer the upstream module and
tracked flake input described in [nixos.md](nixos.md), rather than embedding
one release's store path in system configuration.

## Local build and export

From the checkout, use `oslo make dist-nix`. It builds the native default
package, tests its installed CLI/presets/examples/metadata outside the checkout,
then exports and verifies the complete runtime closure. Outputs live under
`target/distribution`. `oslo make test-distribution` runs unprivileged bundle
validation tests; it neither builds nor imports anything.
`oslo make verify-msrv` runs the full native Rust gate on the pinned 1.88.0
toolchain in a separate target directory. Untagged local exports record a
dirty worktree explicitly; dirty snapshots cannot claim a release tag.

`oslo make check-dist-nix` enumerates and builds **every** check exported for
the current native system. CI uses the same `tests/run-nix-gate.sh`; there is no
second hand-maintained check list. Empty, duplicate or malformed inventories
fail before building. Checks that prepare privileged fixtures only build/lint
them; the gate never launches the root scripts.

### ARM64 cross-build, without execution

`oslo make build-aarch64` uses the declared `cross-aarch64` development shell to
build the current application for `aarch64-unknown-linux-musl`. It inspects the
actual retained ELF with `readelf`, requiring 64-bit little-endian AArch64,
the `/lib/ld-musl-aarch64.so.1` interpreter and dynamic musl libc linkage.
Only system `libc.so` and `libgcc_s.so.1` dependencies are accepted; any
`RPATH`/`RUNPATH` or unexpected dependency fails the build. The cross-shell
disables automatic Nix runtime paths and clears inherited linker flags.
It does not execute the result or use `ldd`, an emulator, VM or remote machine.

`target/cross-aarch64/latest` points to that attempt's logs, source revision and
dirty-state record, source-diff digest, complete ELF headers/dynamic table and
`status.txt`. A successful attempt retains its own `prek` copy, `SHA256SUMS` and
`receipt.txt`; failed or changed-source builds have no receipt. The receipt
explicitly says `execution=not-attempted` and confirms the checked linkage.
Rejected ELF headers and logs remain available without a successful receipt.

`oslo make exec-aarch64` runs that artifact separately, under the host kernel's
enabled aarch64 binfmt emulation. It accepts only a passed, clean cross-build of
the current `HEAD`, rechecks the retained hash before and after, and executes a
copy whose interpreter and runtime path point at the pinned cross musl loader
and `libgcc_s.so.1`; the retained artifact is never modified. It requires
`prek --version` to match `Cargo.toml` and `prek check --json` to report the
shipped `touch_routing`, `window_keys` and `power` Lua examples valid, which
exercises Luna and the isolated compile worker. Evidence lands in
`target/cross-aarch64/executed`; its receipt says `execution=emulated-binfmt`
and `hardware=false`. Emulation is not ARM64 hardware, phone, native-bundle or
system-loader evidence.

This is a **dynamic musl cross-build**, not a portable standalone binary or the
native GNU/Nix ARM64 bundle. The target needs the recorded loader, libraries and
runtime integration; cross-compilation does not prove they are installed or
compatible on a phone. Native ARM tests, protected systemd/PAM integration,
real device acceptance and the native bundle's fresh-runner checks remain
separate requirements. No installation, service activation or deployment occurs.

For an offline import challenge without writing the host's store:

```sh
python3 install.py import --trust-unsigned --require-missing \
  --store-root /absolute/path/to/new-empty-store
```

This creates an isolated relocated store and verifies every imported NAR.
It does **not** execute the application there: the ELF's logical store paths
still point to `/nix/store`. Running relocated-store programs would require
an appropriate chroot/namespace. No VM or privileged test is launched.
After export, `oslo make test-dist-offline` performs this isolated import,
refuses a repeated fresh import and challenges Nix with a corrupted NAR whose
outer file checksums have been recalculated. Evidence remains under
`target/distribution/offline-*`; only that extracted test copy is corrupted.

## CI and release boundary

Branch CI builds bundles natively on x86-64 and ARM64. Separate fresh jobs
download each bundle, require the application path to be absent, import with
builders/substituters disabled and run installed non-device checks. Native
Rust 1.88 and stable jobs also run all four actual supervised Lua helper
families. A workflow declaration or cross-compilation is not evidence these
jobs have executed successfully.

Release jobs accept only existing `vVERSION` tags whose version matches both
Cargo.toml and the installed executable. Both native bundles must pass their
fresh-runner imports before publication. No source-crate archive is published
as an architecture-specific application. Publication uses the workflow's
repository identity, not an assumed external cache or unrelated credentials.
The archive itself supplies a file binary cache; public external cache
hosting/signing and successful published/download-only acceptance remain
separate evidence gates.

Local CLI/configuration checks do not prove physical routing, kernel tests,
rendered Cage behavior, native phone execution or device/session integration.
The dynamic ARM musl phone shell is a separate target, not the GNU/Nix bundle.

Nix documents closure copying and relocated-store limitations in its
[copy reference](https://nix.dev/manual/nix/2.24/command-ref/new-cli/nix3-copy)
and [local-store reference](https://nix.dev/manual/nix/2.35/store/types/local-store.html).
