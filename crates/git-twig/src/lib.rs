//! Native CoW materialization with Git-owned linked-worktree bookkeeping.
mod native;
mod snapshot;
mod staging;
mod tree;

use anyhow::{Context, Result, ensure};
use std::{
    env,
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
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
            Ok(Some(prepared)) => return install(cwd, args, prepared),
            Ok(None) => trace("stock Git: request is outside the CoW fast path"),
            Err(error) => trace(&format!("stock Git: {error:#}")),
        }
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
}

fn prepare(
    cwd: &Path,
    request: &AddRequest,
    staging: &mut staging::Staging,
) -> Result<Option<Prepared>> {
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return Ok(None);
    }
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_COMMON_DIR",
        "GIT_ATTR_SOURCE",
        "GIT_CONFIG",
    ] {
        if env::var_os(key).is_some() {
            return Ok(None);
        }
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
    if commit != head {
        return Ok(None);
    }
    if config_true(&root, "core.sparseCheckout") || config_true(&root, "core.sparseCheckoutCone") {
        return Ok(None);
    }
    let destination = if request.destination.is_absolute() {
        request.destination.clone()
    } else {
        cwd.join(&request.destination)
    };
    if destination.try_exists()? || fs::symlink_metadata(&destination).is_ok() {
        return Ok(None);
    }
    let parent = destination.parent().context("destination has no parent")?;
    if !parent.is_dir() {
        return Ok(None);
    }
    let parent = fs::canonicalize(parent)?;
    let destination = parent.join(destination.file_name().context("destination has no name")?);
    let autocrlf = git(&root)
        .args(["config", "--get", "core.autocrlf"])
        .output()?;
    if autocrlf.status.success() && autocrlf.stdout != b"false\n" {
        return Ok(None);
    }
    let entries =
        tree::parse(&checked(git(&root).args(["ls-tree", "-rz", "--full-tree", &commit]))?.stdout)?;
    if entries.iter().any(|entry| entry.kind == Kind::Submodule) {
        return Ok(None);
    }
    // These semantics are owned by Git, not by the CoW materializer.
    if git(&root)
        .args(["config", "--bool", "--get", "core.symlinks"])
        .output()?
        .stdout
        == b"false\n"
    {
        return Ok(None);
    }
    // Reference/index hooks run while Git registers or initializes the worktree.
    // They may themselves call Twig: never hold our registrar lock across them.
    let hook = output_path(checked(git(&root).args(["rev-parse", "--git-path", "hooks"]))?.stdout)?;
    let configured_hooks = git(&root)
        .args(["config", "--get-regexp", r"^hook\..*\.event$"])
        .output()?;
    let hooks = root.join(hook);
    if ["reference-transaction", "post-index-change"]
        .iter()
        .any(|event| fs::symlink_metadata(hooks.join(event)).is_ok())
        || configured_hooks
            .stdout
            .split(|byte| byte.is_ascii_whitespace())
            .any(|word| word == b"reference-transaction" || word == b"post-index-change")
    {
        return Ok(None);
    }
    let Some(auto_lf) = checkout_attributes(&root, &commit, &entries)? else {
        return Ok(None);
    };
    let stage = staging.create(&root, &parent)?;
    let data = stage.join("data");
    fs::create_dir(&data)?;
    let modes = native::FileModes::new(&stage)?;
    let directories = snapshot::materialize(&root, &data, &entries, &modes)?;
    #[cfg(target_os = "macos")]
    trace(&format!("APFS directory clones: {directories}"));
    #[cfg(not(target_os = "macos"))]
    let _ = directories;
    verify_raw_blobs(&root, &data, &entries, auto_lf)?;
    verify(&root, &data, &stage.join("index"), &commit)?;
    Ok(Some(Prepared {
        root,
        destination,
        stage,
        commit,
    }))
}

