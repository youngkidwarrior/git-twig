use anyhow::{Result, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    source: PathBuf,
}
impl Fixture {
    fn new() -> Result<Self> {
        Self::with_object_format("sha1")
    }
    fn with_object_format(format: &str) -> Result<Self> {
        let fixtures = std::env::var_os("TWIG_TEST_FIXTURE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-fixtures")
            });
        let root = fixtures.join(format!(
            "{}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root)?;
        let root = fs::canonicalize(root)?;
        let source = root.join("source");
        fs::create_dir(&source)?;
        git(
            &source,
            &["init", &format!("--object-format={format}"), "-b", "main"],
        )?;
        git(&source, &["config", "user.name", "Fixture"])?;
        git(
            &source,
            &["config", "user.email", "fixture@example.invalid"],
        )?;
        fs::write(source.join("hello.txt"), "hello\n")?;
        fs::write(source.join(".gitignore"), "ignored/\n")?;
        git(&source, &["add", "."])?;
        git(&source, &["commit", "-m", "seed"])?;
        eprintln!("fixture retained: {}", root.display());
        Ok(Self { root, source })
    }
    fn twig(&self, args: &[&str]) -> Result<Output> {
        twig(&self.source, args)
    }
    fn add(&self) -> Result<Output> {
        self.twig(&["new", "-b", "feature", "../feature", "HEAD"])
    }
    fn destination(&self) -> PathBuf {
        self.root.join("feature")
    }
    fn staging_root(&self) -> PathBuf {
        self.source.join(".git/twigs/staging")
    }
    fn stages(&self) -> Result<Vec<PathBuf>> {
        match fs::read_dir(self.staging_root()) {
            Ok(entries) => entries.map(|entry| Ok(entry?.path())).collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }
    fn assert_no_sibling_stages(&self) -> Result<()> {
        for entry in fs::read_dir(&self.root)? {
            assert!(
                !entry?
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".git-twig-stage-")
            );
        }
        Ok(())
    }
}
fn command(program: impl AsRef<std::ffi::OsStr>, cwd: &Path) -> Command {
    let mut c = Command::new(program);
    c.current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("XDG_CONFIG_HOME", "/nonexistent/twig-tests")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("TWIG_TRACE", "1");
    c
}
#[cfg(unix)]
fn bounded_output(mut command: Command) -> Result<Output> {
    use std::{os::unix::process::CommandExt, sync::mpsc, time::Duration};
    // SAFETY: setpgid is async-signal-safe; isolate only this child and its descendants.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let pid = child.id() as libc::pid_t;
    let (done, finished) = mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if finished.recv_timeout(Duration::from_secs(10)).is_err() {
            // SAFETY: the child created this process group. Kill its descendants too,
            // since a deadlocked grandchild can otherwise keep output pipes open.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    });
    let result = child.wait_with_output();
    let _ = done.send(());
    watchdog.join().expect("watchdog panicked");
    Ok(result?)
}

fn checked(output: Output) -> Result<Output> {
    ensure!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}
fn git(cwd: &Path, args: &[&str]) -> Result<Output> {
    checked(command("git", cwd).args(args).output()?)
}
fn twig(cwd: &Path, args: &[&str]) -> Result<Output> {
    Ok(command(env!("CARGO_BIN_EXE_git-twig"), cwd)
        .args(args)
        .output()?)
}
fn clean(path: &Path) -> Result<()> {
    ensure!(
        git(path, &["status", "--porcelain"])?.stdout.is_empty(),
        "worktree is dirty"
    );
    Ok(())
}
fn native(output: &Output) {
    if cfg!(target_os = "macos") || std::env::var_os("TWIG_TEST_REQUIRE_NATIVE").is_some() {
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("CoW: verified tracked files"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[cfg(target_os = "linux")]
fn linux_reflinks_available(f: &Fixture) -> Result<bool> {
    // Probe independently with coreutils, not Twig, so a broken backend cannot
    // disguise itself as an unsupported filesystem.
    let output = command("cp", &f.source)
        .arg("--reflink=always")
        .arg(f.source.join("hello.txt"))
        .arg(f.root.join("reflink-probe"))
        .output()?;
    if std::env::var_os("TWIG_TEST_REQUIRE_NATIVE").is_some() {
        ensure!(
            output.status.success(),
            "test filesystem must support reflinks"
        );
    }
    Ok(output.status.success())
}

#[cfg(target_os = "linux")]
#[test]
fn linux_selects_reflinks_or_delegates_on_unsupported_filesystems() -> Result<()> {
    let f = Fixture::new()?;
    let supported = linux_reflinks_available(&f)?;
    let output = checked(f.add()?)?;
    let trace = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        trace.contains("Linux reflink CoW: verified tracked files"),
        supported,
        "{trace}"
    );
    if !supported {
        assert!(trace.contains("stock Git:"), "{trace}");
    }
    clean(&f.destination())?;
    fs::write(f.destination().join("hello.txt"), "private")?;
    assert_eq!(fs::read(f.source.join("hello.txt"))?, b"hello\n");
    Ok(())
}

#[cfg(unix)]
#[test]
fn disabled_symlinks_delegate_for_root_and_nested_entries() -> Result<()> {
    let f = Fixture::new()?;
    fs::create_dir(f.source.join("tree"))?;
    for name in ["link", "tree/link"] {
        std::os::unix::fs::symlink("missing", f.source.join(name))?;
    }
    git(&f.source, &["add", "."])?;
    git(&f.source, &["commit", "-m", "links"])?;
    git(&f.source, &["config", "core.symlinks", "false"])?;
    let output = checked(f.add()?)?;
    assert!(!String::from_utf8_lossy(&output.stderr).contains("CoW: verified"));
    for name in ["link", "tree/link"] {
        assert!(fs::symlink_metadata(f.destination().join(name))?.is_file());
        assert_eq!(fs::read(f.destination().join(name))?, b"missing");
        assert!(fs::symlink_metadata(f.source.join(name))?.is_symlink());
    }
    clean(&f.destination())
}

#[test]
fn legacy_crlf_attributes_cannot_hide_dirty_raw_bytes() -> Result<()> {
    for attribute in ["crlf", "crlf=input"] {
        let f = Fixture::new()?;
        fs::create_dir(f.source.join("tree"))?;
        fs::write(f.source.join("tree/file.txt"), "hello\n")?;
        fs::write(
            f.source.join(".gitattributes"),
            format!("*.txt {attribute}\n"),
        )?;
        git(&f.source, &["add", "."])?;
        git(&f.source, &["commit", "-m", "legacy conversion"])?;
        for name in ["hello.txt", "tree/file.txt"] {
            fs::write(f.source.join(name), "hello\r\n")?;
        }
        let output = checked(f.add()?)?;
        assert!(!String::from_utf8_lossy(&output.stderr).contains("CoW: verified"));
        for name in ["hello.txt", "tree/file.txt"] {
            assert_eq!(fs::read(f.destination().join(name))?, b"hello\n");
            assert_eq!(fs::read(f.source.join(name))?, b"hello\r\n");
        }
        clean(&f.destination())?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn destination_setgid_directories_match_git() -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, chown};
    let f = Fixture::new()?;
    fs::create_dir_all(f.source.join("tree/nested"))?;
    fs::write(f.source.join("tree/nested/file"), "data")?;
    git(&f.source, &["add", "."])?;
    git(&f.source, &["commit", "-m", "nested"])?;
    if fs::metadata(&f.root)?.permissions().mode() & 0o2000 == 0 {
        if let Some(group) = std::env::var_os("TWIG_TEST_SHARED_GID") {
            chown(&f.root, None, Some(group.to_str().unwrap().parse::<u32>()?))?;
        }
        fs::set_permissions(&f.root, fs::Permissions::from_mode(0o2770))?;
    }
    let output = checked(f.add()?)?;
    native(&output);
    git(&f.source, &["worktree", "add", "--detach", "../stock"])?;
    let expected_group = fs::metadata(&f.root)?.gid();
    for name in ["tree", "tree/nested", "tree/nested/file"] {
        let ours = fs::metadata(f.destination().join(name))?;
        let stock = fs::metadata(f.root.join("stock").join(name))?;
        assert_eq!(ours.gid(), expected_group, "group for {name}");
        assert_eq!(
            ours.permissions().mode() & 0o7777,
            stock.permissions().mode() & 0o7777,
            "mode for {name}"
        );
    }
    clean(&f.destination())
}

#[cfg(unix)]
#[test]
fn reference_and_index_hooks_can_reenter_without_registrar_deadlock() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for event in ["reference-transaction", "post-index-change"] {
        for style in ["legacy", "relative", "absolute", "configured"] {
            let f = Fixture::new()?;
            if style == "configured"
                && !command("git", &f.source)
                    .args([
                        "-c",
                        &format!("hook.twig-probe.event={event}"),
                        "-c",
                        "hook.twig-probe.command=true",
                        "hook",
                        "list",
                        event,
                    ])
                    .output()?
                    .status
                    .success()
            {
                continue; // Config-defined hooks are new in Git 2.56.
            }
            let hooks = if style == "legacy" {
                f.source.join(".git/hooks")
            } else {
                f.root.join("custom-hooks")
            };
            fs::create_dir_all(&hooks)?;
            let script = hooks.join(event);
            fs::write(
                &script,
                "#!/bin/sh\nif [ \"$TWIG_HOOK_ACTIVE\" != 1 ] && [ ! -e \"$TWIG_HOOK_SOURCE/../hook-child/.git\" ]; then\n  (cd \"$TWIG_HOOK_SOURCE\" && TWIG_HOOK_ACTIVE=1 \"$TWIG_HOOK_BINARY\" new --detach ../hook-child HEAD) || exit $?\nfi\nexit 0\n",
            )?;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
            match style {
                "relative" => {
                    git(&f.source, &["config", "core.hooksPath", "../custom-hooks"])?;
                }
                "absolute" => {
                    git(
                        &f.source,
                        &["config", "core.hooksPath", hooks.to_str().unwrap()],
                    )?;
                }
                "configured" => {
                    git(
                        &f.source,
                        &[
                            "config",
                            "hook.twig-test.command",
                            "sh \"$TWIG_HOOK_SCRIPT\"",
                        ],
                    )?;
                    git(&f.source, &["config", "hook.twig-test.event", event])?;
                }
                _ => {}
            }
            let mut child = command(env!("CARGO_BIN_EXE_git-twig"), &f.source);
            child
                .args(["new", "-b", "feature", "../feature"])
                .env("TWIG_HOOK_BINARY", env!("CARGO_BIN_EXE_git-twig"))
                .env("TWIG_HOOK_SOURCE", &f.source)
                .env("TWIG_HOOK_SCRIPT", &script);
            let output = checked(bounded_output(child)?)?;
            assert!(
                !String::from_utf8_lossy(&output.stderr).contains("CoW: verified"),
                "{event}/{style}"
            );
            clean(&f.destination())?;
            clean(&f.root.join("hook-child"))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn non_utf8_empty_large_and_sparse_files_have_private_writes() -> Result<()> {
    use std::{
        io::{Seek, SeekFrom, Write},
        os::unix::ffi::OsStringExt,
    };
    let f = Fixture::new()?;
    // APFS requires valid Unicode names; Linux exercises the raw-byte path here.
    let name = std::ffi::OsString::from_vec(if cfg!(target_os = "macos") {
        b"unicode-file-shapes".to_vec()
    } else {
        b"non-utf8-\xff".to_vec()
    });
    fs::create_dir(f.source.join(&name))?;
    fs::write(f.source.join(&name).join("empty"), b"")?;
    fs::write(
        f.source.join(&name).join("large"),
        vec![0x5a; 2 * 1024 * 1024],
    )?;
    let mut sparse = fs::File::create(f.source.join(&name).join("sparse"))?;
    sparse.set_len(16 * 1024 * 1024)?;
    sparse.seek(SeekFrom::Start(8 * 1024 * 1024))?;
    sparse.write_all(b"middle")?;
    drop(sparse);
    git(&f.source, &["add", "."])?;
    git(&f.source, &["commit", "-m", "file shapes"])?;
    let output = checked(f.add()?)?;
    native(&output);
    for file in ["empty", "large", "sparse"] {
        let source = f.source.join(&name).join(file);
        let target = f.destination().join(&name).join(file);
        let expected = fs::read(&source)?;
        assert_eq!(fs::read(&target)?, expected);
        fs::OpenOptions::new()
            .write(true)
            .open(&target)?
            .write_all(b"destination")?;
        assert_eq!(fs::read(&source)?, expected);
        let private = fs::read(&target)?;
        fs::write(&source, "source truncated")?;
        assert_eq!(fs::read(&target)?, private);
        fs::OpenOptions::new()
            .write(true)
            .open(&target)?
            .set_len(0)?;
        assert_eq!(fs::read(&source)?, b"source truncated");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn nested_tracked_fifo_falls_back_without_blocking_or_touching_source() -> Result<()> {
    let f = Fixture::new()?;
    fs::create_dir(f.source.join("tree"))?;
    fs::write(f.source.join("tree/file"), "committed")?;
    git(&f.source, &["add", "."])?;
    git(&f.source, &["commit", "-m", "regular"])?;
    fs::rename(f.source.join("tree/file"), f.root.join("original"))?;
    let fifo = std::ffi::CString::new(f.source.join("tree/file").as_os_str().as_encoded_bytes())?;
    // SAFETY: fifo is a live C path and the permission mode is valid.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let mut child = command(env!("CARGO_BIN_EXE_git-twig"), &f.source);
    child.args(["new", "--detach", "../feature"]);
    let output = checked(bounded_output(child)?)?;
    assert!(!String::from_utf8_lossy(&output.stderr).contains("CoW: verified"));
    assert_eq!(fs::read(f.destination().join("tree/file"))?, b"committed");
    assert_eq!(fs::read(f.root.join("original"))?, b"committed");
    use std::os::unix::fs::FileTypeExt;
    assert!(
        fs::symlink_metadata(f.source.join("tree/file"))?
            .file_type()
            .is_fifo()
    );
    clean(&f.destination())
}

#[test]
fn sha256_repositories_create_clean_worktrees() -> Result<()> {
    let f = Fixture::with_object_format("sha256")?;
    let output = checked(f.add()?)?;
    native(&output);
    assert_eq!(
        git(&f.destination(), &["rev-parse", "HEAD"])?.stdout.len(),
        65
    );
    clean(&f.destination())
}

#[test]
fn same_size_dirty_bytes_with_restored_mtime_are_not_inherited() -> Result<()> {
    let f = Fixture::new()?;
    git(&f.source, &["config", "core.ignorestat", "true"])?;
    let path = f.source.join("hello.txt");
    let modified = fs::metadata(&path)?.modified()?;
    fs::write(&path, b"jello\n")?;
    fs::File::options()
        .write(true)
        .open(&path)?
        .set_times(fs::FileTimes::new().set_modified(modified))?;
    checked(f.add()?)?;
    assert_eq!(fs::read(f.destination().join("hello.txt"))?, b"hello\n");
    assert_eq!(fs::read(&path)?, b"jello\n");
    clean(&f.destination())
}

#[cfg(target_os = "linux")]
#[test]
fn cross_filesystem_reflink_failure_delegates_before_registration() -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let Some(other) = std::env::var_os("TWIG_TEST_CROSS_FILESYSTEM_ROOT") else {
        return Ok(());
    };
    let f = Fixture::new()?;
    let parent = PathBuf::from(other).join(f.root.file_name().unwrap());
    fs::create_dir(&parent)?;
    assert_ne!(fs::metadata(&parent)?.dev(), fs::metadata(&f.source)?.dev());
    let destination = parent.join("feature");
    let output = checked(
        command(env!("CARGO_BIN_EXE_git-twig"), &f.source)
            .args(["new", "--detach"])
            .arg(&destination)
            .output()?,
    )?;
    assert!(!String::from_utf8_lossy(&output.stderr).contains("CoW: verified"));
    assert_eq!(fs::read(destination.join("hello.txt"))?, b"hello\n");
    assert_eq!(
        git(&f.source, &["worktree", "list", "--porcelain"])?
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| line.starts_with(b"worktree "))
            .count(),
        2
    );
    clean(&destination)?;
    clean(&f.source)
}

#[test]
fn skill_is_embedded_and_does_not_require_git() -> Result<()> {
    let output = checked(
        Command::new(env!("CARGO_BIN_EXE_git-twig"))
            .arg("skill")
            .env("PATH", "")
            .current_dir(std::env::temp_dir())
            .output()?,
    )?;
    assert_eq!(output.stdout, include_bytes!("../skills/git-twig/SKILL.md"));
    assert!(output.stderr.is_empty());
    Ok(())
}

#[test]
fn skill_rejects_extra_arguments_without_git() -> Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_git-twig"))
        .args(["skill", "extra"])
        .env("PATH", "")
        .current_dir(std::env::temp_dir())
        .output()?;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"Usage: git twig skill\n");
    Ok(())
}

#[test]
fn git_c_main_layout_creates_a_sibling_from_head() -> Result<()> {
    let f = Fixture::new()?;
    let bin = Path::new(env!("CARGO_BIN_EXE_git-twig")).parent().unwrap();
    let path = std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))?;
    let output = checked(
        command("git", &f.root)
            .env("PATH", path)
            .args([
                "-C",
                "source",
                "twig",
                "new",
                "-b",
                "feat/x",
                "../feat-x",
                "HEAD",
            ])
            .output()?,
    )?;
    native(&output);
    assert_eq!(fs::read(f.root.join("feat-x/hello.txt"))?, b"hello\n");
    assert_eq!(
        git(&f.source, &["rev-parse", "HEAD"])?.stdout,
        git(&f.root.join("feat-x"), &["rev-parse", "HEAD"])?.stdout
    );
    assert!(f.stages()?.is_empty());
    f.assert_no_sibling_stages()?;
    clean(&f.root.join("feat-x"))
}

#[test]
fn linked_source_retains_failed_staging_in_the_common_git_directory() -> Result<()> {
    let f = Fixture::new()?;
    checked(f.add()?)?;
    let output = twig(&f.destination(), &["new", "-b", "main", "../third", "HEAD"])?;
    assert!(!output.status.success());
    assert_eq!(f.stages()?.len(), 1);
    assert!(f.destination().join(".git").is_file());
    f.assert_no_sibling_stages()?;
    clean(&f.source)?;
    clean(&f.destination())
}

#[test]
fn short_help_does_not_require_git_or_a_repository() -> Result<()> {
    for args in [
        vec!["-h"],
        vec!["--help"],
        vec!["new", "-h"],
        vec!["new", "--help"],
        vec!["new", "-b", "example", "--help"],
        vec!["skill", "--help"],
    ] {
        let output = checked(
            Command::new(env!("CARGO_BIN_EXE_git-twig"))
                .args(&args)
                .env("PATH", "")
                .current_dir(std::env::temp_dir())
                .output()?,
        )?;
        assert!(String::from_utf8(output.stdout)?.contains("Usage: git twig"));
        assert!(output.stderr.is_empty());
    }
    Ok(())
}

#[test]
fn git_dispatches_short_help_outside_a_repository() -> Result<()> {
    let f = Fixture::new()?;
    let outside = f.root.join("outside");
    fs::create_dir(&outside)?;
    let bin = Path::new(env!("CARGO_BIN_EXE_git-twig")).parent().unwrap();
    let path = std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))?;
    for args in [
        vec!["twig", "-h"],
        vec!["twig", "new", "--help"],
        vec!["twig", "list", "-h"],
        vec!["twig", "skill", "--help"],
    ] {
        let output = checked(
            command("git", &outside)
                .env("PATH", &path)
                .args(&args)
                .output()?,
        )?;
        assert!(String::from_utf8(output.stdout)?.contains("Usage: git twig"));
        assert!(output.stderr.is_empty());
    }
    assert_eq!(fs::read_dir(&outside)?.count(), 0);
    Ok(())
}

