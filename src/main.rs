use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};

// Client-side hooks worth shimming (githooks(5)). Server-side receive hooks
// (pre-receive/update/post-receive/post-update/proc-receive) and the very hot
// reference-transaction hook are intentionally omitted — this is a client tool.
const HOOKS: &[&str] = &[
    "applypatch-msg",
    "pre-applypatch",
    "post-applypatch",
    "pre-commit",
    "pre-merge-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "pre-rebase",
    "post-checkout",
    "post-merge",
    "pre-push",
    "post-rewrite",
    "pre-auto-gc",
];

const CONFIG_FILE: &str = ".githooks.toml";
// Committed directory of hook scripts. A command in {CONFIG_FILE} that starts
// with this prefix runs the named script (still via `sh -c`, so args and shell
// substitution keep working). Everything under it is hash-covered.
const HOOKS_DIR: &str = ".githooks";
// Marker line in every shim we write, so `init` can tell our shims from a
// foreign hook it must not clobber.
const SHIM_MARKER: &str = "# git-hooks shim";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("install") => install(),
        Some("init") => init(),
        Some("uninstall") => uninstall(),
        Some("add") => add(
            args.get(1).unwrap_or_else(|| usage()),
            args.get(2).unwrap_or_else(|| usage()),
        ),
        Some("accept") => consent(true),
        Some("decline") => consent(false),
        Some("status") => status(),
        Some("diff") => diff(),
        Some("sign") => sign(&args[1..]),
        Some("trust") => trust_cmd(&args[1..]),
        Some("untrust") => untrust_cmd(&args[1..]),
        Some("run") => {
            let hook = args.get(1).unwrap_or_else(|| usage());
            exit(run_hook(hook, &args[2..]));
        }
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: git hooks <command>\n\
         \n\
         install                  one-time global setup (shims via init.templateDir)\n\
         init                     copy shims into an already-cloned repo\n\
         uninstall                remove global setup\n\
         add <hook> <command>     add a command to this repo's {CONFIG_FILE}\n\
         accept | decline         record your decision for this repo's hooks\n\
         status                   show configured hooks and your decision\n\
         diff                     show what changed since you last accepted\n\
         sign --key <path> [--signer <principal>]\n\
         \x20                        sign the current hooks with an ssh key (maintainer)\n\
         trust <fingerprint> [--global]\n\
         \x20                        trust a signing key (repo-local, or org policy)\n\
         untrust <fingerprint> [--global]   stop trusting a signing key\n\
         run <hook> [args...]     (called by the shims)\n\
         \n\
         hooks are inline commands in {CONFIG_FILE}, or scripts committed under\n\
         {HOOKS_DIR}/ (reference them with a command like `{HOOKS_DIR}/check.sh`).\n\
         consent is keyed to the content of both; any byte change re-prompts.\n\
         a maintainer can `sign` the hooks so cloners who `trust` the key never\n\
         get prompted while the signature stays valid."
    );
    exit(2);
}

/// Home directory: HOME on unix, USERPROFILE as a Windows fallback.
fn home() -> PathBuf {
    env::var("HOME")
        .or_else(|_| env::var("USERPROFILE"))
        .map(PathBuf::from)
        .expect("HOME/USERPROFILE not set")
}

fn template_dir() -> PathBuf {
    home().join(".config/git-hooks/template")
}

/// The shim script for `hook`. Includes {SHIM_MARKER} so `init` can recognise
/// our own hooks and refuse to overwrite foreign ones. Stays `#!/bin/sh`: git
/// (incl. git-for-windows, which ships sh) runs hooks through a shell, and it
/// resolves `git-hooks`/`git-hooks.exe` on PATH automatically.
fn shim_body(hook: &str) -> String {
    format!("#!/bin/sh\n{SHIM_MARKER}\nexec git-hooks run {hook} \"$@\"\n")
}

/// Set the executable bit (no-op off unix; Windows relies on the shebang via
/// git's bundled sh).
fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// One-time global setup, LFS-style: write shim hooks into a template dir and
/// point init.templateDir at it. Every future `git clone`/`git init` copies
/// the shims into .git/hooks automatically.
fn install() {
    let hooks_dir = template_dir().join("hooks");
    fs::create_dir_all(&hooks_dir).expect("create template dir");

    for hook in HOOKS {
        let path = hooks_dir.join(hook);
        fs::write(&path, shim_body(hook)).expect("write shim");
        make_executable(&path);
    }

    git(&[
        "config",
        "--global",
        "init.templateDir",
        template_dir().to_str().unwrap(),
    ]);
    println!("installed. new clones get hook shims automatically.");
    println!("existing repo? run `git hooks init` inside it to copy the shims.");
}

