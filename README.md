# hookguard (`git-hooks`)

The trust-first git hook manager: hooks committed to a repo auto-install the
moment you clone it — but nothing runs until you say so.

Most hook managers copy whatever config a repository ships and execute it
immediately; cloning becomes an implicit "run arbitrary code" step. `git-hooks`
installs global shims once per machine (`git hooks install`) so every future
clone or `init` picks up committed hooks automatically, but the shims dispatch
to a gate: the first time a hook would run, you're prompted on the terminal
with the exact commands and scripts involved, and nothing executes until you
accept. Your decision is keyed to a content hash of everything executable, so
if the upstream config changes so much as a byte, you're prompted again — with
a diff, not a wall of text. A maintainer can sign accepted content with an SSH
key; once you trust that key's fingerprint, future signed changes auto-accept
with no prompt, killing the fatigue without reopening the hole.

See [SECURITY.md](SECURITY.md) for the full trust model, including what it
does and does not cover.

## Quickstart

### Maintainer side

```sh
git hooks install                          # one-time, per machine
git hooks add pre-commit "cargo fmt --check"
git add .githooks.toml
git commit -m "add pre-commit hook"

# optional: sign so trusting teammates never get re-prompted
git hooks sign --key ~/.ssh/id_ed25519
git add .githooks/trust
git commit -m "sign hooks"
```

### Cloner side

```sh
git hooks install        # one-time, per machine
git clone git@github.com:acme/widgets.git
cd widgets
git commit -m "..."      # first hook trigger prompts for consent
```

## What the prompt looks like

```
$ git hooks install
installed. new clones get hook shims automatically.

$ git clone git@github.com:acme/widgets.git
Cloning into 'widgets'...
...

$ cd widgets
$ git commit -m "fix bug"

this repository wants to run the following hooks (.githooks.toml):

[pre-commit]
  cargo fmt --check
  cargo clippy -- -D warnings

these commands will run on your machine during git operations.
accept? [y/N] y
accepted. change your mind anytime: `git hooks accept` / `git hooks decline`.
```

If the hooks are signed by a key you already trust, this is silent — no
prompt, no terminal required. If there's no terminal at all (CI, cron,
scripts), the answer is always "no": hooks are skipped, safe by default.

## Config reference

### `.githooks.toml`

Hooks live under a `[hooks]` table, keyed by githooks(5) hook name. Each entry
is a list; every list item is either a bare command string or a table with
`run` and an optional `glob`:

```toml
[hooks]
pre-commit = [
  "cargo fmt --check",
  { run = "rustfmt --check {staged_files}", glob = "*.rs" },
  ".githooks/check.sh",
]
```

- **String** — an inline shell command, run via `sh -c`.
- **Table (`{ run, glob }`)** — same, plus staged-file awareness: `{staged_files}`
  expands to the shell-quoted list of staged files
  (`git diff --cached --name-only --diff-filter=ACMR`). An optional `glob`
  filters that list first (`*` matches within a path segment, `**` crosses
  `/`, `?` matches one character); with a glob and no matching staged files,
  the command is skipped entirely.
- Any command may reference a script committed under `.githooks/` — that
  content is covered by the consent hash exactly like the toml itself.

### `.githooks/` scripts

Executable scripts referenced by a command starting with `.githooks/`
(`".githooks/check.sh"`). They run through `sh -c`, so arguments and shell
substitution behave as normal. Every file under this directory, walked
recursively in sorted order, is folded into the consent hash — change one byte
and every cloner is re-prompted.

### `~/.config/git-hooks/policy.toml`

Org-wide policy, seeded once (e.g. by config management), consulted before any
repo-local trust:

```toml
trusted_keys = ["SHA256:abcd1234..."]
default = "prompt"   # or "decline" for locked-down machines
```

`default = "decline"` means: if no *trusted* signature verifies, hooks are
skipped with a stderr notice and no prompt is ever shown — the machine only
ever runs hooks signed by a pre-approved key.

## How we compare

| | hookguard | pre-commit | husky | lefthook | hk | gabyx/Githooks |
|---|---|---|---|---|---|---|
| Auto-install on clone | yes | no (refused, [#1084](https://github.com/pre-commit/pre-commit/issues/1084)) | yes (npm postinstall) | no | no | yes |
| Consent prompt with diff | yes | — | — | — | — | checksum, weak UX |
| Signed trust (key, not content) | yes | no | no | no | no | no |
| Staged-file filters | yes | yes | plugin-dependent | yes | yes | no |
| Language env management | **no** | **yes** (its moat) | no | no | no | no |
| Hook ecosystem / marketplace | **no** | **yes** (huge) | no | no | no | **yes** |
| Single static binary | yes | no (Python) | no (Node) | yes | yes | no (heavy install) |
| Auditable in one sitting | yes (~1 file) | no | no | mostly | mostly | no |

We lose on language environments and the hook ecosystem — pre-commit and
gabyx/Githooks have spent years on those and we deliberately haven't. What we
bet on instead is a consent model with no holes: content-keyed hashing, diffs
on every change, and signed trust that only ever narrows to a key you
explicitly vouched for.

## Security model

Full writeup: [SECURITY.md](SECURITY.md). The short version:

- **No terminal, no run.** CI, cron, and scripts get a safe-off no-op unless
  they opt in explicitly.
- **Nothing runs before consent**, and consent is bound to the exact bytes of
  everything executable — the toml and every file under `.githooks/`.
- **Signed trust moves the boundary from content to key**, not away from
  consent: an untrusted signature still prompts, an invalid one screams.
- What it does **not** cover: runtime-referenced repo files outside
  `.githooks/`, `$PATH` binaries, and the network. See SECURITY.md for the
  honest boundary.

## CI

CI should opt in explicitly, never interactively. Either accept once as a
reviewed pipeline step:

```sh
git hooks accept
```

or set the environment variable for a single run:

```sh
GIT_HOOKS_CONSENT=accept       git commit -m "..."   # ignore stored consent, run
GIT_HOOKS_CONSENT=decline      git commit -m "..."   # ignore stored consent, skip
GIT_HOOKS_CONSENT=accept:<hash> git commit -m "..."  # run only if content matches <hash>
```

The pinned `accept:<hash>` form (hash from `git hooks status`) is preferred:
the moment committed hooks change, the pin stops matching and the run falls
back to safe-off.

## Install

```sh
cargo install hookguard
cargo binstall hookguard        # prebuilt binary, no compile
nix run github:itaywol/hookguard  # flake, ad hoc
nix profile install github:itaywol/hookguard  # flake, persistent
```

Or grab a prebuilt binary from the
[releases page](https://github.com/itaywol/hookguard/releases) — static musl on
Linux, native on macOS and Windows.

## License

MIT OR Apache-2.0.
