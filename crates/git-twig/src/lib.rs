//! Native CoW materialization with Git-owned linked-worktree bookkeeping.
mod checkout;
mod native;
mod objects;
mod snapshot;
mod staging;
mod tree;

use anyhow::{Context, Result, ensure};
use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tree::{Entry, Kind};

/// Execute supported creation requests natively; delegate everything else to Git.
pub fn run(cwd: &Path, args: &[OsString]) -> Result<i32> {
    ensure!(
        args.first().is_none_or(|arg| arg != "add"),
        "\'add\' has been removed; use \'git twig new\'"
    );
    // Both native preparation and fallback consume Git worktree arguments.
    let mut git_args = args.to_vec();
    if git_args.first().is_some_and(|arg| arg == "new") {
        git_args[0] = "add".into();
    }
    let args = &git_args;
    let mut staging = staging::Staging::default();
    let result = run_request(cwd, args, &mut staging);
    if matches!(result, Ok(0)) {
        staging.cleanup();
    }
    result
}

fn run_request(cwd: &Path, args: &[OsString], staging: &mut staging::Staging) -> Result<i32> {
    if let Some(request) = AddRequest::parse(args) {
        match prepare(cwd, &request, staging) {
            Ok(prepared) => return install(cwd, args, prepared),
            Err(error) => trace(&format!("stock Git: {error:#}")),
        }
    } else if args.first().is_some_and(|arg| arg == "add") {
        trace(
            "stock Git: creation options require Git; native creation supports -b or --detach, a destination, and an optional same-HEAD revision",
        );
    }
    Ok(git(cwd)
        .arg("worktree")
        .args(args)
        .status()?
        .code()
        .unwrap_or(1))
}

fn trace(message: &str) {
    if env::var_os("TWIG_TRACE").is_some() {
        eprintln!("git-twig: {message}");
    }
}

#[derive(Debug)]
struct AddRequest {
    destination: PathBuf,
    revision: OsString,
}

impl AddRequest {
    fn parse(args: &[OsString]) -> Option<Self> {
        if args.first()? != "add" {
            return None;
        }
        let mut index = 1;
        if args.get(index)? == "-b" {
            let branch = args.get(index + 1)?;
            if branch.is_empty() || branch.to_string_lossy().starts_with('-') {
                return None;
            }
            index += 2;
        } else if args.get(index)? == "--detach" {
            index += 1;
        } else {
            // Omitted branch has Git's branch-name inference semantics. Delegate for now.
            return None;
        }
        let destination = args.get(index)?;
        if destination.is_empty() || destination.to_string_lossy().starts_with('-') {
            return None;
        }
        index += 1;
        let revision = args.get(index).cloned().unwrap_or_else(|| "HEAD".into());
        if revision.to_str().is_none()
            || revision.to_string_lossy().starts_with('-')
            || args.len() > index + 1
        {
            return None;
        }
        Some(Self {
            destination: destination.into(),
            revision,
        })
    }
}

struct Prepared {
    root: PathBuf,
    destination: PathBuf,
    stage: PathBuf,
    commit: String,
    entries: Vec<Entry>,
    policy: checkout::Policy,
}

