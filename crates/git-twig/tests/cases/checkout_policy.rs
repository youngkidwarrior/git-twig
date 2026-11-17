//! Differential checkout tests: stock Git's raw working tree is the oracle.
use super::*;
use std::io::Write;

const LF: &[u8] = b"hello\n";
const CRLF: &[u8] = b"hello\r\n";

#[test]
fn config_command_override_does_not_hide_checkout_policy() -> Result<()> {
    let f = Fixture::new()?;
    fs::write(f.source.join(".gitattributes"), "*.txt text\n")?;
    git(&f.source, &["add", ".gitattributes"])?;
    git(&f.source, &["commit", "-m", "text checkout policy"])?;
    git(&f.source, &["config", "core.eol", "crlf"])?;
    checked(
        command("git", &f.source)
            .env("GIT_CONFIG", "/dev/null")
            .args(["worktree", "add", "--detach", "../stock", "HEAD"])
            .output()?,
    )?;
    checked(
        command(env!("CARGO_BIN_EXE_git-twig"), &f.source)
            .env("GIT_CONFIG", "/dev/null")
            .args(["new", "-b", "feature", "../feature", "HEAD"])
            .output()?,
    )?;
    let expected = fs::read(f.root.join("stock/hello.txt"))?;
    assert_eq!(expected, CRLF);
    assert_eq!(fs::read(f.destination().join("hello.txt"))?, expected);
    assert_eq!(fs::read(f.source.join("hello.txt"))?, LF);
    Ok(())
}

struct Case {
    name: &'static str,
    attributes: &'static str,
    blob: &'static [u8],
    donor: &'static [u8],
    config: &'static [(&'static str, &'static str)],
}

fn commit_blob(f: &Fixture, path: &str, bytes: &[u8], mode: &str) -> Result<()> {
    let mut child = command("git", &f.source)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(bytes)?;
    let oid = String::from_utf8(checked(child.wait_with_output()?)?.stdout)?;
    git(
        &f.source,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            mode,
            oid.trim(),
            path,
        ],
    )?;
    git(
        &f.source,
        &["commit", "--allow-empty", "-m", "raw target blob"],
    )?;
    Ok(())
}

fn compare_stock(f: &Fixture, paths: &[&str], require_native: bool) -> Result<Output> {
    git(
        &f.source,
        &["worktree", "add", "--detach", "../stock", "HEAD"],
    )?;
    let output = checked(f.add()?)?;
    if require_native {
        native(&output);
    }
    for name in paths {
        assert_eq!(
            fs::read(f.destination().join(name))?,
            fs::read(f.root.join("stock").join(name))?,
            "{name}"
        );
    }
    assert_eq!(
        git(&f.destination(), &["status", "--porcelain"])?.stdout,
        git(&f.root.join("stock"), &["status", "--porcelain"])?.stdout
    );
    Ok(output)
}