#[cfg(unix)]
#[test]
fn git_help_finds_the_bundled_manual_when_installed() -> Result<()> {
    let f = Fixture::new()?;
    if command("man", &f.root).arg("--version").output().is_err() {
        return Ok(()); // Minimal Linux containers need not install a man viewer.
    }
    let manpath = f.root.join("manual");
    fs::create_dir_all(manpath.join("man1"))?;
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("man/git-twig.1"),
        manpath.join("man1/git-twig.1"),
    )?;
    let bin = Path::new(env!("CARGO_BIN_EXE_git-twig")).parent().unwrap();
    let path = std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))?;
    let output = checked(
        command("git", &f.root)
            .env("PATH", path)
            .env("MANPATH", manpath)
            .env("MANPAGER", "cat")
            .env("PAGER", "cat")
            .args(["twig", "--help"])
            .output()?,
    )?;
    assert!(!output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("No manual entry"));
    Ok(())
}

#[test]
fn new_delegates_revisions_and_checkout_options_to_git() -> Result<()> {
    let f = Fixture::new()?;
    fs::write(f.source.join("hello.txt"), "newer\n")?;
    git(&f.source, &["add", "."])?;
    git(&f.source, &["commit", "-m", "newer"])?;
    checked(f.twig(&["new", "--detach", "../older", "HEAD~1"])?)?;
    assert_eq!(fs::read(f.root.join("older/hello.txt"))?, b"hello\n");
    checked(f.twig(&["new", "--no-checkout", "--detach", "../empty"])?)?;
    assert!(!f.root.join("empty/hello.txt").exists());
    checked(f.twig(&["new", "../inferred"])?)?;
    assert_eq!(
        git(&f.root.join("inferred"), &["branch", "--show-current"])?.stdout,
        b"inferred\n"
    );
    assert!(f.stages()?.is_empty());
    clean(&f.root.join("older"))?;
    clean(&f.root.join("inferred"))?;
    clean(&f.source)
}

