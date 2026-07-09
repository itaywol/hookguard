//! End-to-end tests. Plain std::process::Command, no dev-dependencies.
//!
//! Each test builds an isolated sandbox under the system temp dir with its own
//! fake HOME (so `git config --global` and our template dir stay contained) and
//! a PATH that finds the freshly built binary. Every git/git-hooks invocation
//! runs under `setsid -w`, which detaches the controlling terminal: opening
//! /dev/tty then fails, so the consent prompt is never interactive and tests
//! are deterministic. Explicit `git hooks accept`/`decline` drive consent.
//!
//! Linux-only: the sandbox depends on `setsid -w` to drop the controlling
//! terminal, which isn't available by default on macOS/Windows. CI on those
//! platforms just does `cargo build` + `cargo test` (this file compiles away).
#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_git-hooks");
static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    bindir: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("githooks-e2e-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let bindir = PathBuf::from(BIN).parent().unwrap().to_path_buf();
        let sb = Sandbox { root, home, bindir };
        sb.git(&["config", "--global", "user.email", "test@example.com"]);
        sb.git(&["config", "--global", "user.name", "Test"]);
        sb.git(&["config", "--global", "init.defaultBranch", "main"]);
        sb
    }

    /// Run `program args...` under setsid (no controlling terminal), with the
    /// fake HOME and a PATH that finds our binary. cwd defaults to HOME.
    fn run_in(&self, program: &str, args: &[&str], cwd: &Path) -> Output {
        let path = format!(
            "{}:{}",
            self.bindir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new("setsid")
            .arg("-w")
            .arg(program)
            .args(args)
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("PATH", path)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .output()
            .expect("spawn setsid")
    }

    fn git(&self, args: &[&str]) -> Output {
        self.run_in("git", args, &self.home)
    }

    fn git_in(&self, args: &[&str], cwd: &Path) -> Output {
        self.run_in("git", args, cwd)
    }

    fn hooks_in(&self, args: &[&str], cwd: &Path) -> Output {
        self.run_in("git-hooks", args, cwd)
    }

    /// Like `run_in`, but with extra environment variables (e.g.
    /// GIT_HOOKS_CONSENT). Same isolated HOME/PATH and detached TTY.
    fn run_in_env(&self, program: &str, args: &[&str], cwd: &Path, extra: &[(&str, &str)]) -> Output {
        let path = format!(
            "{}:{}",
            self.bindir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new("setsid");
        cmd.arg("-w")
            .arg(program)
            .args(args)
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("PATH", path)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null());
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.output().expect("spawn setsid")
    }

    fn hooks_in_env(&self, args: &[&str], cwd: &Path, extra: &[(&str, &str)]) -> Output {
        self.run_in_env("git-hooks", args, cwd, extra)
    }

    /// Global install: shims + init.templateDir.
    fn install(&self) {
        let out = self.hooks_in(&["install"], &self.home);
        assert!(out.status.success(), "install failed: {}", err(&out));
    }

    /// Build a committed upstream repo: write .githooks.toml + scripts, commit
    /// with --no-verify so its own shims don't interfere.
    fn make_origin(&self, toml: &str, scripts: &[(&str, &str)]) -> PathBuf {
        let origin = self.root.join("origin");
        fs::create_dir_all(&origin).unwrap();
        self.git_in(&["init"], &origin);
        fs::write(origin.join(".githooks.toml"), toml).unwrap();
        for (name, body) in scripts {
            write_script(&origin.join(name), body);
        }
        self.git_in(&["add", "-A"], &origin);
        let c = self.git_in(&["commit", "-m", "init", "--no-verify"], &origin);
        assert!(c.status.success(), "origin commit failed: {}", err(&c));
        origin
    }

    fn clone(&self, origin: &Path, name: &str) -> PathBuf {
        let dest = self.root.join(name);
        let out = self.git_in(
            &["clone", origin.to_str().unwrap(), dest.to_str().unwrap()],
            &self.root,
        );
        assert!(out.status.success(), "clone failed: {}", err(&out));
        dest
    }

    /// Stage a file change so the next commit has something to commit.
    fn stage(&self, work: &Path, fname: &str, content: &str) {
        fs::write(work.join(fname), content).unwrap();
        self.git_in(&["add", fname], work);
    }

    /// Generate a throwaway ed25519 signing key in the sandbox.
    fn keygen(&self) -> PathBuf {
        let key = self.root.join("signing_key");
        let o = self.run_in(
            "ssh-keygen",
            &["-t", "ed25519", "-N", "", "-f", key.to_str().unwrap(), "-q"],
            &self.home,
        );
        assert!(o.status.success(), "keygen failed: {}", err(&o));
        key
    }

    /// SHA256 fingerprint of a key (the `SHA256:…` field of `ssh-keygen -lf`).
    fn fingerprint(&self, key: &Path) -> String {
        let o = self.run_in("ssh-keygen", &["-lf", key.to_str().unwrap()], &self.home);
        assert!(o.status.success(), "fingerprint failed: {}", err(&o));
        out(&o).split_whitespace().nth(1).unwrap().to_string()
    }

    /// Sign the origin repo's hooks with `key` and commit the trust/ dir.
    fn sign_origin(&self, origin: &Path, key: &Path, signer: &str) {
        let o = self.hooks_in(
            &["sign", "--key", key.to_str().unwrap(), "--signer", signer],
            origin,
        );
        assert!(o.status.success(), "sign failed: {}", err(&o));
        self.git_in(&["add", "-A"], origin);
        let c = self.git_in(&["commit", "-m", "sign", "--no-verify"], origin);
        assert!(c.status.success(), "sign commit failed: {}", err(&c));
    }

    /// Write the org policy file into the fake HOME.
    fn write_policy(&self, body: &str) {
        let dir = self.home.join(".config/git-hooks");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("policy.toml"), body).unwrap();
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn write_script(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    // Scripts run via `sh -c '.githooks/x.sh'`, so they need the exec bit.
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

// install -> clone -> no TTY -> hooks skipped, commit passes.
#[test]
fn clone_no_tty_skips_and_commit_passes() {
    let sb = Sandbox::new();
    sb.install();
    // A hook that WOULD fail, to prove it never ran.
    let origin = sb.make_origin("[hooks]\npre-commit = [\"exit 1\"]\n", &[]);
    let work = sb.clone(&origin, "work");

    sb.stage(&work, "file.txt", "hello\n");
    let commit = sb.git_in(&["commit", "-m", "change"], &work);

    assert!(
        commit.status.success(),
        "commit should pass when hooks are skipped: {}",
        err(&commit)
    );
    assert!(
        err(&commit).contains("skipping"),
        "expected skip notice, got: {}",
        err(&commit)
    );
}

// explicit accept -> failing hook blocks commit.
#[test]
fn accept_then_failing_hook_blocks_commit() {
    let sb = Sandbox::new();
    sb.install();
    let origin = sb.make_origin("[hooks]\npre-commit = [\"exit 1\"]\n", &[]);
    let work = sb.clone(&origin, "work");

    let acc = sb.hooks_in(&["accept"], &work);
    assert!(acc.status.success(), "accept failed: {}", err(&acc));

    sb.stage(&work, "file.txt", "hello\n");
    let commit = sb.git_in(&["commit", "-m", "change"], &work);

    assert!(
        !commit.status.success(),
        "commit should be blocked by failing hook"
    );
    assert!(
        err(&commit).contains("FAILED"),
        "expected FAILED notice, got: {}",
        err(&commit)
    );
}

// .githooks.toml change -> consent invalidated (status reports it, hook skipped
// without TTY).
#[test]
fn toml_change_invalidates_consent() {
    let sb = Sandbox::new();
    sb.install();
    let origin = sb.make_origin("[hooks]\npre-commit = [\"touch ran.marker\"]\n", &[]);
    let work = sb.clone(&origin, "work");

    sb.hooks_in(&["accept"], &work);
    sb.stage(&work, "a.txt", "1\n");
    let c1 = sb.git_in(&["commit", "-m", "c1"], &work);
    assert!(c1.status.success(), "c1 failed: {}", err(&c1));
    assert!(work.join("ran.marker").exists(), "hook should have run on c1");

    // Change the toml — consent must no longer match.
    fs::write(
        work.join(".githooks.toml"),
        "[hooks]\npre-commit = [\"touch ran.marker\", \"true\"]\n",
    )
    .unwrap();

    let st = sb.hooks_in(&["status"], &work);
    assert!(
        out(&st).contains("changed") && out(&st).contains("re-prompt"),
        "status should report the change: {}",
        out(&st)
    );

    fs::remove_file(work.join("ran.marker")).unwrap();
    sb.stage(&work, "b.txt", "2\n");
    let c2 = sb.git_in(&["commit", "-m", "c2"], &work);
    assert!(c2.status.success(), "c2 should pass (hook skipped): {}", err(&c2));
    assert!(
        !work.join("ran.marker").exists(),
        "hook must be skipped after toml change without TTY"
    );
}

// .githooks/ script change (toml untouched) -> consent invalidated. THE M1 point.
#[test]
fn script_change_invalidates_consent() {
    let sb = Sandbox::new();
    sb.install();
    let origin = sb.make_origin(
        "[hooks]\npre-commit = [\".githooks/hook.sh\"]\n",
        &[(".githooks/hook.sh", "#!/bin/sh\ntouch ran.marker\n")],
    );
    let work = sb.clone(&origin, "work");

    sb.hooks_in(&["accept"], &work);
    sb.stage(&work, "a.txt", "1\n");
    let c1 = sb.git_in(&["commit", "-m", "c1"], &work);
    assert!(c1.status.success(), "c1 failed: {}", err(&c1));
    assert!(work.join("ran.marker").exists(), "script should have run on c1");

    // Change ONLY the script; the toml is untouched.
    write_script(
        &work.join(".githooks/hook.sh"),
        "#!/bin/sh\n# edited\ntouch ran.marker\n",
    );

    let st = sb.hooks_in(&["status"], &work);
    assert!(
        out(&st).contains("changed") && out(&st).contains("re-prompt"),
        "status should report the script change: {}",
        out(&st)
    );

    fs::remove_file(work.join("ran.marker")).unwrap();
    sb.stage(&work, "b.txt", "2\n");
    let c2 = sb.git_in(&["commit", "-m", "c2"], &work);
    assert!(c2.status.success(), "c2 should pass (hook skipped): {}", err(&c2));
    assert!(
        !work.join("ran.marker").exists(),
        "script must be skipped after .githooks/ change without TTY"
    );
}

// decline -> silent skip.
#[test]
fn decline_silently_skips() {
    let sb = Sandbox::new();
    sb.install();
    let origin = sb.make_origin("[hooks]\npre-commit = [\"exit 1\"]\n", &[]);
    let work = sb.clone(&origin, "work");

    let dec = sb.hooks_in(&["decline"], &work);
    assert!(dec.status.success(), "decline failed: {}", err(&dec));

    sb.stage(&work, "file.txt", "hello\n");
    let commit = sb.git_in(&["commit", "-m", "change"], &work);

    assert!(
        commit.status.success(),
        "declined hooks must not block commit: {}",
        err(&commit)
    );
    assert!(
        !err(&commit).contains("FAILED"),
        "declined hook must not run: {}",
        err(&commit)
    );
}

// `git hooks add` auto-accepts for the author.
#[test]
fn add_auto_accepts_for_author() {
    let sb = Sandbox::new();
    let repo = sb.root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    sb.git_in(&["init"], &repo);

    let a = sb.hooks_in(&["add", "pre-commit", "true"], &repo);
    assert!(a.status.success(), "add failed: {}", err(&a));

    let st = sb.hooks_in(&["status"], &repo);
    assert!(
        out(&st).contains("accepted"),
        "author should be auto-accepted: {}",
        out(&st)
    );
}

// A script under .githooks/ executes and its exit code propagates.
#[test]
fn script_executes_and_exit_code_propagates() {
    let sb = Sandbox::new();
    let repo = sb.root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    sb.git_in(&["init"], &repo);

    fs::write(
        repo.join(".githooks.toml"),
        "[hooks]\npre-commit = [\".githooks/boom.sh\"]\n",
    )
    .unwrap();
    write_script(
        &repo.join(".githooks/boom.sh"),
        "#!/bin/sh\necho SCRIPT_RAN\nexit 7\n",
    );

    let acc = sb.hooks_in(&["accept"], &repo);
    assert!(acc.status.success(), "accept failed: {}", err(&acc));

    // Invoke the runner directly to observe the propagated exit code.
    let run = sb.hooks_in(&["run", "pre-commit"], &repo);
    assert_eq!(
        run.status.code(),
        Some(7),
        "script exit code should propagate; stderr: {}",
        err(&run)
    );
    assert!(
        out(&run).contains("SCRIPT_RAN"),
        "script stdout should surface: {} / {}",
        out(&run),
        err(&run)
    );
}

// `git hooks init` copies shims into an already-cloned repo but never clobbers
// a foreign hook.
#[test]
fn init_adopts_shims_and_refuses_to_clobber() {
    let sb = Sandbox::new();
    let repo = sb.root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    // init BEFORE install, so no templateDir is set and the repo has no shims.
    sb.git_in(&["init"], &repo);
    sb.install();

    // A foreign pre-commit hook that must be left untouched.
    let hooks = repo.join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    let foreign = "#!/bin/sh\necho FOREIGN\n";
    write_script(&hooks.join("pre-commit"), foreign);

    let o = sb.hooks_in(&["init"], &repo);
    assert!(o.status.success(), "init failed: {}", err(&o));

    // pre-commit is still the foreign hook…
    assert_eq!(
        fs::read_to_string(hooks.join("pre-commit")).unwrap(),
        foreign,
        "foreign pre-commit must not be overwritten"
    );
    assert!(
        out(&o).contains("skipped") && out(&o).contains("pre-commit"),
        "init should report the skip: {}",
        out(&o)
    );
    // …but a hook with no conflict got our shim.
    let cm = fs::read_to_string(hooks.join("commit-msg")).unwrap();
    assert!(
        cm.contains("git-hooks shim"),
        "commit-msg should be our shim: {cm}"
    );
}

// glob + {staged_files}: runs only when a matching file is staged, and the
// command receives the staged filename; skipped when only non-matching files
// are staged.
#[test]
fn glob_and_staged_files_substitution() {
    let sb = Sandbox::new();
    sb.install();
    let toml = "[hooks]\npre-commit = [{ run = \"echo {staged_files} > got.txt\", glob = \"*.rs\" }]\n";
    let origin = sb.make_origin(toml, &[]);
    let work = sb.clone(&origin, "work");
    sb.hooks_in(&["accept"], &work);

    // Stage a .rs file → the command runs and receives it.
    sb.stage(&work, "lib.rs", "fn main() {}\n");
    let c1 = sb.git_in(&["commit", "-m", "rs"], &work);
    assert!(c1.status.success(), "rs commit failed: {}", err(&c1));
    let got = fs::read_to_string(work.join("got.txt")).expect("hook should have run");
    assert!(
        got.contains("lib.rs"),
        "command should receive the staged .rs file: {got}"
    );

    // Stage only a .md file → glob has no match, command is skipped.
    fs::remove_file(work.join("got.txt")).unwrap();
    sb.stage(&work, "README.md", "hi\n");
    let c2 = sb.git_in(&["commit", "-m", "md"], &work);
    assert!(c2.status.success(), "md commit should pass: {}", err(&c2));
    assert!(
        !work.join("got.txt").exists(),
        "command must be skipped when no staged file matches the glob"
    );
    assert!(
        err(&c2).contains("no matching staged files"),
        "expected skip notice: {}",
        err(&c2)
    );
}

// GIT_HOOKS_CONSENT overrides stored consent for one invocation, is not
// persisted, and the pinned form only applies on a matching hash.
#[test]
fn env_consent_override() {
    let sb = Sandbox::new();
    sb.install();
    let origin = sb.make_origin("[hooks]\npre-commit = [\"touch ran.marker\"]\n", &[]);
    let work = sb.clone(&origin, "work");
    let marker = work.join("ran.marker");

    // No stored consent + accept env → hook runs.
    let o1 = sb.hooks_in_env(&["run", "pre-commit"], &work, &[("GIT_HOOKS_CONSENT", "accept")]);
    assert!(o1.status.success(), "run failed: {}", err(&o1));
    assert!(marker.exists(), "accept env should run the hook");

    // The override was not persisted.
    let st = sb.hooks_in(&["status"], &work);
    assert!(
        out(&st).contains("no decision yet"),
        "env override must not persist consent: {}",
        out(&st)
    );

    // decline env → hook skipped.
    fs::remove_file(&marker).unwrap();
    let o2 = sb.hooks_in_env(&["run", "pre-commit"], &work, &[("GIT_HOOKS_CONSENT", "decline")]);
    assert!(o2.status.success());
    assert!(!marker.exists(), "decline env should skip the hook");

    // accept:<wrong-hash> → does NOT enable; falls back to no-TTY skip.
    let o3 = sb.hooks_in_env(
        &["run", "pre-commit"],
        &work,
        &[("GIT_HOOKS_CONSENT", "accept:deadbeef")],
    );
    assert!(o3.status.success());
    assert!(!marker.exists(), "wrong-hash pin must not enable the hook");
    assert!(
        err(&o3).contains("skipping"),
        "wrong-hash pin should fall back to no-tty skip: {}",
        err(&o3)
    );
}

// The full hook list is wired into `add` validation: a hook new in M2 is
// accepted.
#[test]
fn add_accepts_new_hook_name() {
    let sb = Sandbox::new();
    let repo = sb.root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    sb.git_in(&["init"], &repo);

    let a = sb.hooks_in(&["add", "pre-merge-commit", "true"], &repo);
    assert!(a.status.success(), "add pre-merge-commit failed: {}", err(&a));

    let st = sb.hooks_in(&["status"], &repo);
    assert!(
        out(&st).contains("pre-merge-commit"),
        "status should list the new hook: {}",
        out(&st)
    );
}

// M3: a repo signed by a key pre-trusted in the org policy file auto-accepts on
// clone with NO prompt and NO terminal — the prompt-fatigue killer.
#[test]
fn signed_and_policy_trusted_auto_accepts() {
    let sb = Sandbox::new();
    sb.install();
    let key = sb.keygen();
    let fp = sb.fingerprint(&key);
    let origin = sb.make_origin("[hooks]\npre-commit = [\"touch ran.marker\"]\n", &[]);
    sb.sign_origin(&origin, &key, "maint@example.com");
    // Org pre-seeds the trusted key before the developer ever clones.
    sb.write_policy(&format!("trusted_keys = [\"{fp}\"]\n"));

    // Clone fires post-checkout, which auto-accepts with no tty and no prompt.
    let work = sb.root.join("work");
    let cl = sb.git_in(
        &["clone", origin.to_str().unwrap(), work.to_str().unwrap()],
        &sb.root,
    );
    assert!(cl.status.success(), "clone failed: {}", err(&cl));
    assert!(
        err(&cl).contains("auto-accepted"),
        "expected prompt-less auto-accept notice on clone: {}",
        err(&cl)
    );

    let st = sb.hooks_in(&["status"], &work);
    assert!(
        out(&st).contains("accepted") && out(&st).contains("trusted"),
        "clone should be auto-accepted and key trusted: {}",
        out(&st)
    );

    // And the hook actually runs on commit (consent already recorded, no prompt).
    sb.stage(&work, "a.txt", "1\n");
    let c = sb.git_in(&["commit", "-m", "c"], &work);
    assert!(c.status.success(), "commit failed: {}", err(&c));
    assert!(
        work.join("ran.marker").exists(),
        "trusted-signed hook should run: {}",
        err(&c)
    );
}

// M3: pressing `t`rust is equivalent to `git hooks trust <fp>` repo-locally;
// afterwards the signed repo auto-accepts on the next hook without a tty.
#[test]
fn trust_key_locally_then_auto_accepts() {
    let sb = Sandbox::new();
    sb.install();
    let key = sb.keygen();
    let fp = sb.fingerprint(&key);
    let origin = sb.make_origin("[hooks]\npre-commit = [\"touch ran.marker\"]\n", &[]);
    sb.sign_origin(&origin, &key, "maint@example.com");
    let work = sb.clone(&origin, "work");

    let t = sb.hooks_in(&["trust", &fp], &work);
    assert!(t.status.success(), "trust failed: {}", err(&t));

    sb.stage(&work, "a.txt", "1\n");
    let c = sb.git_in(&["commit", "-m", "c"], &work);
    assert!(c.status.success(), "commit failed: {}", err(&c));
    assert!(
        work.join("ran.marker").exists(),
        "locally-trusted signed hook should auto-run: {}",
        err(&c)
    );
}

// M3: tampering with a .githooks/ script after signing invalidates the
// signature. Even with the key trusted, an invalid signature never auto-accepts;
// with no tty the hook is skipped, and status reports INVALID.
#[test]
fn tampered_signature_never_auto_accepts() {
    let sb = Sandbox::new();
    sb.install();
    let key = sb.keygen();
    let fp = sb.fingerprint(&key);
    let origin = sb.make_origin(
        "[hooks]\npre-commit = [\".githooks/hook.sh\"]\n",
        &[(".githooks/hook.sh", "#!/bin/sh\ntouch ran.marker\n")],
    );
    sb.sign_origin(&origin, &key, "maint@example.com");
    let work = sb.clone(&origin, "work");
    sb.hooks_in(&["trust", &fp], &work);

    // Tamper AFTER signing: the signed content no longer matches.
    write_script(
        &work.join(".githooks/hook.sh"),
        "#!/bin/sh\n# injected\ntouch ran.marker\n",
    );

    let st = sb.hooks_in(&["status"], &work);
    assert!(
        out(&st).contains("INVALID"),
        "status should report INVALID: {}",
        out(&st)
    );

    sb.stage(&work, "a.txt", "1\n");
    let c = sb.git_in(&["commit", "-m", "c"], &work);
    assert!(
        c.status.success(),
        "commit should pass (hook skipped): {}",
        err(&c)
    );
    assert!(
        !work.join("ran.marker").exists(),
        "tampered signature must not auto-accept even with a trusted key"
    );
}

// M3: an unsigned repo reports `unsigned` and its consent flow is unchanged.
#[test]
fn unsigned_repo_status_reports_unsigned() {
    let sb = Sandbox::new();
    sb.install();
    let origin = sb.make_origin("[hooks]\npre-commit = [\"true\"]\n", &[]);
    let work = sb.clone(&origin, "work");
    let st = sb.hooks_in(&["status"], &work);
    assert!(
        out(&st).contains("signature: unsigned"),
        "unsigned repo should report unsigned: {}",
        out(&st)
    );
}

// M3: org policy `default = "decline"` skips hooks in an unsigned/untrusted repo
// with no prompt and no tty — a failing hook never runs, the commit passes.
#[test]
fn policy_default_decline_skips_silently() {
    let sb = Sandbox::new();
    sb.install();
    let origin = sb.make_origin("[hooks]\npre-commit = [\"exit 1\"]\n", &[]);
    let work = sb.clone(&origin, "work");
    sb.write_policy("default = \"decline\"\n");

    sb.stage(&work, "a.txt", "1\n");
    let c = sb.git_in(&["commit", "-m", "c"], &work);
    assert!(
        c.status.success(),
        "decline policy must skip the failing hook: {}",
        err(&c)
    );
    assert!(
        err(&c).contains("decline"),
        "expected a policy-decline notice: {}",
        err(&c)
    );
    assert!(
        !err(&c).contains("FAILED"),
        "hook must not run under decline policy: {}",
        err(&c)
    );
}

// M3: `git hooks diff` shows what changed since the last accept.
#[test]
fn diff_shows_change_since_accept() {
    let sb = Sandbox::new();
    sb.install();
    let origin = sb.make_origin(
        "[hooks]\npre-commit = [\".githooks/hook.sh\"]\n",
        &[(".githooks/hook.sh", "#!/bin/sh\necho ORIGINAL\n")],
    );
    let work = sb.clone(&origin, "work");
    sb.hooks_in(&["accept"], &work);

    write_script(&work.join(".githooks/hook.sh"), "#!/bin/sh\necho CHANGED\n");
    let d = sb.hooks_in(&["diff"], &work);
    assert!(d.status.success(), "diff failed: {}", err(&d));
    assert!(
        out(&d).contains("CHANGED"),
        "diff should contain the change: {}",
        out(&d)
    );
    assert!(
        out(&d).contains("signature: unsigned"),
        "diff should show the signature status: {}",
        out(&d)
    );
}