fn prepare(cwd: &Path, request: &AddRequest, staging: &mut staging::Staging) -> Result<Prepared> {
    ensure!(
        cfg!(any(target_os = "macos", target_os = "linux")),
        "native CoW is supported only on macOS and Linux"
    );
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_COMMON_DIR",
        "GIT_ATTR_SOURCE",
        "GIT_REPLACE_REF_BASE",
        // git config honors this override, but checkout commands do not.
        "GIT_CONFIG",
    ] {
        ensure!(
            env::var_os(key).is_none(),
            "environment override {key} requires stock Git"
        );
    }
    let root = output_path(checked(git(cwd).args(["rev-parse", "--show-toplevel"]))?.stdout)?;
    let commit = text(
        checked(
            git(&root)
                .arg("rev-parse")
                .arg("--verify")
                .arg(format!("{}^{{commit}}", request.revision.to_string_lossy())),
        )?
        .stdout,
    )?;
    let head = text(checked(git(&root).args(["rev-parse", "HEAD"]))?.stdout)?;
    ensure!(
        commit == head,
        "requested revision differs from source HEAD"
    );
    ensure!(
        !config_true(&root, "core.sparseCheckout")
            && !config_true(&root, "core.sparseCheckoutCone"),
        "sparse checkout requires stock Git"
    );
    let destination = if request.destination.is_absolute() {
        request.destination.clone()
    } else {
        cwd.join(&request.destination)
    };
    ensure!(
        !destination.try_exists()? && fs::symlink_metadata(&destination).is_err(),
        "destination already exists: {:?}",
        destination
    );
    let parent = destination.parent().context("destination has no parent")?;
    ensure!(
        parent.is_dir(),
        "destination parent does not exist: {parent:?}"
    );
    let parent = fs::canonicalize(parent)?;
    let destination = parent.join(destination.file_name().context("destination has no name")?);
    objects::ensure_literal_objects(&root)?;
    checkout::ensure_relocatable_worktree_config(&root)?;
    let entries =
        tree::parse(&checked(git(&root).args(["ls-tree", "-rz", "--full-tree", &commit]))?.stdout)?;
    ensure!(
        !entries.iter().any(|entry| entry.kind == Kind::Submodule),
        "submodules require stock Git"
    );
    // These semantics are owned by Git, not by the CoW materializer.
    if git(&root)
        .args(["config", "--bool", "--get", "core.symlinks"])
        .output()?
        .stdout
        == b"false\n"
    {
        anyhow::bail!("core.symlinks=false requires stock Git");
    }
    // Reference/index hooks run while Git registers or initializes the worktree.
    // They may themselves call Twig: never hold our registrar lock across them.
    let hook = output_path(checked(git(&root).args(["rev-parse", "--git-path", "hooks"]))?.stdout)?;
    let configured_hooks = git(&root)
        .args(["config", "--get-regexp", r"^hook\..*\.event$"])
        .output()?;
    let hooks = root.join(hook);
    for event in ["reference-transaction", "post-index-change"] {
        let configured = configured_hooks
            .stdout
            .split(|byte| byte.is_ascii_whitespace())
            .any(|word| word == event.as_bytes());
        ensure!(
            !configured && fs::symlink_metadata(hooks.join(event)).is_err(),
            "{event} hook requires stock Git to avoid reentrant registration locks"
        );
    }
    let stage = staging.create(&root, &parent)?;
    let data = stage.join("data");
    fs::create_dir(&data)?;
    let index = stage.join("index");
    let checkout = checkout::Checkout::new(&root, &data, &index)?;
    checkout.initialize(&commit)?;
    let policy = checkout.policy(&entries)?;
    let modes = native::FileModes::new(&stage)?;
    let directories = snapshot::materialize(&root, &data, &entries, &modes)?;
    #[cfg(target_os = "macos")]
    trace(&format!("APFS directory clones: {directories}"));
    #[cfg(not(target_os = "macos"))]
    let _ = directories;
    checkout.repair(&root, &entries, &policy)?;
    Ok(Prepared {
        root,
        destination,
        stage,
        commit,
        entries,
        policy,
    })
}