#[test]
fn removed_add_is_rejected_without_creating_anything() -> Result<()> {
    let f = Fixture::new()?;
    for args in [
        vec!["add"],
        vec!["add", "--help"],
        vec!["add", "-b", "legacy", "../legacy", "HEAD"],
    ] {
        let output = f.twig(&args)?;
        assert!(!output.status.success());
        assert!(String::from_utf8(output.stderr)?.contains("'add' has been removed"));
    }
    assert!(!f.root.join("legacy").exists());
    assert!(!f.staging_root().exists());
    clean(&f.source)
}

#[test]
fn removed_add_is_rejected_outside_a_repo_without_git() -> Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_git-twig"))
        .args(["add", "--help"])
        .env("PATH", "")
        .current_dir(std::env::temp_dir())
        .output()?;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr)?.contains("'add' has been removed"));
    Ok(())
}

#[test]
fn help_advertises_the_skill() -> Result<()> {
    let output = checked(
        Command::new(env!("CARGO_BIN_EXE_git-twig"))
            .arg("--help")
            .output()?,
    )?;
    let text = String::from_utf8(output.stdout)?;
    assert!(text.contains("git twig skill"));
    assert!(text.contains("git -C main twig new"));
    assert!(!text.contains("  add       Create"));
    Ok(())
}