#[test]
fn checkout_bytes_match_git_across_line_ending_policies() -> Result<()> {
    let cases = [
        Case {
            name: "auto LF",
            attributes: "* text=auto eol=lf\n",
            blob: LF,
            donor: LF,
            config: &[],
        },
        Case {
            name: "normalized clean CRLF donor",
            attributes: "* text=auto eol=lf\n",
            blob: LF,
            donor: CRLF,
            config: &[],
        },
        Case {
            name: "normalized clean mixed donor",
            attributes: "* text=auto eol=lf\n",
            blob: b"a\nb\n",
            donor: b"a\r\nb\n",
            config: &[],
        },
        Case {
            name: "legacy CRLF blob",
            attributes: "* text=auto eol=lf\n",
            blob: CRLF,
            donor: CRLF,
            config: &[],
        },
        Case {
            name: "forced text legacy CRLF",
            attributes: "* text eol=lf\n",
            blob: CRLF,
            donor: CRLF,
            config: &[],
        },
        Case {
            name: "binary",
            attributes: "* text=auto eol=lf\n",
            blob: b"\0binary\r\n",
            donor: b"\0binary\r\n",
            config: &[],
        },
        Case {
            name: "bare CR",
            attributes: "* text=auto eol=lf\n",
            blob: b"a\rb\n",
            donor: b"a\rb\n",
            config: &[],
        },
        Case {
            name: "empty",
            attributes: "* text=auto eol=lf\n",
            blob: b"",
            donor: b"",
            config: &[],
        },
        Case {
            name: "forced LF",
            attributes: "* text eol=lf\n",
            blob: LF,
            donor: LF,
            config: &[],
        },
        Case {
            name: "CRLF needs checkout",
            attributes: "hello.txt text eol=crlf\n",
            blob: LF,
            donor: LF,
            config: &[],
        },
        Case {
            name: "CRLF already checked out",
            attributes: "hello.txt text eol=crlf\n",
            blob: LF,
            donor: CRLF,
            config: &[],
        },
        Case {
            name: "automatic CRLF mixed blob",
            attributes: "* text=auto eol=crlf\n",
            blob: b"a\r\nb\n",
            donor: b"a\r\nb\n",
            config: &[],
        },
        Case {
            name: "binary disables CRLF",
            attributes: "hello.txt -text eol=crlf\n",
            blob: LF,
            donor: LF,
            config: &[],
        },
        Case {
            name: "literal unset is not -text",
            attributes: "hello.txt text=unset eol=crlf\n",
            blob: LF,
            donor: LF,
            config: &[],
        },
        Case {
            name: "input",
            attributes: "",
            blob: LF,
            donor: CRLF,
            config: &[("core.autocrlf", "input")],
        },
        Case {
            name: "true",
            attributes: "",
            blob: LF,
            donor: LF,
            config: &[("core.autocrlf", "true")],
        },
        Case {
            name: "LF overrides true",
            attributes: "* text=auto eol=lf\n",
            blob: LF,
            donor: LF,
            config: &[("core.autocrlf", "true")],
        },
        Case {
            name: "config CRLF",
            attributes: "* text\n",
            blob: LF,
            donor: LF,
            config: &[("core.eol", "crlf")],
        },
        Case {
            name: "boolean synonym",
            attributes: "* text\n",
            blob: LF,
            donor: LF,
            config: &[("core.autocrlf", "off")],
        },
        Case {
            name: "macro",
            attributes: "[attr]lftext text=auto eol=lf\n* lftext\n",
            blob: LF,
            donor: LF,
            config: &[],
        },
    ];
    for case in cases {
        eprintln!("checkout case: {}", case.name);
        let f = Fixture::new()?;
        for (key, value) in case.config {
            git(&f.source, &["config", key, value])?;
        }
        if !case.attributes.is_empty() {
            fs::write(f.source.join(".gitattributes"), case.attributes)?;
            git(&f.source, &["add", ".gitattributes"])?;
        }
        commit_blob(&f, "hello.txt", case.blob, "100644")?;
        fs::write(f.source.join("hello.txt"), case.donor)?;
        compare_stock(&f, &["hello.txt", ".gitignore"], true)?;
        assert_eq!(fs::read(f.source.join("hello.txt"))?, case.donor);
        fs::write(f.destination().join("hello.txt"), "private")?;
        assert_eq!(fs::read(f.source.join("hello.txt"))?, case.donor);
        assert!(f.stages()?.is_empty());
    }
    Ok(())
}

#[test]
fn target_attributes_ignore_dirty_staged_and_untracked_source_policies() -> Result<()> {
    for kind in ["dirty", "staged", "untracked", "nested-untracked"] {
        let f = Fixture::new()?;
        fs::create_dir_all(f.source.join("tree"))?;
        fs::write(f.source.join("tree/file"), LF)?;
        if matches!(kind, "dirty" | "staged") {
            fs::write(f.source.join(".gitattributes"), "* text=auto eol=lf\n")?;
        }
        git(&f.source, &["add", "."])?;
        git(&f.source, &["commit", "-m", "target policy"])?;
        let policy = if kind == "nested-untracked" {
            "tree/.gitattributes"
        } else {
            ".gitattributes"
        };
        fs::write(f.source.join(policy), "* text eol=crlf\n")?;
        if kind == "staged" {
            git(&f.source, &["add", ".gitattributes"])?;
        }
        fs::write(f.source.join("hello.txt"), CRLF)?;
        fs::write(f.source.join("tree/file"), CRLF)?;
        let before = git(&f.source, &["status", "--porcelain"])?.stdout;
        compare_stock(&f, &["hello.txt", "tree/file"], true)?;
        assert_eq!(git(&f.source, &["status", "--porcelain"])?.stdout, before);
        if matches!(kind, "untracked" | "nested-untracked") {
            assert!(!f.destination().join(policy).exists());
        } else {
            assert_eq!(
                fs::read(f.destination().join(policy))?,
                b"* text=auto eol=lf\n"
            );
        }
    }
    Ok(())
}

