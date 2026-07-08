//! End-to-end tests. Plain std::process::Command, no dev-dependencies.
//!
//! Each test builds an isolated sandbox under the system temp dir with its own
//! fake HOME (so `git config --global` and our template dir stay contained) and
//! a PATH that finds the freshly built binary. Every git/git-hooks invocation
//! runs under `setsid -w`, which detaches the controlling terminal: opening
//! /dev/tty then fails, so the consent prompt is never interactive and tests
//! are deterministic. Explicit `git hooks accept`/`decline` drive consent.

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