#[test]
fn successful_create_cleans_only_its_own_staging() -> Result<()> {
    let f = Fixture::new()?;
    let retained = f.staging_root().join("old-failed-attempt");
    fs::create_dir_all(&retained)?;
    fs::write(retained.join("keep"), "diagnostics")?;
    let output = checked(f.add()?)?;
    native(&output);
    assert_eq!(f.stages()?, vec![retained.clone()]);
    assert_eq!(fs::read_to_string(retained.join("keep"))?, "diagnostics");
    f.assert_no_sibling_stages()?;
    clean(&f.source)?;
    clean(&f.destination())
}

#[test]
fn successful_fallback_cleans_its_failed_native_attempt() -> Result<()> {
    let f = Fixture::new()?;
    fs::write(f.source.join("hello.txt"), "dirty source")?;
    checked(f.add()?)?;
    assert!(f.stages()?.is_empty());
    f.assert_no_sibling_stages()?;
    assert_eq!(fs::read(f.source.join("hello.txt"))?, b"dirty source");
    assert_eq!(fs::read(f.destination().join("hello.txt"))?, b"hello\n");
    clean(&f.destination())
}

#[cfg(unix)]
#[test]
fn failed_create_retains_private_staging_in_git_metadata() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new()?;
    let output = f.twig(&["new", "-b", "main", "../feature", "HEAD"])?;
    assert!(!output.status.success());
    let stages = f.stages()?;
    assert_eq!(stages.len(), 1);
    assert!(stages[0].starts_with(f.staging_root()));
    assert_eq!(fs::metadata(&stages[0])?.permissions().mode() & 0o077, 0);
    assert!(stages[0].join("checkout/data").is_dir());
    assert!(!f.destination().exists());
    f.assert_no_sibling_stages()?;
    clean(&f.source)
}

#[test]
fn creates_real_clean_worktree_without_source_hardlinks() -> Result<()> {
    let f = Fixture::new()?;
    let output = checked(f.add()?)?;
    native(&output);
    if cfg!(target_os = "macos") {
        assert!(f.source.join(".git/twigs/registration.lock").is_file());
    }
    clean(&f.destination())?;
    assert_eq!(
        git(&f.destination(), &["branch", "--show-current"])?.stdout,
        b"feature\n"
    );
    assert!(
        git(&f.source, &["worktree", "list", "--porcelain"])?
            .stdout
            .windows(7)
            .any(|w| w == b"feature")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_ne!(
            fs::metadata(f.source.join("hello.txt"))?.ino(),
            fs::metadata(f.destination().join("hello.txt"))?.ino()
        );
    }
    fs::write(f.destination().join("hello.txt"), "private\n")?;
    assert_eq!(fs::read(f.source.join("hello.txt"))?, b"hello\n");
    Ok(())
}

#[test]
fn dirty_source_produces_clean_commit_checkout() -> Result<()> {
    let f = Fixture::new()?;
    fs::write(f.source.join("hello.txt"), "dirty\n")?;
    checked(f.add()?)?;
    clean(&f.destination())?;
    assert_eq!(fs::read(f.destination().join("hello.txt"))?, b"hello\n");
    assert_eq!(fs::read(f.source.join("hello.txt"))?, b"dirty\n");
    Ok(())
}