/// Adopt an already-cloned repo: copy the shims from the template dir into this
/// repo's hooks dir. Never clobbers a foreign (non-shim) hook. Reports what was
/// installed vs skipped.
fn init() {
    if repo_root().is_none() {
        eprintln!("not inside a git repository");
        exit(1);
    }
    let tmpl = template_dir().join("hooks");
    if !tmpl.is_dir() {
        eprintln!("no hook template found — run `git hooks install` first.");
        exit(1);
    }
    let dest = hooks_path().unwrap_or_else(|| {
        eprintln!("could not resolve this repo's hooks dir");
        exit(1);
    });
    fs::create_dir_all(&dest).expect("create hooks dir");

    let mut installed = Vec::new();
    let mut skipped = Vec::new();
    for hook in HOOKS {
        let src = tmpl.join(hook);
        if !src.is_file() {
            continue;
        }
        let target = dest.join(hook);
        if target.exists() {
            let existing = fs::read_to_string(&target).unwrap_or_default();
            if !existing.contains(SHIM_MARKER) {
                skipped.push(*hook);
                continue; // foreign hook — leave it alone
            }
        }
        fs::copy(&src, &target).expect("copy shim");
        make_executable(&target);
        installed.push(*hook);
    }

    if installed.is_empty() {
        println!("no shims installed.");
    } else {
        println!("installed shims: {}", installed.join(", "));
    }
    if !skipped.is_empty() {
        println!(
            "skipped (existing non-shim hook, left untouched): {}",
            skipped.join(", ")
        );
    }
}

/// This repo's hooks directory (honours core.hooksPath / worktrees via
/// `git rev-parse --git-path hooks`). The path git prints is relative to the
/// current directory, so anchor it there.
fn hooks_path() -> Option<PathBuf> {
    let p = PathBuf::from(git_capture(&["rev-parse", "--git-path", "hooks"])?);
    Some(if p.is_absolute() {
        p
    } else {
        env::current_dir().ok()?.join(p)
    })
}

fn uninstall() {
    git(&["config", "--global", "--unset", "init.templateDir"]);
    let _ = fs::remove_dir_all(template_dir());
    println!("uninstalled. already-cloned repos keep their shims; delete .git/hooks/* to remove.");
}

/// Append a command to the repo's committed hook config, e.g.
///   git hooks add pre-commit "cargo fmt --check"
fn add(hook: &str, cmd: &str) {
    if !HOOKS.contains(&hook) {
        eprintln!("unknown hook '{hook}'. known: {}", HOOKS.join(", "));
        exit(2);
    }
    let root = repo_root().unwrap_or_else(|| {
        eprintln!("not inside a git repository");
        exit(1);
    });
    let path = root.join(CONFIG_FILE);
    let mut config: toml::Table = fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();

    let hooks = config
        .entry("hooks")
        .or_insert_with(|| toml::Table::new().into())
        .as_table_mut()
        .expect("[hooks] must be a table");
    let list = hooks
        .entry(hook)
        .or_insert_with(|| toml::Value::Array(vec![]))
        .as_array_mut()
        .expect("hook entry must be an array");
    list.push(cmd.into());

    fs::write(&path, toml::to_string(&config).unwrap()).expect("write config");
    println!("added to {CONFIG_FILE}: [{hook}] {cmd}");
    println!("commit the file so others see it. they will be prompted before it runs for them.");
    record_consent(true); // editing the config implies trusting it
}

/// Explicitly accept/decline the current config (also the escape hatch when
/// there was no TTY to prompt on).
fn consent(accept: bool) {
    if repo_root().is_none() || consent_hash().is_none() {
        eprintln!("no {CONFIG_FILE} in this repository");
        exit(1);
    }
    record_consent(accept);
    println!(
        "{} hooks from {CONFIG_FILE} for this repository.",
        if accept { "accepted" } else { "declined" }
    );
}

fn status() {
    let root = repo_root().unwrap_or_else(|| {
        eprintln!("not inside a git repository");
        exit(1);
    });
    match fs::read_to_string(root.join(CONFIG_FILE)) {
        Ok(raw) => {
            print!("{}", render_config(&raw));
            let scripts = githooks_dir_files(&root);
            if !scripts.is_empty() {
                println!("\ncovered scripts under {HOOKS_DIR}/:");
                for f in &scripts {
                    println!("  {}", rel(&root, f));
                }
            }
            let state = match (consent_state(), consent_hash()) {
                (Some(s), Some(h)) if s == format!("accept:{h}") => "accepted",
                (Some(s), Some(h)) if s == format!("decline:{h}") => "declined",
                (Some(_), _) => {
                    "config or .githooks/ changed since your last decision — will re-prompt"
                }
                _ => "no decision yet — will prompt on first hook",
            };
            println!("\nstatus: {state}");
            println!("{}", signature_line(&root));
        }
        Err(_) => println!("no {CONFIG_FILE} in this repository"),
    }
}

/// `git hooks diff`: show what changed since the last accept (the same diff the
/// re-prompt would show), plus the current signature status.
fn diff() {
    let root = repo_root().unwrap_or_else(|| {
        eprintln!("not inside a git repository");
        exit(1);
    });
    if consent_hash().is_none() {
        println!("no {CONFIG_FILE} in this repository");
        return;
    }
    let manifest = read_manifest();
    if manifest.is_empty() {
        println!("nothing accepted yet.");
    } else if consent_state().as_deref()
        == Some(format!("accept:{}", consent_hash().unwrap()).as_str())
    {
        println!("unchanged since your last accept.");
    } else {
        print!("{}", change_prompt_body(&root, &manifest));
    }
    println!("{}", signature_line(&root));
}

/// A single hook command: either a plain string or an inline table
/// `{ run = "...", glob = "*.rs" }`.
struct HookCmd {
    run: String,
    glob: Option<String>,
}

