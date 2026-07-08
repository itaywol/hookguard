use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{exit, Command};

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
         run <hook> [args...]     (called by the shims)"
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
    if repo_root().is_none() || config_hash().is_none() {
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
            let state = match (consent_state(), config_hash()) {
                (Some(s), Some(h)) if s == format!("accept:{h}") => "accepted",
                (Some(s), Some(h)) if s == format!("decline:{h}") => "declined",
                (Some(_), _) => "config changed since your last decision — will re-prompt",
                _ => "no decision yet — will prompt on first hook",
            };
            println!("\nstatus: {state}");
        }
        Err(_) => println!("no {CONFIG_FILE} in this repository"),
    }
}

/// Called by the shims. Runs the commands for `hook` from the repo's committed
/// {CONFIG_FILE} — but only after the user has accepted this exact version of
/// the file. First run (right after clone, via post-checkout) shows the config
/// and prompts on /dev/tty; the decision is stored in .git/config keyed to the
/// file's git hash, so any change to the file re-prompts.
fn run_hook(hook: &str, hook_args: &[String]) -> i32 {
    let Some(root) = repo_root() else { return 0 };
    let Ok(raw) = fs::read_to_string(root.join(CONFIG_FILE)) else {
        return 0; // repo hasn't opted in; shims stay inert
    };
    let hash = config_hash().unwrap();

    match consent_state().as_deref() {
        Some(s) if s == format!("accept:{hash}") => {}
        Some(s) if s == format!("decline:{hash}") => return 0,
        _ => {
            if !prompt_consent(&raw) {
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

/// Show the committed config and ask on the controlling terminal (hook stdin
/// belongs to git, /dev/tty is the standard way to reach the user). No TTY —
/// e.g. CI — means no consent: hooks are skipped and we say how to opt in.
fn prompt_consent(raw: &str) -> bool {
    let Ok(tty_in) = fs::File::open("/dev/tty") else {
        eprintln!(
            "git-hooks: this repository defines hooks in {CONFIG_FILE} (not yet accepted).\n\
             git-hooks: no terminal to ask on — skipping them. run `git hooks accept` to enable."
        );
        return false;
    };
    let mut tty_out = fs::OpenOptions::new().write(true).open("/dev/tty").unwrap();

    writeln!(
        tty_out,
        "\nthis repository wants to run the following hooks ({CONFIG_FILE}):\n\n{}\n\
         these commands will run on your machine during git operations.\n\
         accept? [y/N] ",
        raw.trim()
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

fn record_consent(accept: bool) {
    let verdict = if accept { "accept" } else { "decline" };
    let hash = config_hash().expect("config file exists");
    git(&[
        "config",
        "--local",
        "hooks.consent",
        &format!("{verdict}:{hash}"),
    ]);
}

fn consent_state() -> Option<String> {
    git_capture(&["config", "--local", "hooks.consent"])
}

/// Content hash of the committed config — git computes it for us, no crypto dep.
fn config_hash() -> Option<String> {
    let root = repo_root()?;
    git_capture(&[
        "hash-object",
        root.join(CONFIG_FILE).to_str().unwrap(),
    ])
}

fn repo_root() -> Option<PathBuf> {
    git_capture(&["rev-parse", "--show-toplevel"]).map(PathBuf::from)
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
