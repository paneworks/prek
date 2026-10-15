# Morf integration

Prek's broker can consume Morf's native lock evidence with
`broker --morf-executable /path/to/the/actual/morf-ELF`. The NixOS equivalent is
`services.prek.morfExecutable`. Use a Morf build containing the native provider;
a shell wrapper is not an executable identity. No desktop/root deployment is
performed by building this project.

The upstream implementation lives in `../mold` under
`crates/frontend/morf-host/src/lock/provider.rs`. It creates a private native
endpoint when the real Wayland lock loop starts. Compositor identity comes from
that connection's socket credentials and process birth. Lua cannot supply or
advance a native phase. The provider reports `pending`, the actual `locked`
acknowledgement, and `unlock_dispatched` after the backend flushed unlock.

Prek verifies endpoint ownership/permissions, Morf's exact executable and process
lifetime, active logind membership, and the bound compositor before accepting
the report. Its sampler independently confirms that the compositor is unlocked
before retiring inhibition; it then acknowledges the exact provider epoch and
revision. Socket/process loss never proves unlock. Morf preserves completed
evidence for a sampled observer for at most two seconds, dropping its rendering
resources before waiting. Only root may acknowledge completion; a same-user
reader can inspect the native report without releasing it.

This supplies native lock evidence. Morf shell/keyboard rendered regions,
typed drawer/workspace actions and progress, synthetic keyboard ownership and
the global-recognizer handoff still require implementation. The public
`prek.morf` runtime family is not advertised as implemented. Root/logind and
physical authentication acceptance remain unverified by local model fixtures.

The optional owned local test is:

```sh
nix run .#cage-test -- --modes --input --power --windows \
  --morf /absolute/path/to/morf/target/debug/morf
```

Pass the actual ELF and its normal runtime library environment. The test runs
Morf on the owned nested compositor, compares native reports with actual stock
lock state, checks process/compositor/epoch/revision identity, refuses an
unprivileged release and checks endpoint cleanup. Its small fixture explicitly
authorizes unlock through a timer after observing the compositor lock. It does
not exercise PAM, root logind registration, host devices or the host display.
`--visible` repeats the same checks in the visible owned nested session.

Reports are saved beside the normal Cage receipts in `morf-lock-receipts.json`
and `morf-lock.log`. The report explicitly identifies fixture authentication
and the absence of a root/logind execution claim. Tests use short private paths
under `/tmp/pc-*` so native Unix sockets still fit when launched from a Nix
development shell with a long `TMPDIR`.