/// Parse one hook-array entry. Accepts a bare string or a `{ run, glob }` table.
fn parse_hook_cmd(v: &toml::Value) -> Option<HookCmd> {
    if let Some(s) = v.as_str() {
        return Some(HookCmd {
            run: s.to_string(),
            glob: None,
        });
    }
    if let Some(t) = v.as_table() {
        let run = t.get("run")?.as_str()?.to_string();
        let glob = t.get("glob").and_then(|g| g.as_str()).map(String::from);
        return Some(HookCmd { run, glob });
    }
    None
}

/// Human-readable rendering of the config for prompts/status. Table-form
/// entries become `run  (glob: …)` so the reader sees the command and its
/// filter at a glance; falls back to the raw text if it doesn't parse.
fn render_config(raw: &str) -> String {
    let Ok(config) = raw.parse::<toml::Table>() else {
        return format!("{}\n", raw.trim());
    };
    let Some(hooks) = config.get("hooks").and_then(|h| h.as_table()) else {
        return format!("{}\n", raw.trim());
    };
    let mut s = String::new();
    for (hook, val) in hooks {
        let Some(arr) = val.as_array() else { continue };
        s.push_str(&format!("[{hook}]\n"));
        for cmd in arr {
            match parse_hook_cmd(cmd) {
                Some(hc) => {
                    s.push_str(&format!("  {}", hc.run));
                    if let Some(g) = hc.glob {
                        s.push_str(&format!("   (glob: {g})"));
                    }
                    s.push('\n');
                }
                None => s.push_str("  <unparseable entry>\n"),
            }
        }
    }
    s
}

/// Called by the shims. Runs the commands for `hook` from the repo's committed
/// {CONFIG_FILE} — but only after the user has accepted this exact version of
/// the hooks. First run (right after clone, via post-checkout) shows the config
/// and prompts on /dev/tty; the decision is stored in .git/config keyed to a
/// content hash of {CONFIG_FILE} *and* the {HOOKS_DIR}/ tree, so any change to
/// either re-prompts.
///
/// `GIT_HOOKS_CONSENT` overrides stored consent for this invocation only (never
/// persisted): `accept` / `decline` unconditionally, or `accept:<hash>` which
/// only takes effect when it matches the current content hash.
fn run_hook(hook: &str, hook_args: &[String]) -> i32 {
    let Some(root) = repo_root() else { return 0 };
    let Ok(raw) = fs::read_to_string(root.join(CONFIG_FILE)) else {
        return 0; // repo hasn't opted in; shims stay inert
    };
    let hash = consent_hash().unwrap();

    // Env override wins over stored consent but is never written back.
    let env_verdict = match env::var("GIT_HOOKS_CONSENT").ok().as_deref() {
        Some("accept") => Some(true),
        Some("decline") => Some(false),
        // Pinned form: only honoured when the hash matches (reproducible CI).
        Some(s) if s.strip_prefix("accept:") == Some(hash.as_str()) => Some(true),
        _ => None, // unset, empty, or accept:<wrong-hash> → fall back to stored
    };
    let proceed = match env_verdict {
        Some(true) => true,
        Some(false) => return 0,
        None => match consent_state().as_deref() {
            Some(s) if s == format!("accept:{hash}") => true,
            Some(s) if s == format!("decline:{hash}") => return 0,
            _ => resolve_new_consent(&root, &raw),
        },
    };
    if !proceed {
        return 0;
    }

    let config: toml::Table = match raw.parse() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("git-hooks: bad {CONFIG_FILE}: {e}");
            return 1;
        }
    };
    let Some(cmds) = config
        .get("hooks")
        .and_then(|h| h.get(hook))
        .and_then(|v| v.as_array())
    else {
        return 0;
    };

    let staged = staged_files(&root);

    for cmd in cmds {
        let Some(hc) = parse_hook_cmd(cmd) else {
            continue;
        };

        // If a glob is set, gate on (and substitute) only the matching staged
        // files. No glob → all staged files are eligible for substitution.
        let matched: Vec<String> = match &hc.glob {
            Some(g) => staged
                .iter()
                .filter(|f| glob_match(g, f))
                .cloned()
                .collect(),
            None => staged.clone(),
        };
        if hc.glob.is_some() && matched.is_empty() {
            eprintln!(
                "git-hooks[{hook}]: skipped (no matching staged files): {}",
                hc.run
            );
            continue;
        }

        let cmd_str = if hc.run.contains("{staged_files}") {
            hc.run
                .replace("{staged_files}", &shell_quote_list(&matched))
        } else {
            hc.run.clone()
        };

        eprintln!("git-hooks[{hook}]: {}", hc.run);
        // sh -c '<cmd>' sh <hook_args...> — git's hook args land in $1, $2...
        // A `.githooks/…` command is just a path sh runs, so scripts and inline
        // commands share this one code path. On Windows we rely on sh being on
        // PATH (git-for-windows puts its bundled sh there for hook execution).
        let status = Command::new("sh")
            .arg("-c")
            .arg(&cmd_str)
            .arg("sh")
            .args(hook_args)
            .current_dir(&root)
            .status()
            .expect("spawn sh");
        if !status.success() {
            eprintln!("git-hooks[{hook}]: FAILED: {}", hc.run);
            return status.code().unwrap_or(1);
        }
    }
    0
}

