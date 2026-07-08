use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};

// Hooks worth shimming. Add more from githooks(5) if ever needed.
const HOOKS: &[&str] = &[
    "pre-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "pre-push",
    "post-checkout",
    "post-merge",
    "pre-rebase",
];

const CONFIG_FILE: &str = ".githooks.toml";
// Committed directory of hook scripts. A command in {CONFIG_FILE} that starts
// with this prefix runs the named script (still via `sh -c`, so args and shell
// substitution keep working). Everything under it is hash-covered.
const HOOKS_DIR: &str = ".githooks";
// Dir component of the consent hash when there is no {HOOKS_DIR}.
const NO_DIR: &str = "none";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("install") => install(),
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

// ponytail: $HOME only — Windows needs USERPROFILE, add when someone runs it there.
fn template_dir() -> PathBuf {
    PathBuf::from(env::var("HOME").expect("HOME not set"))
        .join(".config/git-hooks/template")
}

/// One-time global setup, LFS-style: write shim hooks into a template dir and
/// point init.templateDir at it. Every future `git clone`/`git init` copies
/// the shims into .git/hooks automatically.
fn install() {
    let hooks_dir = template_dir().join("hooks");
    fs::create_dir_all(&hooks_dir).expect("create template dir");

    for hook in HOOKS {
        let shim = format!("#!/bin/sh\nexec git-hooks run {hook} \"$@\"\n");
        let path = hooks_dir.join(hook);
        fs::write(&path, shim).expect("write shim");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    git(&[
        "config",
        "--global",
        "init.templateDir",
        template_dir().to_str().unwrap(),
    ]);
    println!("installed. new clones get hook shims automatically.");
    println!("existing repo? run `git init` inside it to copy the shims.");
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
            print!("{raw}");
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

/// Called by the shims. Runs the commands for `hook` from the repo's committed
/// {CONFIG_FILE} — but only after the user has accepted this exact version of
/// the hooks. First run (right after clone, via post-checkout) shows the config
/// and prompts on /dev/tty; the decision is stored in .git/config keyed to a
/// content hash of {CONFIG_FILE} *and* the {HOOKS_DIR}/ tree, so any change to
/// either re-prompts.
fn run_hook(hook: &str, hook_args: &[String]) -> i32 {
    let Some(root) = repo_root() else { return 0 };
    let Ok(raw) = fs::read_to_string(root.join(CONFIG_FILE)) else {
        return 0; // repo hasn't opted in; shims stay inert
    };
    let hash = consent_hash().unwrap();

    match consent_state().as_deref() {
        Some(s) if s == format!("accept:{hash}") => {}
        Some(s) if s == format!("decline:{hash}") => return 0,
        _ => {
            if !prompt_consent(&root, &raw) {
                return 0;
            }
        }
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

    for cmd in cmds {
        let Some(cmd) = cmd.as_str() else { continue };
        eprintln!("git-hooks[{hook}]: {cmd}");
        // sh -c '<cmd>' sh <hook_args...> — git's hook args land in $1, $2...
        // A `.githooks/…` command is just a path sh runs, so scripts and inline
        // commands share this one code path.
        let status = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .arg("sh")
            .args(hook_args)
            .current_dir(&root)
            .status()
            .expect("spawn sh");
        if !status.success() {
            eprintln!("git-hooks[{hook}]: FAILED: {cmd}");
            return status.code().unwrap_or(1);
        }
    }
    0
}

/// Ask on the controlling terminal (hook stdin belongs to git, /dev/tty is the
/// standard way to reach the user). No TTY — e.g. CI — means no consent: hooks
/// are skipped and we say how to opt in.
///
/// First-ever prompt shows the full config + covered scripts. If a previous
/// accept exists (a manifest), the hooks *changed*: we show a diff against the
/// accepted version instead of dumping everything again.
fn prompt_consent(root: &Path, raw: &str) -> bool {
    let Ok(tty_in) = fs::File::open("/dev/tty") else {
        eprintln!(
            "git-hooks: this repository defines hooks in {CONFIG_FILE} (not yet accepted).\n\
             git-hooks: no terminal to ask on — skipping them. run `git hooks accept` to enable."
        );
        return false;
    };
    let mut tty_out = fs::OpenOptions::new().write(true).open("/dev/tty").unwrap();

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

/// First contact: show the whole config, plus the scripts it can reach.
fn first_prompt_body(root: &Path, raw: &str) -> String {
    let mut s = format!(
        "this repository wants to run the following hooks ({CONFIG_FILE}):\n\n{}\n",
        raw.trim()
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
                let Some(cmd) = cmd.as_str() else { continue };
                for tok in cmd.split_whitespace() {
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