#[test]
fn staged_source_changes_are_not_inherited() -> Result<()> {
    let f = Fixture::new()?;
    fs::write(f.source.join("hello.txt"), "staged\n")?;
    git(&f.source, &["add", "."])?;
    checked(f.add()?)?;
    clean(&f.destination())?;
    assert_eq!(fs::read(f.destination().join("hello.txt"))?, b"hello\n");
    assert!(
        !git(&f.source, &["diff", "--cached", "--name-only"])?
            .stdout
            .is_empty()
    );
    Ok(())
}

#[test]
fn untracked_and_ignored_files_stay_out() -> Result<()> {
    let f = Fixture::new()?;
    fs::create_dir(f.source.join("ignored"))?;
    fs::write(f.source.join("ignored/cache"), "cache")?;
    fs::write(f.source.join("untracked"), "secret")?;
    let output = checked(f.add()?)?;
    native(&output);
    assert!(!f.destination().join("ignored").exists());
    assert!(!f.destination().join("untracked").exists());
    clean(&f.destination())
}

#[test]
fn existing_destination_is_untouched() -> Result<()> {
    let f = Fixture::new()?;
    fs::create_dir(f.destination())?;
    fs::write(f.destination().join("keep"), "keep")?;
    assert!(!f.add()?.status.success());
    assert_eq!(fs::read(f.destination().join("keep"))?, b"keep");
    Ok(())
}

#[test]
fn different_commit_uses_git() -> Result<()> {
    let f = Fixture::new()?;
    fs::write(f.source.join("hello.txt"), "second\n")?;
    git(&f.source, &["commit", "-am", "second"])?;
    checked(f.twig(&["new", "--detach", "../feature", "HEAD~1"])?)?;
    assert_eq!(fs::read(f.destination().join("hello.txt"))?, b"hello\n");
    clean(&f.destination())
}

#[test]
fn can_clone_a_linked_source() -> Result<()> {
    let f = Fixture::new()?;
    checked(f.add()?)?;
    let output = checked(twig(
        &f.destination(),
        &["new", "-b", "second", "../second"],
    )?)?;
    native(&output);
    clean(&f.root.join("second"))
}

#[test]
fn detached_and_no_checkout_modes() -> Result<()> {
    let f = Fixture::new()?;
    let output = checked(f.twig(&["new", "--detach", "../feature"])?)?;
    native(&output);
    clean(&f.destination())?;
    assert!(
        git(&f.destination(), &["branch", "--show-current"])?
            .stdout
            .is_empty()
    );
    checked(f.twig(&["new", "--no-checkout", "--detach", "../empty"])?)?;
    assert!(!f.root.join("empty/hello.txt").exists());
    Ok(())
}

#[test]
fn passthrough_matches_git() -> Result<()> {
    let f = Fixture::new()?;
    checked(f.add()?)?;
    for args in [vec!["list"], vec!["list", "--porcelain"]] {
        let mut stock_args = vec!["worktree"];
        stock_args.extend_from_slice(&args);
        assert_eq!(
            checked(f.twig(&args)?)?.stdout,
            git(&f.source, &stock_args)?.stdout
        );
    }
    let twig = f.twig(&["not-a-command"])?;
    let stock = command("git", &f.source)
        .args(["worktree", "not-a-command"])
        .output()?;
    assert_eq!(twig.status.code(), stock.status.code());
    Ok(())
}

#[cfg(unix)]
#[test]
fn executable_symlink_and_unusual_names() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new()?;
    let odd = std::ffi::OsString::from("odd-雪\t\n");
    fs::write(f.source.join(&odd), "odd")?;
    fs::write(f.source.join("run"), "#!/bin/sh\nexit 0\n")?;
    fs::set_permissions(f.source.join("run"), fs::Permissions::from_mode(0o755))?;
    std::os::unix::fs::symlink("missing", f.source.join("link"))?;
    git(&f.source, &["add", "."])?;
    git(&f.source, &["commit", "-m", "modes"])?;
    let output = checked(f.add()?)?;
    native(&output);
    assert_eq!(fs::read(f.destination().join(odd))?, b"odd");
    assert_eq!(
        fs::read_link(f.destination().join("link"))?,
        PathBuf::from("missing")
    );
    assert_eq!(
        fs::metadata(f.destination().join("run"))?
            .permissions()
            .mode()
            & 0o777,
        // Match Git's creation permissions under the caller's umask, not a fixed 022.
        {
            use std::os::unix::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o777)
                .open(f.root.join("mode-probe"))?
                .metadata()?
                .permissions()
                .mode()
                & 0o777
        }
    );
    clean(&f.destination())
}

#[cfg(unix)]
#[test]
fn hooks_run_once_and_failure_retains_worktree() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for failure in [false, true] {
        let f = Fixture::new()?;
        let hook = f.source.join(".git/hooks/post-checkout");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf '%s %s %s\\n' \"$1\" \"$2\" \"$3\" >> hook-output\nexit {}\n",
                if failure { 23 } else { 0 }
            ),
        )?;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
        let output = f.add()?;
        assert_eq!(output.status.code(), Some(if failure { 23 } else { 0 }));
        let contents = fs::read_to_string(f.destination().join("hook-output"))?;
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.starts_with(&"0".repeat(40)));
        assert!(contents.ends_with(" 1\n"));
        assert!(f.destination().join(".git").is_file());
        assert!(!f.source.join("hook-output").exists());
        f.assert_no_sibling_stages()?;
        if failure {
            let stages = f.stages()?;
            assert_eq!(stages.len(), 1);
            assert_eq!(fs::metadata(&stages[0])?.permissions().mode() & 0o077, 0);
        } else {
            assert!(f.stages()?.is_empty());
        }
    }
    Ok(())
}