/// Staged paths (repo-relative), added/copied/modified/renamed. Empty for hooks
/// that fire with nothing staged — callers treat that as "no files", same path.
fn staged_files(root: &Path) -> Vec<String> {
    let out = Command::new("git")
        .args(["diff", "--cached", "--name-only", "--diff-filter=ACMR"])
        .current_dir(root)
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Single-quote a path for `sh`, escaping embedded single quotes.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn shell_quote_list(files: &[String]) -> String {
    files
        .iter()
        .map(|f| shell_quote(f))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Minimal glob matcher, whole-string against a repo-relative path:
///   `*`  matches any run of characters except `/`
///   `**` matches any run of characters including `/`
///   `?`  matches exactly one character (not `/`)
/// everything else is literal. Operates on bytes (fine for path matching).
fn glob_match(pattern: &str, text: &str) -> bool {
    fn m(p: &[u8], t: &[u8]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            b'*' if p.get(1) == Some(&b'*') => {
                // `**`: try consuming any amount of text, including `/`.
                let rest = &p[2..];
                (0..=t.len()).any(|i| m(rest, &t[i..]))
            }
            b'*' => {
                // `*`: consume text up to but not across a `/`.
                let rest = &p[1..];
                let mut i = 0;
                loop {
                    if m(rest, &t[i..]) {
                        return true;
                    }
                    if i >= t.len() || t[i] == b'/' {
                        return false;
                    }
                    i += 1;
                }
            }
            b'?' => !t.is_empty() && t[0] != b'/' && m(&p[1..], &t[1..]),
            c => !t.is_empty() && t[0] == c && m(&p[1..], &t[1..]),
        }
    }
    m(pattern.as_bytes(), text.as_bytes())
}

/// No env override and no stored decision for the current content: resolve via
/// signed trust, then org policy, then an interactive prompt.
///
/// A signature by an already-trusted key auto-accepts with no prompt and no
/// terminal (the whole point — kills prompt fatigue org-wide). A `decline`
/// policy default then skips silently. Otherwise we fall through to the prompt,
/// whose header/choices adapt to the signature status.
fn resolve_new_consent(root: &Path, raw: &str) -> bool {
    let trust = signature_status(root);
    if let Trust::Valid {
        principal,
        fingerprint,
    } = &trust
    {
        if key_trusted(fingerprint) {
            record_consent(true);
            eprintln!(
                "git-hooks: hooks auto-accepted: signed by trusted key {fingerprint} ({principal})"
            );
            return true;
        }
    }
    // Locked-down machines: never prompt, skip untrusted/unsigned repos. Needs
    // no terminal. A trusted signature was already handled above.
    if policy_default_decline() {
        eprintln!(
            "git-hooks: org policy default is `decline` and no trusted signature — skipping hooks."
        );
        return false;
    }
    prompt_consent(root, raw, &trust)
}

/// Ask on the controlling terminal (hook stdin belongs to git; the console
/// device — /dev/tty on unix, CONIN$/CONOUT$ on Windows — is the standard way
/// to reach the user). No console — e.g. CI — means no consent: hooks are
/// skipped and we say how to opt in.
///
/// First-ever prompt shows the full config + covered scripts. If a previous
/// accept exists (a manifest), the hooks *changed*: we show a diff against the
/// accepted version instead of dumping everything again.
///
/// The signature status shapes the prompt: a valid-but-untrusted signature adds
/// a "signed by …" header and a third `[t]rust key` choice (accept AND remember
/// the key), an invalid signature adds a loud warning line, and an unsigned repo
/// keeps the original two-choice prompt byte-for-byte.
fn prompt_consent(root: &Path, raw: &str, trust: &Trust) -> bool {
    let (in_dev, out_dev) = tty_devices();
    let Ok(tty_in) = fs::File::open(in_dev) else {
        eprintln!(
            "git-hooks: this repository defines hooks in {CONFIG_FILE} (not yet accepted).\n\
             git-hooks: no terminal to ask on — skipping them. run `git hooks accept` to enable."
        );
        return false;
    };
    let mut tty_out = fs::OpenOptions::new().write(true).open(out_dev).unwrap();

    let manifest = read_manifest();
    let body = if manifest.is_empty() {
        first_prompt_body(root, raw)
    } else {
        change_prompt_body(root, &manifest)
    };
    let warning = runtime_ref_warning(root);

    let (header, offer_trust, sig_warn) = match trust {
        Trust::Valid {
            principal,
            fingerprint,
        } => (
            format!("signed by {principal}, key {fingerprint}\n\n"),
            true,
            String::new(),
        ),
        Trust::Invalid => (
            String::new(),
            false,
            "signature present but INVALID — do not accept unless you understand why.\n"
                .to_string(),
        ),
        Trust::Unsigned => (String::new(), false, String::new()),
    };
    let choices = if offer_trust {
        "[y]es once / [t]rust key / [N]o "
    } else {
        "accept? [y/N] "
    };

    writeln!(
        tty_out,
        "\n{header}{body}\n\
         these commands will run on your machine during git operations.\n\
         {sig_warn}{warning}{choices}"
    )
    .unwrap();

    let mut answer = String::new();
    BufReader::new(tty_in).read_line(&mut answer).unwrap();
    let (accepted, trust_key) = match answer.trim() {
        "t" | "T" if offer_trust => (true, true),
        "y" | "Y" | "yes" => (true, false),
        _ => (false, false),
    };
    record_consent(accepted);
    if trust_key {
        if let Trust::Valid { fingerprint, .. } = trust {
            add_local_trusted_key(fingerprint);
        }
    }
    writeln!(
        tty_out,
        "{}. change your mind anytime: `git hooks accept` / `git hooks decline`.",
        if accepted { "accepted" } else { "declined" }
    )
    .unwrap();
    accepted
}

/// Console device paths for prompting: /dev/tty on unix, CONIN$/CONOUT$ on
/// Windows (both open like files against the attached console).
#[cfg(windows)]
fn tty_devices() -> (&'static str, &'static str) {
    ("CONIN$", "CONOUT$")
}
#[cfg(not(windows))]
fn tty_devices() -> (&'static str, &'static str) {
    ("/dev/tty", "/dev/tty")
}

/// First contact: show the whole config, plus the scripts it can reach.
fn first_prompt_body(root: &Path, raw: &str) -> String {
    let mut s = format!(
        "this repository wants to run the following hooks ({CONFIG_FILE}):\n\n{}",
        render_config(raw)
    );
    let scripts = githooks_dir_files(root);
    if !scripts.is_empty() {
        s.push_str(&format!("\ncovered scripts under {HOOKS_DIR}/:\n"));
        for f in &scripts {
            s.push_str(&format!("  {}\n", rel(root, f)));
        }
    }
    s
}

/// Re-prompt: the content changed since the last accept. Show what changed as a
/// diff against the accepted blobs, not the whole file.
fn change_prompt_body(root: &Path, manifest: &[(String, String)]) -> String {
    let accepted: BTreeMap<&str, &str> = manifest
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_str()))
        .collect();

    // Currently-relevant files: the toml + everything under .githooks/.
    let mut current: Vec<String> = vec![CONFIG_FILE.to_string()];
    for f in githooks_dir_files(root) {
        current.push(rel(root, &f));
    }

    let mut s = String::from(
        "this repository's hooks CHANGED since you last accepted them. review the changes:\n\n",
    );
    for relpath in &current {
        let path = root.join(relpath);
        let cur = git_capture(&["hash-object", path.to_str().unwrap()]).unwrap_or_default();
        match accepted.get(relpath.as_str()) {
            Some(old) if *old == cur => {} // unchanged, skip
            Some(old) => {
                s.push_str(&format!("--- changed: {relpath} ---\n"));
                s.push_str(&git_diff_blob_file(old, &path));
                s.push('\n');
            }
            None => {
                s.push_str(&format!("--- added: {relpath} ---\n"));
                if let Ok(c) = fs::read_to_string(&path) {
                    s.push_str(c.trim_end());
                    s.push('\n');
                }
            }
        }
    }
    for (relpath, _) in manifest {
        if !current.contains(relpath) {
            s.push_str(&format!("--- removed: {relpath} ---\n"));
        }
    }
    s
}

