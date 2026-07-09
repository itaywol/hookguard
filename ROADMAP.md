# Roadmap

Positioning: **the trust-first git hook manager.** Hooks are code someone else
wrote that runs on your machine — every competitor treats that as an
afterthought. We treat it as the product.

- vs **pre-commit**: they explicitly refused auto-install-on-clone (issue #1084). We do it safely.
- vs **lefthook / hk**: fast runners, zero trust model — cloned config runs, period.
- vs **gabyx/Githooks**: has trust checksums, but huge tool, heavy install, weak UX around the prompt.

Our bet: one tiny static Rust binary, auditable in one sitting, whose consent
model has no holes. Feature-count is not the game; being *provably safe to
recommend org-wide* is.

## M1 — sound trust model (the reason to exist; nothing ships before this)

The current hole: consent hashes `.githooks.toml`, but a command like
`./scripts/check.sh` can change content without changing the hash.

- [ ] Hook definitions limited to two forms, both coverable by hash:
      inline commands in `.githooks.toml`, and scripts under committed `.githooks/` dir.
- [ ] Consent hash = `git hash-object` of the toml **+ tree hash of `.githooks/`**.
      Any byte change anywhere → re-prompt.
- [ ] Re-prompt shows a **diff** against the last accepted version, not the whole file.
- [ ] Document the trust model in SECURITY.md, including what it does NOT cover
      (commands can still invoke repo files at runtime — warn in prompt when a
      command references a repo path outside `.githooks/`).
- [ ] Integration tests for every consent path (port the ad-hoc e2e shell runs).

## M2 — table stakes (parity where absence blocks adoption)

- [x] `git hooks init` — adopt shims into an already-cloned repo.
- [x] Full githooks(5) hook list.
- [x] Windows: USERPROFILE, CONIN$/CONOUT$ instead of /dev/tty (git-for-windows ships sh, shims already fine).
- [x] Staged-file awareness: `{staged_files}` substitution + glob filter per hook
      (the one lefthook feature people actually use).
- [x] `GIT_HOOKS_CONSENT=accept:<hash>` env override — reproducible CI opt-in.
- [x] CI matrix (linux/macos/windows), static musl release builds.

## M3 — differentiators (features nobody has)

- [x] **Signed trust**: maintainer signs `.githooks.toml` (ssh-keygen -Y, key
      committed or in allowed_signers). User trusts the *key* once; config
      changes signed by it stop prompting. Kills prompt fatigue org-wide.
- [x] `git hooks diff` — what changed since I last accepted.
- [x] Team policy file (`~/.config/git-hooks/policy.toml`): pre-trusted keys,
      always-decline patterns, org defaults.

## M4 — distribution

- [x] Name + crates.io claim (`git-hooks` was taken/generic — published as `hookguard`,
      binary stays `git-hooks`).
- [x] cargo-binstall, nix flake. README with honest comparison table.
- [ ] homebrew tap (deferred — needs separate tap repo).
- [x] MIT OR Apache-2.0.

## Explicitly not doing

- Language environment management (pre-commit's moat — years of work, orthogonal to trust).
- Hook marketplaces / shared hook repos (gabyx's complexity spiral; a git URL in your toml is enough).
- Auto-update of ourselves (package managers exist).
- Sandboxing hooks (research note only; consent model is the boundary we sell).
