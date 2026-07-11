# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.1] - Unreleased

### Added

- crates.io publish job in the release workflow.
- Lint (rustfmt + clippy) job in CI.
- Crate metadata: authors, homepage, rust-version/MSRV 1.85.
- CONTRIBUTING, CODE_OF_CONDUCT, and issue/PR templates.

## [0.1.0] - 2026-07-09

### Added

- Trust-first hook execution: hooks committed to a repo auto-install via
  global shims (`git hooks install`), but nothing runs until you explicitly
  accept a prompt showing the exact commands and scripts involved.
- Consent bound to a content hash covering both `.githooks.toml` and every
  file under `.githooks/`, so any byte change re-prompts — the re-prompt shows
  a diff against what you last accepted, not the whole file.
- Warning in the consent prompt when an inline command references a
  repo-relative file outside `.githooks/` (content not covered by the hash).
- Safe-off by default with no controlling terminal (CI, cron, scripts): hooks
  are skipped, never silently run.
- `git hooks init` to adopt shims into an already-cloned repository.
- Full githooks(5) client hook list supported, not just a curated subset.
- Staged-file awareness: `{staged_files}` substitution and glob filtering per
  hook entry.
- `GIT_HOOKS_CONSENT` environment variable (`accept`, `decline`, or the
  pinned `accept:<hash>` form) for non-interactive CI opt-in.
- Signed trust: maintainers sign accepted hook content with an SSH key
  (`git hooks sign`); once a signer's key fingerprint is trusted, future
  signed changes auto-accept with no prompt.
- Key-level trust store: repo-local (`git hooks trust`) and org-wide
  (`~/.config/git-hooks/policy.toml`) trusted keys, plus a `decline` policy
  default for locked-down machines that only ever run pre-approved-key hooks.
- `git hooks diff` to show what changed since the last accepted content.
- Windows support: `USERPROFILE` and `CONIN$`/`CONOUT$` in place of
  `/dev/tty`.
- Published as the `hookguard` crate (binary stays `git-hooks`), with
  `cargo-binstall` metadata, a Nix flake, and dual MIT/Apache-2.0 licensing.