/// Warn when inline commands reference repo files whose content the consent
/// hash does NOT cover (anything outside {HOOKS_DIR}/). Heuristic: whitespace-
/// split every command token; a token starting with `./` or containing `/`
/// that resolves to an existing repo file (and is not under {HOOKS_DIR}/ or the
/// config itself) is flagged. Intentionally simple — false negatives are
/// acceptable (e.g. a path built by shell substitution won't be caught).
fn runtime_ref_warning(root: &Path) -> String {
    let Ok(raw) = fs::read_to_string(root.join(CONFIG_FILE)) else {
        return String::new();
    };
    let Ok(config) = raw.parse::<toml::Table>() else {
        return String::new();
    };
    let mut refs: BTreeSet<String> = BTreeSet::new();
    if let Some(hooks) = config.get("hooks").and_then(|h| h.as_table()) {
        for val in hooks.values() {
            let Some(arr) = val.as_array() else { continue };
            for cmd in arr {
                let Some(hc) = parse_hook_cmd(cmd) else {
                    continue;
                };
                for tok in hc.run.split_whitespace() {
                    if !(tok.starts_with("./") || tok.contains('/')) {
                        continue;
                    }
                    let cand = tok.trim_start_matches("./");
                    if cand.starts_with(&format!("{HOOKS_DIR}/")) || cand == CONFIG_FILE {
                        continue;
                    }
                    if root.join(cand).is_file() {
                        refs.insert(cand.to_string());
                    }
                }
            }
        }
    }
    if refs.is_empty() {
        return String::new();
    }
    format!(
        "note: these commands reference repo files whose content is NOT covered by this consent:\n  {}\n",
        refs.into_iter().collect::<Vec<_>>().join("\n  ")
    )
}

/// Record the decision, keyed to the current content hash. On accept, also
/// persist the accepted content into git's object db (`hash-object -w`) and a
/// manifest of `path<TAB>blob` lines, so a later change can be shown as a diff.
/// Declines stay cheap: just the hash, manifest cleared.
fn record_consent(accept: bool) {
    let verdict = if accept { "accept" } else { "decline" };
    let hash = consent_hash().expect("config file exists");
    git(&[
        "config",
        "--local",
        "hooks.consent",
        &format!("{verdict}:{hash}"),
    ]);
    // Clear any previous manifest (unset-all fails if the key is absent — fine).
    let _ = git_capture(&["config", "--local", "--unset-all", "hooks.consentManifest"]);
    if accept {
        write_manifest();
    }
}