fn install(cwd: &Path, args: &[OsString], prepared: Prepared) -> Result<i32> {
    #[cfg(target_os = "macos")]
    trace("APFS CoW: verified tracked files");
    #[cfg(target_os = "linux")]
    trace("Linux reflink CoW: verified tracked files");
    // Git enumerates peer worktree metadata while registering. Serialize Twig registrars
    // so none observes a peer between mkdir and writing commondir/HEAD.
    let registration_lock = lock_registration(&prepared.root)?;
    let data = prepared.stage.join("data");
    let index = prepared.stage.join("index");
    prepared.policy.ensure_same(
        &checkout::Checkout::new(&prepared.root, &data, &index)?.policy(&prepared.entries)?,
    )?;
    // Preserve Git's branch validation and registration. No fallback after this mutation.
    let status = git(cwd)
        .args(["worktree", "add", "--no-checkout"])
        .args(&args[1..])
        .status()?;
    if !status.success() {
        return Ok(status.code().unwrap_or(1));
    }
    finish_install(&prepared).with_context(|| {
        format!(
            "partial worktree retained at {}; staging retained at {}; inspect before retrying",
            prepared.destination.display(),
            prepared.stage.display()
        )
    })?;
    // Do not hold the registry lock across user hooks (which may invoke Twig again).
    drop(registration_lock);
    let zero = "0".repeat(prepared.commit.len());
    let code = git(&prepared.destination)
        .args([
            "hook",
            "run",
            "--ignore-missing",
            "post-checkout",
            "--",
            &zero,
            &prepared.commit,
            "1",
        ])
        .status()?
        .code()
        .unwrap_or(1);
    Ok(code)
}

fn state_directory(root: &Path) -> Result<PathBuf> {
    let common_dir = output_path(
        checked(git(root).args(["rev-parse", "--path-format=absolute", "--git-common-dir"]))?
            .stdout,
    )?;
    let state_dir = common_dir.join("twigs");
    fs::create_dir_all(&state_dir)?;
    Ok(state_dir)
}

fn lock_registration(root: &Path) -> Result<fs::File> {
    let state_dir = state_directory(root)?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(state_dir.join("registration.lock"))?;
    lock.lock()
        .context("cannot lock Git worktree registration")?;
    Ok(lock)
}

fn finish_install(prepared: &Prepared) -> Result<()> {
    // The ref may have moved while staging; never install files for a different HEAD.
    let actual = text(checked(git(&prepared.destination).args(["rev-parse", "HEAD"]))?.stdout)?;
    ensure!(
        actual == prepared.commit,
        "destination HEAD moved during creation"
    );
    let data = prepared.stage.join("data");
    let index = prepared.stage.join("index");
    prepared.policy.ensure_same(
        &checkout::Checkout::new(&prepared.destination, &data, &index)?
            .policy(&prepared.entries)?,
    )?;
    for entry in fs::read_dir(&data)? {
        let entry = entry?;
        let target = prepared.destination.join(entry.file_name());
        ensure!(
            fs::symlink_metadata(&target).is_err(),
            "destination changed during installation"
        );
        fs::rename(entry.path(), target)?;
    }
    checked(git(&prepared.destination).args(["read-tree", &prepared.commit]))?;
    trace(&format!("shared repository: {}", prepared.root.display()));
    Ok(())
}

fn config_true(root: &Path, key: &str) -> bool {
    git(root)
        .args(["config", "--bool", "--get", key])
        .output()
        .is_ok_and(|result| result.stdout == b"true\n")
}

fn git(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(cwd);
    command
}

fn checked(command: &mut Command) -> Result<Output> {
    let output = command.output().context("cannot execute Git")?;
    ensure!(
        output.status.success(),
        "Git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

fn text(mut bytes: Vec<u8>) -> Result<String> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    Ok(String::from_utf8(bytes)?)
}

fn output_path(mut bytes: Vec<u8>) -> Result<PathBuf> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    Ok(PathBuf::from(native::os_string(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(words: &[&str]) -> bool {
        AddRequest::parse(&words.iter().map(OsString::from).collect::<Vec<_>>()).is_some()
    }
    #[test]
    fn parses_only_the_supported_subset() {
        assert!(parse(&["add", "-b", "feature", "../feature"]));
        assert!(parse(&["add", "--detach", "../feature", "HEAD"]));
        for words in [
            vec!["add"],
            vec!["add", "-b"],
            vec!["add", "-B", "a", "b"],
            vec!["add", "-b", "a", "b", "HEAD", "extra"],
            vec!["list"],
            vec!["add", "-b", "a", "--help"],
        ] {
            assert!(!parse(&words));
        }
    }
}