fn verify(root: &Path, data: &Path, index: &Path, commit: &str) -> Result<()> {
    let git_dir =
        output_path(checked(git(root).args(["rev-parse", "--absolute-git-dir"]))?.stdout)?;
    let staged_git = || {
        let mut command = git(root);
        command
            .env("GIT_DIR", &git_dir)
            .env("GIT_WORK_TREE", data)
            .env("GIT_INDEX_FILE", index)
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.sparseCheckout=false",
                "-c",
                "core.filemode=true",
                "-c",
                "core.symlinks=true",
            ]);
        command
    };
    checked(staged_git().args(["read-tree", commit]))?;
    // Refresh hashes files with empty stat caches; raw diff-files alone can report stat-only changes.
    checked(staged_git().args(["update-index", "--really-refresh"]))
        .context("cloned files do not match the requested commit")?;
    checked(staged_git().args([
        "diff-files",
        "--quiet",
        "--no-ext-diff",
        "--ignore-submodules=none",
    ]))
    .context("cloned files do not match the requested commit")?;
    Ok(())
}

fn install(cwd: &Path, args: &[OsString], prepared: Prepared) -> Result<i32> {
    #[cfg(target_os = "macos")]
    trace("APFS CoW: verified tracked files");
    #[cfg(target_os = "linux")]
    trace("Linux reflink CoW: verified tracked files");
    // Git enumerates peer worktree metadata while registering. Serialize Twig registrars
    // so none observes a peer between mkdir and writing commondir/HEAD.
    let registration_lock = lock_registration(&prepared.root)?;
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
    for entry in fs::read_dir(prepared.stage.join("data"))? {
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

fn checkout_attributes(root: &Path, commit: &str, entries: &[Entry]) -> Result<Option<bool>> {
    if !checkout_config_is_stable(root)? {
        return Ok(None);
    }
    let mut input = Vec::new();
    for entry in entries {
        let name = entry.path.file_name().expect("parsed tree path has a name");
        // Case-insensitive filesystems can resolve names Git's tree lookup would miss.
        if name.eq_ignore_ascii_case(".gitattributes")
            && (name != ".gitattributes" || !matches!(entry.kind, Kind::File { .. }))
        {
            return Ok(None);
        }
        input.extend(native::path_bytes(&entry.path));
        input.push(0);
    }
    let mut command = git(root);
    command.args(["check-attr", "-z", "--stdin", "--all"]);
    let mut auto_lf = false;
    for source in [None, Some(commit)] {
        if let Some(commit) = source {
            command.args(["--source", commit]);
        }
        let output = git_with_input(&mut command, &input)?;
        let mut fields: Vec<_> = output.split(|byte| *byte == 0).collect();
        ensure!(
            fields.pop() == Some(b"".as_slice()),
            "unterminated Git attributes"
        );
        let (records, remainder) = fields.as_chunks::<3>();
        ensure!(remainder.is_empty(), "incomplete Git attribute record");
        for path in records.chunk_by(|a, b| a[0] == b[0]) {
            let text = path
                .iter()
                .any(|row| row[1] == b"text" && row[2] == b"auto");
            let eol = path.iter().any(|row| row[1] == b"eol" && row[2] == b"lf");
            if text != eol || has_conversion_attributes(path) {
                return Ok(None);
            }
            auto_lf |= text;
        }
    }
    Ok(Some(auto_lf))
}

fn has_conversion_attributes(attributes: &[[&[u8]; 3]]) -> bool {
    attributes
        .iter()
        .any(|[_, name, value]| match (*name, *value) {
            // --all omits unspecified attributes. A literal filter name can still
            // look like Git's "unset" output, so defer either form to Git.
            (b"filter" | b"working-tree-encoding", _) => true,
            (b"ident" | b"crlf" | b"text" | b"eol", b"unset")
            | (b"text", b"auto")
            | (b"eol", b"lf") => false,
            (b"ident" | b"crlf" | b"text" | b"eol", _) => true,
            _ => false,
        })
}

fn checkout_config_is_stable(root: &Path) -> Result<bool> {
    // Git resolves relative config and attribute files from its checkout cwd.
    if [
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "HOME",
        "XDG_CONFIG_HOME",
    ]
    .iter()
    .filter_map(env::var_os)
    .any(|path| !path.is_empty() && !Path::new(&path).is_absolute())
    {
        return Ok(false);
    }

    // Includes can become active after registration. A copied config.worktree
    // can resolve includes from a different Git directory.
    let config = git(root)
        .args([
            "config",
            "--includes",
            "--null",
            "--type=path",
            "--get-regexp",
            r"^(includeif\..*\.path|extensions\.worktreeconfig|core\.attributesfile)$",
        ])
        .output()?;
    if config.status.code() == Some(1) {
        return Ok(true);
    }
    let Some(records) = config.stdout.strip_suffix(b"\0") else {
        return Ok(false);
    };
    let mut attributes_file = None;
    for record in records.split(|byte| *byte == 0) {
        let Some(path) = record.strip_prefix(b"core.attributesfile\n") else {
            return Ok(false);
        };
        attributes_file = Some(path);
    }
    Ok(config.status.success()
        && attributes_file
            .is_some_and(|path| Path::new(&native::os_string(path.to_vec())).is_absolute()))
}

fn verify_raw_blobs(root: &Path, data: &Path, entries: &[Entry], auto_lf: bool) -> Result<()> {
    // eol=lf leaves committed bytes unchanged at checkout. Git's clean
    // conversion can hide a CRLF copy, so bypass filters when comparing.
    let files = || {
        entries.iter().filter(|entry| {
            matches!(entry.kind, Kind::File { .. })
                && (auto_lf
                    || entry
                        .path
                        .file_name()
                        .is_some_and(|name| name == ".gitattributes"))
        })
    };
    let mut input = Vec::new();
    for entry in files() {
        quote_hash_path(&data.join(&entry.path), &mut input);
    }
    if input.is_empty() {
        return Ok(());
    }
    let output = git_with_input(
        git(root).args(["hash-object", "--no-filters", "--stdin-paths"]),
        &input,
    )?;
    let mut hashes = output
        .strip_suffix(b"\n")
        .context("unterminated Git blob hashes")?
        .split(|byte| *byte == b'\n');
    for entry in files() {
        ensure!(
            hashes.next() == Some(entry.object_id.as_bytes()),
            "cloned bytes do not match the committed blob: {}",
            entry.path.display()
        );
    }
    ensure!(
        hashes.next().is_none(),
        "Git reported an unexpected blob hash"
    );
    Ok(())
}

fn quote_hash_path(path: &Path, input: &mut Vec<u8>) {
    // hash-object --stdin-paths accepts Git's C-quoted paths, one per line.
    input.push(b'"');
    for byte in native::path_bytes(path) {
        match byte {
            b'\\' | b'"' => input.extend_from_slice(&[b'\\', byte]),
            b' '..=b'~' => input.push(byte),
            _ => input.extend_from_slice(&[
                b'\\',
                b'0' + (byte >> 6),
                b'0' + ((byte >> 3) & 7),
                b'0' + (byte & 7),
            ]),
        }
    }
    input.extend_from_slice(b"\"\n");
}

fn git_with_input(command: &mut Command, input: &[u8]) -> Result<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot execute Git")?;
    // Drain output concurrently so a full pipe cannot deadlock the writer.
    let mut stdin = child.stdin.take().context("missing Git stdin")?;
    let (output, written) = std::thread::scope(|scope| {
        let writer = scope.spawn(move || stdin.write_all(input));
        (child.wait_with_output(), writer.join())
    });
    let output = output?;
    ensure!(
        output.status.success(),
        "Git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    written.map_err(|_| anyhow::anyhow!("Git input writer panicked"))??;
    Ok(output.stdout)
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