fn write_manifest() {
    let Some(root) = repo_root() else { return };
    let mut files = vec![root.join(CONFIG_FILE)];
    files.extend(githooks_dir_files(&root));
    for f in files {
        let relpath = rel(&root, &f);
        if let Some(blob) = git_capture(&["hash-object", "-w", f.to_str().unwrap()]) {
            git(&[
                "config",
                "--local",
                "--add",
                "hooks.consentManifest",
                &format!("{relpath}\t{blob}"),
            ]);
        }
    }
}

fn read_manifest() -> Vec<(String, String)> {
    let Some(out) = git_capture(&["config", "--local", "--get-all", "hooks.consentManifest"])
    else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|l| {
            let mut it = l.splitn(2, '\t');
            Some((it.next()?.to_string(), it.next()?.to_string()))
        })
        .collect()
}

fn consent_state() -> Option<String> {
    git_capture(&["config", "--local", "hooks.consent"])
}

/// Content hash covering everything executable: `git hash-object` the config
/// and every file under {HOOKS_DIR}/ (INCLUDING trust/), then hash the whole
/// canonical byte string. Any byte change anywhere flips it. git computes the
/// object hashes for us, no crypto dep. A signature under trust/ therefore
/// re-keys consent — fine, it just means "re-accept the newly-signed content".
fn consent_hash() -> Option<String> {
    let root = repo_root()?;
    if !root.join(CONFIG_FILE).is_file() {
        return None;
    }
    Some(git_hash_stdin(&canonical_content(&root, false)))
}

/// The one deterministic byte string that both consent hashing and signing
/// cover: a `<relpath>:<blob>` line for {CONFIG_FILE}, then one per file under
/// {HOOKS_DIR}/ walked in sorted order, so it captures everything the consent
/// hash covers. `git hash-object` supplies the blob ids (no crypto dep).
///
/// When `exclude_trust` is set, files under {HOOKS_DIR}/trust/ are omitted. The
/// signature is computed over the excluded form because trust/ is where the
/// signature itself lives — signing over its own output would invalidate it the
/// moment it is written (see SECURITY.md). Consent hashing passes `false` and
/// covers trust/ too.
fn canonical_content(root: &Path, exclude_trust: bool) -> String {
    let mut s = String::new();
    let toml = root.join(CONFIG_FILE);
    if let Some(blob) = git_capture(&["hash-object", toml.to_str().unwrap()]) {
        s.push_str(&format!("{CONFIG_FILE}:{blob}\n"));
    }
    let trust_prefix = format!("{HOOKS_DIR}/trust/");
    for f in githooks_dir_files(root) {
        let relpath = rel(root, &f);
        if exclude_trust && relpath.starts_with(&trust_prefix) {
            continue;
        }
        let blob = git_capture(&["hash-object", f.to_str().unwrap()]).unwrap_or_default();
        s.push_str(&format!("{relpath}:{blob}\n"));
    }
    s
}

/// Files under {HOOKS_DIR}/, recursive, sorted for determinism.
fn githooks_dir_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_files(&root.join(HOOKS_DIR), &mut out);
    out.sort();
    out
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_files(&p, out);
        } else if p.is_file() {
            out.push(p);
        }
    }
}

/// Repo-relative path as a string (paths are always under `root` here).
fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

