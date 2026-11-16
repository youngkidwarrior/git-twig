# RFD0001 — Verified CoW worktrees

- Status: implementation snapshot
- Author: leostera
- Date: 2026-11-15

## Summary and motivation

Agent fan-out repeatedly materializes the same working files. Keep real Git linked
worktrees and shared history, replacing only initial file materialization with
native CoW clones. Start with a narrow same-HEAD fast path, not a daemon or VFS.

## Guide

`git twig add -b feat/parser ../feat-parser HEAD` creates an ordinary linked
worktree. Other worktree operations delegate to Git. TWIG_TRACE=1 reports whether
CoW was used. On unsupported platforms Git remains the implementation.

## Reference and invariants

Parse the supported argument subset before effects. Resolve source HEAD and the
target commit; optimize only when equal. Enumerate target tracked entries, reject
submodules/sparse layouts and unsupported conversion attributes, and clone into a
private staging directory on the destination filesystem. Never copy .git,
untracked files, ignored outputs or a source index. Never hardlink to source.
Check ancestors without following symlinks.

Check attributes in both the source and the pinned commit. Delegate conditional
includes, worktree-specific configuration, and checkout-relative config or
attribute paths to Git. Compare staged attribute files and, when `text=auto eol=lf`
applies, all staged regular files with their committed blob IDs without filters.
Git preserves these bytes at checkout, including historical CRLF and binary blobs.

Create a fresh temporary index from the pinned commit, then ask Git to hash-compare
the staged files against it with stat caches and fsmonitor disabled. A dirty or
racing source may fail verification, but must never silently become a clean target.
Verification failure delegates to Git before registration. This verifies completed
clones; it does not claim the source was atomically snapshotted.

Git registers the final path with --no-checkout and owns branch checks and
configuration. Move verified top-level entries into the registered destination.
Initialize its index without rewriting files. Run the post-checkout hook with
zero/target/1 arguments; hook failure retains the destination as Git does.
Any failure after registration reports partial state and does not blindly retry
or delete it. Cancellation may leave a staged directory or registered partial
worktree, which is a documented limitation rather than transactional rollback.

Source mutation is not controlled; the finished cloned bytes are verified before
registration. New worktree mutation by external writers after registration is
outside this tool's safety boundary. Same-user processes are not sandboxed.
Git is executed from PATH without a shell. Paths must be repository-relative,
normal components; NUL-delimited tree entries preserve non-UTF8 filenames.

## Compatibility and limits

This is not a transparent replacement of every Git checkout backend. Delegate
unrecognized add options, different target commits, sparse repositories, submodules,
custom filters/encoding/ident, and unsupported filesystems to stock Git.
A git-twig executable is discovered by Git; it does not override a built-in.
Git 2.40+ is the initial supported baseline. No dirty-file inheritance.
The staging strategy consumes metadata and hashes every file; it may lose on
small files. Source/destination must share a clone-capable filesystem.
Retain scratch fixtures initially, with paths reported by tools.
Do not infer exclusive CoW disk usage from st_blocks.

## Validation and rollout

End-to-end tests drive real Git and the actual CLI: clean branching, dirty-source
fallback, existing path refusal, hook behavior, source isolation, executable and
symlink entries, linked source, unusual names, passthrough and different commits.
A Rust harness runs release CLIs against stock Git, alternating order, warming
both and writing raw JSON after every sample. Record exact commits/tool versions,
fixture sizes, correctness, latency, and first status cost. Run file-size/count
profiles and concurrency sweeps before broad performance claims.

Initial numeric gates: zero incorrect checkouts/source changes; no unexplained
errors; investigate a 20% creation-latency regression rather than hiding it.
Aspirational goal: lower creation+first-status latency for multi-GiB repositories;
no speedup promised without data.

## Drawbacks and alternatives

Verification and Git process startup may dominate. Native clones still enumerate
paths and allocate metadata. Strict staging also requires temporary storage and
crash recovery is manual. Stock Git is the simplest correct alternative.
Rift clones independent repositories and preserves workspace state; those are
different semantics. git-worktree-apfs is closer prior art; compare rather than
assuming improvement.

Sources:
- https://github.com/anomalyco/rift/blob/7077f47ae1f3800bed2c689142d0b32ea568c5fa/crates/core/src/strategy/apfs.rs
- https://github.com/anomalyco/rift/blob/7077f47ae1f3800bed2c689142d0b32ea568c5fa/crates/core/src/git.rs
- https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/clonefile.2
- https://github.com/git/git/blob/v2.56.0/builtin/worktree.c
- https://github.com/dannolan/git-worktree-apfs

## Open questions and non-goals

Full option compatibility, automatic interrupted-operation recovery, Linux reflinks,
inheriting build artifacts, immutable seed caches and faster verification are future
work. Benchmark correctness before optimizing away verification. No security sandbox,
publication broker or new source-control model.
