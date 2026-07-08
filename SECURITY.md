# Security & trust model

git-hooks exists to close one specific hole: **cloning a repository must not
silently run code the person cloning it never agreed to.** This document is the
honest account of what that guarantee does and does not buy you.

## The threat

Git hooks are code the repository author wrote that runs on *your* machine
during ordinary git operations (commit, checkout, push). Most hook managers copy
the cloned config into `.git/hooks` and run it, no questions asked. A malicious
or compromised upstream then executes arbitrary commands the first time you
touch the repo.

## What a clone can and cannot make run

- **Cannot run anything without your consent.** The globally-installed shims
  (`init.templateDir`) dispatch to `git-hooks run <hook>`, which refuses to run
  any command until you have *accepted this exact content* for this repository.
  Your decision lives in `.git/config` (`hooks.consent = accept:<hash>` /
  `decline:<hash>`), which is never cloned.
- **Cannot run on a machine with no terminal.** If there is no `/dev/tty` to
  prompt on (CI, scripts, cron), consent defaults to *off*: hooks are skipped
  and the run is a no-op. Safe by default, never a silent yes.
- **Can only ever run the two hook forms below** — both fully covered by the
  consent hash.

## Hook forms

1. **Inline commands** in `.githooks.toml` (`pre-commit = ["cargo fmt --check"]`).
2. **Scripts** committed under a `.githooks/` directory at the repo root,
   referenced by a command that starts with `.githooks/`
   (`pre-commit = [".githooks/check.sh"]`). Scripts still run via `sh -c`, so
   arguments and shell substitution work as normal.

Either form may be written as a bare string or as an inline table that adds
staged-file awareness:

```toml
pre-commit = [{ run = "rustfmt --check {staged_files}", glob = "*.rs" }]
```

`{staged_files}` expands to the shell-quoted list of staged files
(`git diff --cached --name-only --diff-filter=ACMR`). An optional `glob` filters
that list first; with a glob and no matching staged files the command is skipped
entirely. The glob is deliberately tiny: `*` matches any run of characters
except `/`, `**` matches across `/`, and `?` matches one character — so `*.rs`
matches `main.rs` but not `src/main.rs` (use `**/*.rs` for nested paths). The
table form is covered by the consent hash exactly like the string form.

## What the consent hash covers

The hash bound to your decision is:

```
<git hash-object of .githooks.toml> - <tree hash of .githooks/>
```

The tree hash is deterministic: every file under `.githooks/` is walked
recursively in sorted order, `git hash-object`'d, and the concatenated
`path:blob` lines are themselves hashed (`git hash-object --stdin`). No
`.githooks/` directory yields a fixed `none` sentinel.

**Any byte change to `.githooks.toml` or to any file under `.githooks/`
produces a different hash and re-prompts.** Because consent is keyed to
*content*, an upstream that changes what its hooks do always asks you again —
there is no "trust once, run forever" window for the code itself.

On accept, the accepted bytes are written into git's object database
(`git hash-object -w`) and a manifest of `path <TAB> blob` lines is stored in
`.git/config` (`hooks.consentManifest`). When content later changes, the
re-prompt shows a **`git diff` of each changed file against what you accepted**,
not a wall of config. Declines stay cheap — just the hash, no objects written.

## What the consent hash does NOT cover

Be clear-eyed about the boundary:

- **Runtime-referenced repo files.** An inline command can invoke any file in
  the working tree at run time — `./scripts/deploy.sh`, `make`, a checked-in
  binary. Those files are **not** part of the consent hash, so their contents
  can change without re-prompting. As a mitigation, the consent prompt scans
  inline commands and warns when a token looks like a repo-relative path outside
  `.githooks/` that resolves to an existing file. The heuristic is deliberately
  simple (whitespace-split tokens starting with `./` or containing `/`); it has
  **false negatives** — a path assembled by shell substitution, an env var, or a
  wrapper script will not be caught. If you want a file's content covered, put
  it under `.githooks/`.
- **Programs on `$PATH`.** `cargo`, `npm`, `python`, `sh` itself — you are
  trusting whatever those resolve to on your machine. Consent covers the command
  text, not the binaries it names.
- **Network.** A hook may download and execute remote content. Consent covers
  the command that reaches out, never what comes back.
- **Everything after acceptance.** Consent is a decision about specific content,
  not a sandbox. Accepted hooks run with your full user privileges.

## No-TTY behavior (safe-off)

With no controlling terminal, `git-hooks` cannot ask, so it does not run.
Hooks are skipped and a one-line notice explains how to opt in. This is the
correct default for CI and any non-interactive context.

## CI story

CI should opt in **explicitly and deliberately**, never interactively:

```sh
git hooks accept    # records accept:<hash> for the current content
```

Run it as a reviewed pipeline step after you have inspected the hooks. If the
committed hooks change, a stale `accept` no longer matches the new hash, so the
hooks are skipped (safe-off) until someone re-accepts — CI fails open to *not
running*, not to running unknown code.

### `GIT_HOOKS_CONSENT` (non-interactive override)

For pipelines that cannot run `git hooks accept` (ephemeral checkouts, container
builds), set the environment variable `GIT_HOOKS_CONSENT`:

- `GIT_HOOKS_CONSENT=accept` — run the hooks regardless of stored consent.
- `GIT_HOOKS_CONSENT=decline` — skip the hooks regardless of stored consent.
- `GIT_HOOKS_CONSENT=accept:<hash>` — run **only if** `<hash>` matches the
  current content hash (the value shown by `git hooks status`). This is the
  reproducible, pinned form: the moment the committed hooks change, the pinned
  hash no longer matches and the hooks fall back to normal (safe-off) behavior.

The override applies to that single invocation and is **never persisted** to
`.git/config`. Prefer the pinned `accept:<hash>` form. Plain `accept` disables
the content check entirely, so use it only in environments you fully control and
whose repository contents you trust.

## Comparison note

Managers like lefthook and hk have no trust model: cloned config runs, period.
pre-commit refused auto-install-on-clone rather than solve consent.
gabyx/Githooks has trust checksums but a heavy install and weak prompt UX.
git-hooks keys consent to the **content** of everything executable, so an
upstream change always re-prompts and shows you exactly what changed — the trust
boundary is the product, not an afterthought.