fn repo_root() -> Option<PathBuf> {
    git_capture(&["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

/// Diff the accepted blob against the file's current content. The current
/// content is written to the object db (harmless dangling blob) so `git diff`
/// can resolve both sides.
fn git_diff_blob_file(old_blob: &str, path: &Path) -> String {
    let new_blob = git_capture(&["hash-object", "-w", path.to_str().unwrap()]).unwrap_or_default();
    git_capture(&["--no-pager", "diff", old_blob, &new_blob]).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Signed trust. A maintainer signs the canonical content with an ssh key
// (`sign`); the signature + allowed_signers live under {HOOKS_DIR}/trust/. A
// cloner who trusts the signing key's fingerprint (repo-local `hooks.trustedKey`
// or the org policy file) gets prompt-less auto-accept. Namespace is pinned to
// exactly "git-hooks" so a signature made for another tool cannot be replayed.
// ---------------------------------------------------------------------------

const SIG_NAMESPACE: &str = "git-hooks";

/// Result of checking the repo's signature against its allowed_signers.
enum Trust {
    /// No signature files present — behave exactly as an unsigned repo.
    Unsigned,
    /// Signature files present but nothing verifies (tampered/mismatched).
    Invalid,
    /// A principal in allowed_signers verified the canonical content.
    Valid {
        principal: String,
        fingerprint: String,
    },
}

fn trust_dir(root: &Path) -> PathBuf {
    root.join(HOOKS_DIR).join("trust")
}

/// Verify {HOOKS_DIR}/trust/hooks.sig against allowed_signers over the canonical
/// content (EXCLUDING trust/). Try each principal until one verifies.
fn signature_status(root: &Path) -> Trust {
    let dir = trust_dir(root);
    let sig = dir.join("hooks.sig");
    let signers = dir.join("allowed_signers");
    if !sig.is_file() || !signers.is_file() {
        return Trust::Unsigned;
    }
    let content = canonical_content(root, true);
    for principal in allowed_principals(&signers) {
        if ssh_verify(&signers, &principal, &sig, &content) {
            let fingerprint = signing_fingerprint(&signers, &principal);
            return Trust::Valid {
                principal,
                fingerprint,
            };
        }
    }
    Trust::Invalid
}

/// One human-readable signature line for `status` / `diff`.
fn signature_line(root: &Path) -> String {
    match signature_status(root) {
        Trust::Unsigned => "signature: unsigned".to_string(),
        Trust::Invalid => "signature: present but INVALID".to_string(),
        Trust::Valid {
            principal,
            fingerprint,
        } => {
            let t = if key_trusted(&fingerprint) {
                "trusted"
            } else {
                "untrusted"
            };
            format!("signature: signed by {principal}, key {fingerprint} — valid, {t}")
        }
    }
}

/// Principals from an allowed_signers file: first field of each non-comment,
/// non-empty line, split on `,` (a line may list several principals).
fn allowed_principals(signers: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(signers) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let first = line.split_whitespace().next().unwrap_or("");
        for p in first.split(',') {
            if !p.is_empty() {
                out.push(p.to_string());
            }
        }
    }
    out
}

/// `ssh-keygen -Y verify` with the namespace pinned, feeding the canonical
/// content on stdin. Success (exit 0) means this principal signed this content.
fn ssh_verify(signers: &Path, principal: &str, sig: &Path, content: &str) -> bool {
    let child = Command::new("ssh-keygen")
        .args([
            "-Y",
            "verify",
            "-f",
            signers.to_str().unwrap(),
            "-I",
            principal,
            "-n",
            SIG_NAMESPACE,
            "-s",
            sig.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return false };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(content.as_bytes());
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

/// SHA256 fingerprint of the key that a principal maps to in allowed_signers.
fn signing_fingerprint(signers: &Path, principal: &str) -> String {
    let Ok(text) = fs::read_to_string(signers) else {
        return String::new();
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.splitn(2, char::is_whitespace);
        let first = it.next().unwrap_or("");
        if !first.split(',').any(|p| p == principal) {
            continue;
        }
        let keypart = it.next().unwrap_or("").trim();
        if !keypart.is_empty() {
            return fingerprint_of_pubkey(keypart);
        }
    }
    String::new()
}

/// SHA256 fingerprint of a public key line (`<keytype> <base64> [comment]`) via
/// `ssh-keygen -lf -`. Output is `<bits> SHA256:… <comment> (<TYPE>)`; the
/// fingerprint is the second whitespace field.
fn fingerprint_of_pubkey(pubkey: &str) -> String {
    let child = Command::new("ssh-keygen")
        .args(["-lf", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return String::new();
    };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(pubkey.as_bytes());
        let _ = si.write_all(b"\n");
    }
    let Ok(out) = child.wait_with_output() else {
        return String::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .nth(1)
        .unwrap_or("")
        .to_string()
}

/// Is this fingerprint trusted? Org policy first, then repo-local config.
fn key_trusted(fingerprint: &str) -> bool {
    if fingerprint.is_empty() {
        return false;
    }
    policy_trusted_keys().iter().any(|k| k == fingerprint)
        || local_trusted_keys().iter().any(|k| k == fingerprint)
}

fn local_trusted_keys() -> Vec<String> {
    git_capture(&["config", "--local", "--get-all", "hooks.trustedKey"])
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

fn add_local_trusted_key(fingerprint: &str) {
    if !local_trusted_keys().iter().any(|k| k == fingerprint) {
        git(&[
            "config",
            "--local",
            "--add",
            "hooks.trustedKey",
            fingerprint,
        ]);
    }
}

// --- org policy file: ~/.config/git-hooks/policy.toml ---------------------

fn policy_path() -> PathBuf {
    home().join(".config/git-hooks/policy.toml")
}

fn policy() -> toml::Table {
    fs::read_to_string(policy_path())
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default()
}

fn policy_trusted_keys() -> Vec<String> {
    policy()
        .get("trusted_keys")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn policy_default_decline() -> bool {
    policy().get("default").and_then(|v| v.as_str()) == Some("decline")
}

fn write_policy(p: &toml::Table) {
    let path = policy_path();
    fs::create_dir_all(path.parent().unwrap()).expect("create policy dir");
    fs::write(&path, toml::to_string(p).unwrap()).expect("write policy");
}

// --- commands -------------------------------------------------------------

/// `git hooks sign --key <path> [--signer <principal>]`: sign the canonical
/// content (EXCLUDING trust/) with an ssh key, writing the armored signature to
/// {HOOKS_DIR}/trust/hooks.sig and ensuring the signer's public key is in
/// {HOOKS_DIR}/trust/allowed_signers. Prints the key fingerprint and a reminder.
fn sign(args: &[String]) {
    let mut key: Option<String> = None;
    let mut signer: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--key" => {
                key = args.get(i + 1).cloned();
                i += 2;
            }
            "--signer" => {
                signer = args.get(i + 1).cloned();
                i += 2;
            }
            _ => usage(),
        }
    }
    let key = key.unwrap_or_else(|| usage());
    let root = repo_root().unwrap_or_else(|| {
        eprintln!("not inside a git repository");
        exit(1);
    });
    if !root.join(CONFIG_FILE).is_file() {
        eprintln!("no {CONFIG_FILE} to sign in this repository");
        exit(1);
    }

    // Sign the exact content the cloner will verify (trust/ excluded so writing
    // the signature below does not invalidate it).
    let content = canonical_content(&root, true);
    let signature = ssh_sign(&key, &content);

    let dir = trust_dir(&root);
    fs::create_dir_all(&dir).expect("create trust dir");
    fs::write(dir.join("hooks.sig"), &signature).expect("write signature");

    let principal = signer
        .or_else(|| git_capture(&["config", "user.email"]))
        .unwrap_or_else(|| {
            eprintln!("no --signer given and git user.email is unset");
            exit(1);
        });
    let pubkey = ssh_capture(&["-y", "-f", &key]).unwrap_or_else(|| {
        eprintln!("could not read public key from {key}");
        exit(1);
    });
    let line = format!("{principal} {pubkey}");
    let signers_path = dir.join("allowed_signers");
    let existing = fs::read_to_string(&signers_path).unwrap_or_default();
    if !existing.lines().any(|l| l.trim() == line.trim()) {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&signers_path)
            .expect("open allowed_signers");
        writeln!(f, "{line}").expect("append allowed_signers");
    }

    let fingerprint = ssh_capture(&["-lf", &key])
        .and_then(|s| s.split_whitespace().nth(1).map(String::from))
        .unwrap_or_default();
    println!("signed {CONFIG_FILE} + {HOOKS_DIR}/ (excluding trust/) as {principal}");
    println!("signing key fingerprint: {fingerprint}");
    println!("commit {HOOKS_DIR}/trust/ so cloners who trust this key are never prompted.");
}

/// `git hooks trust <fingerprint> [--global]`.
fn trust_cmd(args: &[String]) {
    let (fingerprint, global) = parse_trust_args(args);
    if global {
        let mut p = policy();
        let arr = p
            .entry("trusted_keys")
            .or_insert_with(|| toml::Value::Array(vec![]))
            .as_array_mut()
            .expect("trusted_keys must be an array");
        if !arr.iter().any(|v| v.as_str() == Some(fingerprint.as_str())) {
            arr.push(fingerprint.clone().into());
        }
        write_policy(&p);
        println!(
            "globally trusted signing key {fingerprint} ({})",
            policy_path().display()
        );
    } else {
        if repo_root().is_none() {
            eprintln!("not inside a git repository (use --global for the org policy file)");
            exit(1);
        }
        add_local_trusted_key(&fingerprint);
        println!("trusted signing key {fingerprint} for this repository.");
    }
}

/// `git hooks untrust <fingerprint> [--global]`.
fn untrust_cmd(args: &[String]) {
    let (fingerprint, global) = parse_trust_args(args);
    if global {
        let mut p = policy();
        if let Some(arr) = p.get_mut("trusted_keys").and_then(|v| v.as_array_mut()) {
            arr.retain(|v| v.as_str() != Some(fingerprint.as_str()));
        }
        write_policy(&p);
        println!("removed {fingerprint} from global trust.");
    } else {
        if repo_root().is_none() {
            eprintln!("not inside a git repository (use --global for the org policy file)");
            exit(1);
        }
        // Rewrite the multi-valued key without the removed fingerprint (avoids
        // regex-escaping the SHA256 value for `--unset`).
        let remaining: Vec<String> = local_trusted_keys()
            .into_iter()
            .filter(|k| k != &fingerprint)
            .collect();
        let _ = git_capture(&["config", "--local", "--unset-all", "hooks.trustedKey"]);
        for k in &remaining {
            git(&["config", "--local", "--add", "hooks.trustedKey", k]);
        }
        println!("untrusted {fingerprint} for this repository.");
    }
}

/// Parse `<fingerprint> [--global]` in any order.
fn parse_trust_args(args: &[String]) -> (String, bool) {
    let mut fingerprint: Option<String> = None;
    let mut global = false;
    for a in args {
        if a == "--global" {
            global = true;
        } else if fingerprint.is_none() {
            fingerprint = Some(a.clone());
        } else {
            usage();
        }
    }
    (fingerprint.unwrap_or_else(|| usage()), global)
}

/// `ssh-keygen -Y sign -f <key> -n git-hooks`, content on stdin → armored
/// signature on stdout.
fn ssh_sign(key: &str, content: &str) -> String {
    let child = Command::new("ssh-keygen")
        .args(["-Y", "sign", "-f", key, "-n", SIG_NAMESPACE])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = child.unwrap_or_else(|e| {
        eprintln!("could not run ssh-keygen: {e}");
        exit(1);
    });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(content.as_bytes())
        .expect("write to ssh-keygen");
    let out = child.wait_with_output().expect("wait ssh-keygen");
    if !out.status.success() {
        eprintln!(
            "ssh-keygen sign failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        exit(1);
    }
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn ssh_capture(args: &[&str]) -> Option<String> {
    let out = Command::new("ssh-keygen").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_hash_stdin(data: &str) -> String {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn git hash-object");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(data.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_capture(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git(args: &[&str]) {
    let status = Command::new("git").args(args).status().expect("run git");
    if !status.success() {
        eprintln!("git-hooks: `git {}` failed", args.join(" "));
        exit(1);
    }
}