#[test]
fn nested_info_global_and_worktree_configuration_match_git() -> Result<()> {
    for kind in ["nested", "info", "global", "worktree"] {
        let f = Fixture::new()?;
        fs::create_dir_all(f.source.join("tree"))?;
        fs::write(f.source.join("tree/file"), LF)?;
        fs::write(f.source.join(".gitattributes"), "* text=auto eol=lf\n")?;
        if kind == "nested" {
            fs::write(f.source.join("tree/.gitattributes"), "file text eol=crlf\n")?;
        }
        git(&f.source, &["add", "."])?;
        git(&f.source, &["commit", "-m", "attributes"])?;
        match kind {
            "info" => fs::write(
                f.source.join(".git/info/attributes"),
                "tree/file text eol=crlf\n",
            )?,
            "global" => {
                // Override root policy's eol with unspecified so global can supply text
                // for an otherwise unclassified path in this dedicated fixture.
                fs::write(
                    f.source.join(".gitattributes"),
                    "hello.txt text=auto eol=lf\n",
                )?;
                git(&f.source, &["add", "."])?;
                git(&f.source, &["commit", "-m", "narrow root policy"])?;
                let global = f.root.join("global-attributes");
                fs::write(&global, "tree/file text eol=crlf\n")?;
                git(
                    &f.source,
                    &["config", "core.attributesFile", global.to_str().unwrap()],
                )?;
            }
            "worktree" => {
                fs::write(f.source.join(".gitattributes"), "* text\n")?;
                git(&f.source, &["add", "."])?;
                git(&f.source, &["commit", "-m", "config selects endings"])?;
                git(&f.source, &["config", "extensions.worktreeConfig", "true"])?;
                git(&f.source, &["config", "core.eol", "lf"])?;
                git(&f.source, &["config", "--worktree", "core.eol", "crlf"])?;
            }
            _ => {}
        }
        compare_stock(&f, &["hello.txt", "tree/file"], true)?;
    }
    Ok(())
}

#[test]
fn copied_worktree_config_keeps_absolute_includes_but_delegates_relative_ones() -> Result<()> {
    for relative in [false, true] {
        let f = Fixture::new()?;
        fs::write(f.source.join(".gitattributes"), "* text\n")?;
        git(&f.source, &["add", "."])?;
        git(&f.source, &["commit", "-m", "worktree include policy"])?;
        git(&f.source, &["config", "extensions.worktreeConfig", "true"])?;
        git(&f.source, &["config", "core.eol", "lf"])?;
        let included = f.source.join(".git/source-only.inc");
        fs::write(&included, "[core]\n eol = crlf\n")?;
        let include_path = if relative {
            "source-only.inc"
        } else {
            included.to_str().unwrap()
        };
        git(
            &f.source,
            &["config", "--worktree", "include.path", include_path],
        )?;
        assert_eq!(
            git(&f.source, &["config", "--get", "core.eol"])?.stdout,
            b"crlf\n"
        );
        // Source is a canonical CRLF checkout under its own effective config.
        fs::write(f.source.join("hello.txt"), CRLF)?;
        let output = compare_stock(&f, &["hello.txt", ".gitattributes"], !relative)?;
        if relative {
            let trace = String::from_utf8_lossy(&output.stderr);
            assert!(
                trace.contains("relative config.worktree includes require stock Git"),
                "{trace}"
            );
            assert!(!trace.contains("CoW: verified"), "{trace}");
            assert!(!f.staging_root().exists());
            assert_eq!(fs::read(f.destination().join("hello.txt"))?, LF);
        } else {
            assert_eq!(fs::read(f.destination().join("hello.txt"))?, CRLF);
        }
        assert_eq!(fs::read(f.source.join("hello.txt"))?, CRLF);
    }
    Ok(())
}

