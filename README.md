# git twig 🌱

With twig, your agents can create cheap lightweight worktrees and use up to 97% less space on disk. Imagine all the worktrees you could now make!

## Install

```sh
brew tap leostera/git-twig https://github.com/leostera/git-twig
brew install leostera/git-twig/git-twig
```

Or install from crates.io with Rust 1.95+ and Git 2.40+:

```sh
cargo install git-twig --locked
```

For local development: `cargo install --path crates/git-twig --locked`.

## Use

With your source checkout in `main/`:

```sh
git -C main twig new -b feat/x ../feat-x HEAD
git -C main twig list
```

Native CoW requires the source checkout\'s current `HEAD`; other revisions use Git.
Short help works outside a checkout: `git twig -h` or `git twig new --help`.

For coding agents: `git twig skill` prints the bundled skill.

Native CoW supports macOS/APFS and Linux on Btrfs or reflink-enabled XFS.
Creates ordinary linked worktrees from committed files; local changes and ignored
files aren't copied. Unsupported filesystems (including typical ext4), options and
checkout semantics use `git worktree`. Native CoW can verify `text=auto eol=lf`
checkouts with stable Git configuration. Set `TWIG_TRACE=1` to see backend selection.

Source, destination and Git metadata must share a clone-capable filesystem for
native CoW. Twig verifies cloned files against the requested commit; writes are
private, never hardlinked to source. Staging lives under the common Git directory
in `twigs/staging` and is removed after success. Failed/interrupted attempts retain
staging and may leave a partial registered worktree; inspect before retrying.

## Benchmarks

<!-- twig-bench:start -->

<!-- Generated from bench/results/readme.json by twig-readme-table. Do not edit numbers. -->

| Repo | `git worktree` | `git twig` | Improvement |
|---|---|---|---|
| [Workers starter](bench/results/2026-11-15-real/workers-starter-host.json) | 1.94s / 6.0 MiB | 6.41s / 3.7 MiB | 3.30× slower / **1.6× less space** |
| [git-twig](bench/results/2026-11-15-real/host.json)† | 4.90s / 53.0 MiB | 10.13s / 7.1 MiB | 2.07× slower / **7.5× less space** |
| [Axo](bench/results/2026-11-15-real/host.json) | 4m40s / 3.0 GiB | 2m19s / 165 MiB | 2.02× faster / **18.8× less space** |
| [Linux kernel (10 sequential)](bench/results/2026-11-15-real/kernel-directory10.json) | 3m15s / 18.3 GiB | 4m32s / 549 MiB | 1.40× slower / **34.2× less space** |

Each cell is **creation time / additional disk space**. Batch sizes differ; creation excludes first status. † Includes failed Git attempts. [Setup and raw results](bench/results/readme-details.md).

<!-- twig-bench:end -->

For Linux CLI time/allocation measurements, see [Linux benchmarks](bench/linux.md).

## Develop

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Linux CI tests the host filesystem plus reflink-enabled XFS and Btrfs images,
requiring native CoW on both. Locally on a supported Linux filesystem, run
`TWIG_TEST_REQUIRE_NATIVE=1 cargo test -p git-twig`. Tests and benchmarks retain
fixtures under `target/`; disk use grows. See the
[design contract](docs/rfds/RFD0001-verified-cow-worktrees.md),
[Linux correctness boundaries](docs/rfds/RFD0012-linux-correctness-and-measurement.md),
and [release operations](docs/releases.md).

[MIT](LICENSE)