#[test]
fn filters_and_sparse_checkouts_delegate() -> Result<()> {
    for sparse in [false, true] {
        let f = Fixture::new()?;
        if sparse {
            git(&f.source, &["sparse-checkout", "init", "--cone"])?;
        } else {
            fs::write(f.source.join(".gitattributes"), "*.txt ident\n")?;
            git(&f.source, &["add", "."])?;
            git(&f.source, &["commit", "-m", "attributes"])?;
        }
        let output = checked(f.add()?)?;
        assert!(!String::from_utf8_lossy(&output.stderr).contains("CoW:"));
        clean(&f.destination())?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn respects_restrictive_umask() -> Result<()> {
    use std::os::unix::{fs::PermissionsExt, process::CommandExt};
    let f = Fixture::new()?;
    let mut child = command(env!("CARGO_BIN_EXE_git-twig"), &f.source);
    child.args(["new", "--detach", "../feature"]);
    // SAFETY: umask is async-signal-safe, affects only this forked child, and captures nothing.
    unsafe {
        child.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    checked(child.output()?)?;
    assert_eq!(
        fs::metadata(f.destination().join("hello.txt"))?
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    clean(&f.destination())
}

#[test]
fn empty_initial_commit_creates_a_clean_empty_worktree() -> Result<()> {
    let f = Fixture::new()?;
    let source = f.root.join("empty");
    fs::create_dir(&source)?;
    git(&source, &["init", "-b", "main"])?;
    git(
        &source,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "empty",
        ],
    )?;
    let output = checked(twig(&source, &["new", "--detach", "../empty-worktree"])?)?;
    native(&output);
    let destination = f.root.join("empty-worktree");
    clean(&destination)?;
    assert!(git(&destination, &["ls-files"])?.stdout.is_empty());
    Ok(())
}

#[cfg(unix)]
#[test]
fn auto_lf_preserves_committed_text_and_binary_bytes_natively() -> Result<()> {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
    let f = Fixture::with_object_format("sha256")?;
    let weird_name = Path::new(OsStr::from_bytes(if cfg!(target_os = "macos") {
        b"tree/odd\tname\n\".bin"
    } else {
        b"tree/odd\tname\n\"\xff.bin"
    }));
    let binary = b"first\0\r\nsecond\n";
    fs::create_dir(f.source.join("tree"))?;
    fs::write(f.source.join(weird_name), binary)?;
    fs::write(f.source.join("tree/.gitattributes"), "*.bin eol=lf\n")?;
    fs::write(f.source.join("tree/reset.txt"), "reset\n")?;
    fs::write(f.source.join("tree/unset.txt"), "unset\n")?;
    git(&f.source, &["config", "core.eol", "crlf"])?;
    commit_attributes(
        &f,
        "[attr]lf text=auto eol=lf\n* lf\ntree/*.bin !eol\n\
         tree/reset.txt !text !eol\ntree/unset.txt -text -eol\n",
    )?;

    let output = checked(f.add()?)?;
    native(&output);
    #[cfg(target_os = "linux")]
    if linux_reflinks_available(&f)? {
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Linux reflink CoW: verified"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(fs::read(f.destination().join("hello.txt"))?, b"hello\n");
    assert_eq!(fs::read(f.destination().join(weird_name))?, binary);
    assert_eq!(
        fs::read(f.destination().join("tree/reset.txt"))?,
        b"reset\n"
    );
    assert_eq!(
        fs::read(f.destination().join("tree/unset.txt"))?,
        b"unset\n"
    );
    clean(&f.destination())?;
    fs::write(f.destination().join(weird_name), b"private")?;
    assert_eq!(fs::read(f.source.join(weird_name))?, binary);
    Ok(())
}

#[test]
fn auto_lf_delegates_when_git_clean_source_bytes_differ_from_blob() -> Result<()> {
    let f = Fixture::new()?;
    fs::create_dir(f.source.join("tree"))?;
    fs::write(f.source.join("tree/file.txt"), "hello\n")?;
    commit_attributes(&f, "*.txt text=auto eol=lf\n")?;
    let dirty_path = "tree/file.txt";
    fs::write(f.source.join(dirty_path), "hello\r\n")?;
    assert_eq!(
        git(&f.source, &["hash-object", dirty_path])?.stdout,
        git(&f.source, &["rev-parse", "HEAD:tree/file.txt"])?.stdout
    );

    let output = checked(f.add()?)?;
    let trace = String::from_utf8_lossy(&output.stderr);
    assert!(trace.contains("stock Git:"), "{trace}");
    #[cfg(target_os = "linux")]
    let reflinks = linux_reflinks_available(&f)?;
    #[cfg(not(target_os = "linux"))]
    let reflinks = cfg!(target_os = "macos");
    if reflinks {
        assert!(
            trace
                .contains("stock Git: cloned bytes do not match the committed blob: tree/file.txt"),
            "{trace}"
        );
    }
    assert_eq!(fs::read(f.destination().join(dirty_path))?, b"hello\n");
    assert_eq!(fs::read(f.source.join(dirty_path))?, b"hello\r\n");
    clean(&f.destination())
}

#[test]
fn auto_lf_checkout_matches_git_when_history_contains_crlf() -> Result<()> {
    let f = Fixture::new()?;
    let historical = b"first\r\nsecond\n";
    fs::write(f.source.join("hello.txt"), historical)?;
    git(&f.source, &["add", "hello.txt"])?;
    fs::write(f.source.join(".gitattributes"), "*.txt text=auto eol=lf\n")?;
    git(&f.source, &["add", ".gitattributes"])?;
    git(&f.source, &["commit", "-m", "retain historical CRLF blob"])?;
    assert_eq!(
        git(&f.source, &["show", "HEAD:hello.txt"])?.stdout,
        historical
    );
    git(
        &f.source,
        &["worktree", "add", "--detach", "../stock", "HEAD"],
    )?;

    let output = checked(f.add()?)?;
    native(&output);
    assert_eq!(
        fs::read(f.destination().join("hello.txt"))?,
        fs::read(f.root.join("stock/hello.txt"))?
    );
    clean(&f.destination())
}

#[test]
fn auto_lf_attributes_must_apply_to_the_same_path() -> Result<()> {
    let f = Fixture::new()?;
    git(&f.source, &["config", "core.eol", "crlf"])?;
    commit_attributes(&f, "*.txt text=auto\n.gitignore eol=lf\n")?;

    let output = checked(f.add()?)?;
    assert!(String::from_utf8_lossy(&output.stderr).contains("stock Git:"));
    assert_eq!(fs::read(f.destination().join("hello.txt"))?, b"hello\r\n");
    assert_eq!(fs::read(f.source.join("hello.txt"))?, b"hello\n");
    clean(&f.destination())
}

#[test]
fn auto_lf_does_not_allow_ident_expansion() -> Result<()> {
    let f = Fixture::new()?;
    fs::write(f.source.join("hello.txt"), "$Id$\n")?;
    commit_attributes(&f, "*.txt text=auto eol=lf ident\n")?;
    assert_destination_ident(&f, &f.destination(), f.add()?)
}

#[test]
fn auto_lf_delegates_for_named_filters() -> Result<()> {
    for driver in ["prefix", "unset", "unspecified"] {
        let f = Fixture::new()?;
        git(
            &f.source,
            &[
                "config",
                &format!("filter.{driver}.smudge"),
                "sed 's/^/prefix: /'",
            ],
        )?;
        git(
            &f.source,
            &[
                "config",
                &format!("filter.{driver}.clean"),
                "sed 's/^prefix: //'",
            ],
        )?;
        commit_attributes(&f, &format!("*.txt text=auto eol=lf filter={driver}\n"))?;

        let output = checked(f.add()?)?;
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("stock Git:"),
            "{driver}"
        );
        assert_eq!(
            fs::read(f.destination().join("hello.txt"))?,
            b"prefix: hello\n",
            "{driver}"
        );
        assert_eq!(fs::read(f.source.join("hello.txt"))?, b"hello\n");
        clean(&f.destination())?;
    }
    Ok(())
}

fn commit_attributes(f: &Fixture, rules: &str) -> Result<()> {
    fs::write(f.source.join(".gitattributes"), rules)?;
    git(&f.source, &["add", "."])?;
    git(&f.source, &["commit", "-m", "checkout attributes"])?;
    Ok(())
}

fn auto_lf_ident_fixture() -> Result<Fixture> {
    let f = Fixture::new()?;
    fs::write(f.source.join("hello.txt"), "$Id$\n")?;
    commit_attributes(&f, "*.txt text=auto eol=lf\n")?;
    fs::write(f.root.join("ident.attrs"), "*.txt ident\n")?;
    let attributes = f.root.join("ident.attrs");
    git(
        &f.source,
        &[
            "config",
            "--file",
            "../ident.cfg",
            "core.attributesFile",
            attributes.to_str().expect("fixture path is UTF-8"),
        ],
    )?;
    Ok(f)
}

fn assert_destination_ident(f: &Fixture, destination: &Path, output: Output) -> Result<()> {
    let output = checked(output)?;
    let blob = git(&f.source, &["rev-parse", "HEAD:hello.txt"])?.stdout;
    let expanded = format!("$Id: {} $\n", String::from_utf8(blob)?.trim_end());
    let trace = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        fs::read_to_string(destination.join("hello.txt"))?,
        expanded,
        "{trace}"
    );
    assert!(trace.contains("stock Git:"), "{trace}");
    assert_eq!(fs::read(f.source.join("hello.txt"))?, b"$Id$\n");
    clean(destination)
}

#[test]
fn auto_lf_delegates_for_branch_conditional_config() -> Result<()> {
    let f = auto_lf_ident_fixture()?;
    git(
        &f.source,
        &[
            "config",
            "--file",
            ".git/included.cfg",
            "includeIf.onbranch:feature.path",
            "../../ident.cfg",
        ],
    )?;
    git(&f.source, &["config", "include.path", "included.cfg"])?;
    assert_destination_ident(&f, &f.destination(), f.add()?)
}

#[test]
fn auto_lf_delegates_for_gitdir_conditional_config() -> Result<()> {
    let f = auto_lf_ident_fixture()?;
    let key = format!(
        "includeIf.gitdir:{}/.git/worktrees/.path",
        f.source.display()
    );
    git(&f.source, &["config", &key, "../../ident.cfg"])?;
    let output = f.twig(&["new", "--detach", "../feature"])?;
    assert_destination_ident(&f, &f.destination(), output)
}

#[test]
fn auto_lf_delegates_for_relative_attributes_file() -> Result<()> {
    let f = auto_lf_ident_fixture()?;
    fs::create_dir(f.destination())?;
    fs::copy(
        f.root.join("ident.attrs"),
        f.destination().join("ident.attrs"),
    )?;
    fs::write(f.root.join("ident.attrs"), "")?;
    git(
        &f.source,
        &["config", "core.attributesFile", "../ident.attrs"],
    )?;
    let destination = f.destination().join("checkout");
    let output = f.twig(&["new", "--detach", "../feature/checkout"])?;
    assert_destination_ident(&f, &destination, output)
}

#[test]
fn auto_lf_delegates_for_worktree_config_includes() -> Result<()> {
    let f = auto_lf_ident_fixture()?;
    git(&f.source, &["config", "include.path", "../../ident.cfg"])?;
    git(&f.source, &["config", "extensions.worktreeConfig", "true"])?;
    let empty = f.root.join("empty.attrs");
    fs::write(&empty, "")?;
    git(
        &f.source,
        &[
            "config",
            "--file",
            ".git/override.cfg",
            "core.attributesFile",
            empty.to_str().expect("fixture path is UTF-8"),
        ],
    )?;
    git(
        &f.source,
        &["config", "--worktree", "include.path", "override.cfg"],
    )?;
    assert_destination_ident(&f, &f.destination(), f.add()?)
}

#[test]
fn auto_lf_delegates_when_only_one_attribute_source_is_supported() -> Result<()> {
    let unsupported = "*.txt text=auto eol=crlf\n";
    let supported = "*.txt text=auto eol=lf\n";
    for (committed, working, expected) in [
        (unsupported, supported, b"hello\r\n".as_slice()),
        (supported, unsupported, b"hello\n".as_slice()),
    ] {
        let f = Fixture::new()?;
        fs::create_dir(f.source.join("tree"))?;
        fs::write(f.source.join("tree/hello.txt"), "hello\n")?;
        let attributes = f.source.join("tree/.gitattributes");
        fs::write(&attributes, committed)?;
        commit_attributes(&f, supported)?;
        fs::write(&attributes, working)?;

        let output = checked(f.add()?)?;
        let trace = String::from_utf8_lossy(&output.stderr);
        assert!(
            trace.contains("stock Git: request is outside the CoW fast path"),
            "{trace}"
        );
        assert_eq!(fs::read(f.destination().join("tree/hello.txt"))?, expected);
        assert_eq!(fs::read(&attributes)?, working.as_bytes());
        clean(&f.destination())?;
    }
    Ok(())
}

#[test]
fn line_ending_conversions_delegate() -> Result<()> {
    let f = Fixture::new()?;
    git(&f.source, &["config", "core.autocrlf", "true"])?;
    let output = checked(f.add()?)?;
    assert!(!String::from_utf8_lossy(&output.stderr).contains("CoW:"));
    assert_eq!(fs::read(f.destination().join("hello.txt"))?, b"hello\r\n");
    clean(&f.destination())
}

#[test]
fn branch_already_in_use_is_rejected_without_source_changes() -> Result<()> {
    let f = Fixture::new()?;
    assert!(
        !f.twig(&["new", "-b", "main", "../feature"])?
            .status
            .success()
    );
    assert!(!f.destination().exists());
    clean(&f.source)
}

#[test]
fn relative_destination_from_subdirectory_is_resolved_correctly() -> Result<()> {
    let f = Fixture::new()?;
    fs::create_dir(f.source.join("nested"))?;
    let output = checked(twig(
        &f.source.join("nested"),
        &["new", "--detach", "../../feature"],
    )?)?;
    native(&output);
    clean(&f.destination())
}

#[test]
fn sixteen_independent_native_creators_register_safely() -> Result<()> {
    let f = Fixture::new()?;
    #[cfg(target_os = "linux")]
    if !linux_reflinks_available(&f)? {
        return Ok(());
    }
    let handles: Vec<_> = (0..16)
        .map(|index| {
            let source = f.source.clone();
            std::thread::spawn(move || {
                twig(
                    &source,
                    &["new", "--detach", &format!("../parallel-{index}")],
                )
            })
        })
        .collect();
    for (index, handle) in handles.into_iter().enumerate() {
        let output = checked(handle.join().unwrap()?)?;
        native(&output);
        clean(&f.root.join(format!("parallel-{index}")))?;
    }
    Ok(())
}

#[test]
fn concurrent_creators_do_not_overwrite_each_other() -> Result<()> {
    let f = Fixture::new()?;
    // This is a native registrar guarantee. Stock Git can reuse a concurrent
    // creator's empty directory; delegated operations retain Git's semantics.
    #[cfg(target_os = "linux")]
    if !linux_reflinks_available(&f)? {
        return Ok(());
    }
    let a = f.source.clone();
    let b = a.clone();
    let first = std::thread::spawn(move || twig(&a, &["new", "--detach", "../feature"]));
    let second = std::thread::spawn(move || twig(&b, &["new", "--detach", "../feature"]));
    let a = first.join().unwrap()?;
    let b = second.join().unwrap()?;
    assert_ne!(a.status.success(), b.status.success());
    clean(&f.destination())
}

#[cfg(unix)]
#[test]
fn directory_clones_filter_extras_without_following_links() -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    let f = Fixture::new()?;
    let tree = f.source.join("tree");
    fs::create_dir(&tree)?;
    fs::write(tree.join("a"), "same\n")?;
    fs::hard_link(tree.join("a"), tree.join("b"))?;
    symlink("a", tree.join("link"))?;
    git(&f.source, &["add", "."])?;
    git(&f.source, &["commit", "-m", "nested tree"])?;
    fs::create_dir_all(tree.join("ignored/deep"))?;
    fs::write(tree.join("ignored/deep/secret"), "excluded")?;
    fs::set_permissions(tree.join("ignored/deep"), fs::Permissions::from_mode(0o555))?;
    fs::set_permissions(tree.join("ignored"), fs::Permissions::from_mode(0o555))?;
    fs::write(tree.join("untracked"), "excluded")?;
    let outside = f.root.join("outside");
    fs::create_dir(&outside)?;
    fs::write(outside.join("keep"), "untouched")?;
    symlink(&outside, tree.join("escape"))?;
    let before = git(&f.source, &["status", "--porcelain"])?.stdout;
    let output = checked(f.add()?)?;
    native(&output);
    if cfg!(target_os = "macos") {
        assert!(String::from_utf8_lossy(&output.stderr).contains("APFS directory clones: 1"));
        assert!(f.stages()?.is_empty());
        f.assert_no_sibling_stages()?;
    }
    let destination = f.destination().join("tree");
    assert!(!destination.join("ignored").exists());
    assert!(!destination.join("untracked").exists());
    assert!(fs::symlink_metadata(destination.join("escape")).is_err());
    assert_eq!(fs::read_link(destination.join("link"))?, Path::new("a"));
    assert_ne!(
        fs::metadata(destination.join("a"))?.ino(),
        fs::metadata(destination.join("b"))?.ino()
    );
    clean(&f.destination())?;
    fs::write(destination.join("a"), "private")?;
    assert_eq!(fs::read(destination.join("b"))?, b"same\n");
    assert_eq!(fs::read(tree.join("a"))?, b"same\n");
    assert_eq!(fs::read(outside.join("keep"))?, b"untouched");
    assert_eq!(git(&f.source, &["status", "--porcelain"])?.stdout, before);
    Ok(())
}

#[cfg(unix)]
#[test]
fn directory_clone_permissions_follow_umask_not_source_modes() -> Result<()> {
    use std::os::unix::{fs::PermissionsExt, process::CommandExt};
    let f = Fixture::new()?;
    let tree = f.source.join("tree");
    fs::create_dir(&tree)?;
    fs::write(tree.join("file"), "read only")?;
    fs::write(tree.join("exec"), "#!/bin/sh\n")?;
    fs::set_permissions(tree.join("exec"), fs::Permissions::from_mode(0o755))?;
    git(&f.source, &["add", "."])?;
    git(&f.source, &["commit", "-m", "modes"])?;
    fs::set_permissions(tree.join("file"), fs::Permissions::from_mode(0o444))?;
    fs::set_permissions(&tree, fs::Permissions::from_mode(0o555))?;
    let mut child = command(env!("CARGO_BIN_EXE_git-twig"), &f.source);
    child.args(["new", "--detach", "../feature"]);
    // SAFETY: async-signal-safe, changes only the forked child's umask.
    unsafe {
        child.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    let output = checked(child.output()?)?;
    native(&output);
    for (name, expected) in [("tree", 0o700), ("tree/file", 0o600), ("tree/exec", 0o700)] {
        assert_eq!(
            fs::metadata(f.destination().join(name))?
                .permissions()
                .mode()
                & 0o777,
            expected
        );
    }
    assert_eq!(fs::metadata(&tree)?.permissions().mode() & 0o777, 0o555);
    assert_eq!(
        fs::metadata(tree.join("file"))?.permissions().mode() & 0o777,
        0o444
    );
    clean(&f.destination())
}

#[cfg(unix)]
#[test]
fn replaced_directory_symlink_delegates_without_traversing_it() -> Result<()> {
    use std::os::unix::fs::symlink;
    let f = Fixture::new()?;
    fs::create_dir_all(f.source.join("tree/sub"))?;
    fs::write(f.source.join("tree/sub/file"), "committed")?;
    git(&f.source, &["add", "."])?;
    git(&f.source, &["commit", "-m", "directory"])?;
    fs::rename(f.source.join("tree/sub"), f.root.join("outside"))?;
    symlink(f.root.join("outside"), f.source.join("tree/sub"))?;
    let output = checked(f.add()?)?;
    assert!(!String::from_utf8_lossy(&output.stderr).contains("CoW: verified"));
    assert_eq!(
        fs::read(f.destination().join("tree/sub/file"))?,
        b"committed"
    );
    assert!(fs::symlink_metadata(f.source.join("tree/sub"))?.is_symlink());
    assert_eq!(fs::read(f.root.join("outside/file"))?, b"committed");
    clean(&f.destination())
}