#[test]
fn expansion_and_encoding_attributes_delegate_before_native_installation() -> Result<()> {
    for attributes in [
        "hello.txt ident\n",
        "hello.txt working-tree-encoding=UTF-16LE\n",
        "hello.txt -filter\n",
    ] {
        let f = Fixture::new()?;
        fs::write(f.source.join(".gitattributes"), attributes)?;
        git(&f.source, &["add", ".gitattributes"])?;
        commit_blob(&f, "hello.txt", b"$Id$\n", "100644")?;
        fs::write(f.source.join("hello.txt"), b"$Id$\n")?;
        let output = compare_stock(&f, &["hello.txt"], false)?;
        assert!(!String::from_utf8_lossy(&output.stderr).contains("CoW: verified"));
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn hybrid_preserves_odd_paths_modes_and_symlink_targets() -> Result<()> {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for format in ["sha1", "sha256"] {
        let f = Fixture::with_object_format(format)?;
        fs::write(f.source.join(".gitattributes"), "* text=auto eol=lf\n")?;
        let odd = " leading\tname\nfile";
        fs::write(f.source.join(odd), LF)?;
        fs::set_permissions(f.source.join(odd), fs::Permissions::from_mode(0o755))?;
        symlink("missing\nlink", f.source.join("link"))?;
        git(&f.source, &["add", "."])?;
        git(&f.source, &["commit", "-m", "odd paths"])?;
        fs::write(f.source.join(odd), CRLF)?;
        fs::rename(f.source.join("link"), f.root.join("saved-link"))?;
        symlink("different", f.source.join("link"))?;
        compare_stock(&f, &[odd], true)?;
        assert_eq!(
            fs::read_link(f.destination().join("link"))?,
            Path::new("missing\nlink")
        );
        assert_eq!(
            fs::read_link(f.source.join("link"))?,
            Path::new("different")
        );
        assert_eq!(
            fs::metadata(f.destination().join(odd))?
                .permissions()
                .mode()
                & 0o777,
            fs::metadata(f.root.join("stock").join(odd))?
                .permissions()
                .mode()
                & 0o777
        );
    }
    Ok(())
}

#[test]
fn failing_filters_run_once_through_stock_git_and_retain_failure() -> Result<()> {
    let f = Fixture::new()?;
    let marker = f.root.join("filter-calls");
    fs::write(
        f.source.join(".gitattributes"),
        "hello.txt filter=unspecified\n",
    )?;
    git(&f.source, &["add", ".gitattributes"])?;
    git(&f.source, &["commit", "-m", "required filter"])?;
    git(
        &f.source,
        &["config", "filter.unspecified.required", "true"],
    )?;
    git(
        &f.source,
        &[
            "config",
            "filter.unspecified.smudge",
            &format!("echo call >> '{}'; exit 1", marker.display()),
        ],
    )?;
    let output = f.add()?;
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("CoW: verified"));
    assert_eq!(fs::read_to_string(marker)?, "call\n");
    assert_eq!(fs::read(f.source.join("hello.txt"))?, LF);
    assert!(!f.stages()?.is_empty());
    Ok(())
}

#[test]
fn conditional_includes_and_redirected_attribute_sources_delegate() -> Result<()> {
    let f = Fixture::new()?;
    let include = f.root.join("conditional-config");
    fs::write(&include, "[core]\n eol = crlf\n")?;
    git(
        &f.source,
        &[
            "config",
            "includeIf.onbranch:feature.path",
            include.to_str().unwrap(),
        ],
    )?;
    fs::write(f.source.join(".gitattributes"), "* text\n")?;
    git(&f.source, &["add", "."])?;
    git(&f.source, &["commit", "-m", "branch-sensitive checkout"])?;
    let output = checked(f.add()?)?;
    assert!(!String::from_utf8_lossy(&output.stderr).contains("CoW: verified"));
    assert_eq!(fs::read(f.destination().join("hello.txt"))?, CRLF);
    Ok(())
}
