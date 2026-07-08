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
// Dir component of the consent hash when there is no {HOOKS_DIR}.
const NO_DIR: &str = "none";
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
         run <hook> [args...]     (called by the shims)\n\
         \n\
         hooks are inline commands in {CONFIG_FILE}, or scripts committed under\n\
         {HOOKS_DIR}/ (reference them with a command like `{HOOKS_DIR}/check.sh`).\n\
         consent is keyed to the content of both; any byte change re-prompts."
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
        }
        Err(_) => println!("no {CONFIG_FILE} in this repository"),
    }
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
            _ => prompt_consent(&root, &raw),
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
        let Some(hc) = parse_hook_cmd(cmd) else { continue };

        // If a glob is set, gate on (and substitute) only the matching staged
        // files. No glob → all staged files are eligible for substitution.
        let matched: Vec<String> = match &hc.glob {
            Some(g) => staged.iter().filter(|f| glob_match(g, f)).cloned().collect(),
            None => staged.clone(),
        };
        if hc.glob.is_some() && matched.is_empty() {
            eprintln!("git-hooks[{hook}]: skipped (no matching staged files): {}", hc.run);
            continue;
        }

        let cmd_str = if hc.run.contains("{staged_files}") {
            hc.run.replace("{staged_files}", &shell_quote_list(&matched))
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

/// Ask on the controlling terminal (hook stdin belongs to git; the console
/// device — /dev/tty on unix, CONIN$/CONOUT$ on Windows — is the standard way
/// to reach the user). No console — e.g. CI — means no consent: hooks are
/// skipped and we say how to opt in.
///
/// First-ever prompt shows the full config + covered scripts. If a previous
/// accept exists (a manifest), the hooks *changed*: we show a diff against the
/// accepted version instead of dumping everything again.
fn prompt_consent(root: &Path, raw: &str) -> bool {
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

    writeln!(
        tty_out,
        "\n{body}\n\
         these commands will run on your machine during git operations.\n\
         {warning}accept? [y/N] "
    )
    .unwrap();

    let mut answer = String::new();
    BufReader::new(tty_in).read_line(&mut answer).unwrap();
    let accepted = matches!(answer.trim(), "y" | "Y" | "yes");
    record_consent(accepted);
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
    let accepted: BTreeMap<&str, &str> =
        manifest.iter().map(|(p, b)| (p.as_str(), b.as_str())).collect();

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
                let Some(hc) = parse_hook_cmd(cmd) else { continue };
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

/// Content hash covering everything executable: `git hash-object` of the config
/// combined with a deterministic hash of the {HOOKS_DIR}/ tree. Any byte change
/// in either flips it. git computes the object hashes for us, no crypto dep.
fn consent_hash() -> Option<String> {
    let root = repo_root()?;
    let toml = git_capture(&["hash-object", root.join(CONFIG_FILE).to_str().unwrap()])?;
    Some(format!("{toml}-{}", githooks_dir_hash(&root)))
}

/// Deterministic hash of the working-tree contents of {HOOKS_DIR}/ (not the
/// index): walk recursively in sorted order, `git hash-object` each file, then
/// hash the concatenated "path:blob" lines. No dir → a fixed sentinel.
fn githooks_dir_hash(root: &Path) -> String {
    let files = githooks_dir_files(root);
    if files.is_empty() {
        return NO_DIR.to_string();
    }
    let mut manifest = String::new();
    for f in &files {
        let blob = git_capture(&["hash-object", f.to_str().unwrap()]).unwrap_or_default();
        manifest.push_str(&format!("{}:{}\n", rel(root, f), blob));
    }
    git_hash_stdin(&manifest)
}

/// Files under {HOOKS_DIR}/, recursive, sorted for determinism.
fn githooks_dir_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_files(&root.join(HOOKS_DIR), &mut out);
    out.sort();
    out
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
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
