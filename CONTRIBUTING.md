# Contributing

## Dev setup

Standard cargo, no exotic toolchain.

```sh
cargo build
cargo test
```

Nix users: `nix develop` for a shell with the right toolchain, or `nix build`
to build via the flake (`flake.nix`).

## Tests

`cargo test` runs the unit tests everywhere. The e2e suite (`tests/e2e.rs`) is
**Linux-only** (`#![cfg(target_os = "linux")]`) — it shells out to real `git`,
uses `setsid -w` (util-linux) to detach the controlling terminal for no-tty
consent-path tests, and `ssh-keygen` (openssh) for the signed-trust tests. On
macOS/Windows that file compiles away; CI there is just `cargo build` + the
unit tests.

## Code style

`rustfmt` and `clippy` must pass clean before a PR is reviewed:

```sh
cargo fmt --check
cargo clippy -- -D warnings
```

## PRs

- Keep them small and focused — one behavior change per PR.
- Add or update tests for any behavior change (unit or e2e as appropriate).
- If the change touches the trust/consent model — hashing, signing,
  verification, policy — read [SECURITY.md](SECURITY.md) first and say in the
  PR description how the change affects the threat model. Changes that widen
  the trust boundary need an explicit justification, not just a passing test.

## Reporting security issues

Do not open a public issue for a vulnerability. See [SECURITY.md](SECURITY.md)
for the trust model and where to report.

## License

By contributing, you agree your contribution is licensed under MIT OR
Apache-2.0, matching the rest of the project.
