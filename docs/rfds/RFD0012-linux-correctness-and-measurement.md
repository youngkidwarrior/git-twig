# RFD0012 — Linux correctness and measurement

- Status: implementation snapshot
- Author: leostera
- Date: 2026-11-15

## Decision and motivation

Extend [Linux reflinks](RFD0011-linux-reflinks.md) with explicit Git compatibility
boundaries and native filesystem tests. Measure actual CLI time and filesystem
allocation using the existing real-repository harness, rather than treating a
successful ext4 fallback as proof of Linux CoW correctness or performance.

## Compatibility contract

Preserve destination-inherited Linux directory setgid bits before creating children;
otherwise nested files can receive the wrong group in shared workspaces. Do not
inherit source permission restrictions or special modes. The tracked Linux walk
establishes file types, inventory and checkout modes in one pass; do not run the
APFS subtree-pruning pass over its completed output.

Delegate `core.symlinks=false` and unsupported conversions, including legacy `crlf`.
Use the same [attribute and byte verification](RFD0001-verified-cow-worktrees.md#reference-and-invariants)
as on macOS. The reference and regression checks compare raw bytes and entry types.

Reference-transaction hooks run inside branch registration; post-index-change hooks
can run while initializing the index. Both are inside the native registration lock.
Conservatively delegate when either effective hook path exists or a configured
hook event names either event, including Git 2.56 config-defined hooks. Do not
disable user hooks to retain the optimization. Post-checkout still runs after
unlocking. Hook configuration changes racing preparation remain outside the
same-user mutation boundary, like source-path races.

Tests cover hook recursion with an isolated process group and bounded watchdog,
relative/absolute hook paths, config-defined hooks where supported, raw CRLF,
symlink config, setgid groups, SHA-256 repositories, non-UTF8 paths, sparse/large/
empty files, bidirectional writes/truncation, FIFO replacement, restored mtimes,
cross-filesystem fallback and native concurrent registration. Linux CI uses both
XFS and Btrfs; native-required runs cannot silently delegate.

## Measurement contract

Schema 3 adds Linux XFS and Btrfs images to twig-real-bench and identifies the
suite as real-image-cohort. Existing schema 1/2 APFS data remains readable by the
plotter. The synthetic harness also requires native Linux CoW, rather than timing
delegation as a native result.

Create a fresh, exclusively created regular image file for every backend/repeat;
never format a device or reuse an existing image. Format XFS with reflink=1 or
Btrfs with default features. Mount using noninteractive sudo in an isolated test
environment. Run measurement as an unprivileged user. Retain Linux images and
fixtures after unmount. No host filesystem reformatting, cleanup or deployment.

Pin input commits; seed using shared object storage outside the image and an
explicit checkout inside it. Warm each backend once. Retain warmup and stages
before the baseline snapshot, so they are excluded from measured growth.
Alternate backend order across at least three repeats for publishable comparisons.

Start a bounded worker pool through a barrier. Record all outcomes without retries.
End batch creation at the last CLI completion, before final coordinator checkpoint
and join overhead. Record harness CPU separately from CLI child CPU to help detect
generator saturation. Measure first status after creates; independently verify
every expected byte, link target and executable bit. A one-byte write in each
completed worktree must not modify the seed.

Allocation is statvfs total-minus-free growth on an otherwise idle isolated
filesystem, with sync, repeated observations, stability flags and signed deltas.
It includes data counted once, filesystem metadata, indexes and retained stages.
It is not summed st_blocks, du, underlying image allocation, or exact per-file
exclusive accounting. Btrfs statfs is an allocator estimate, not device-level
physical accounting. Loopback files on ext4 and shared-host warm caches limit
generalization to bare-metal storage. Record CPU, kernel, tool versions, candidate
SHA-256, source/harness revisions and dirty state.

Save raw JSON after each cohort and scratch completion outcomes during creation.
Generate SVG charts from that JSON and print tables to stdout; do not create
verification/status markdown. Failures, unstable samples and budget stops remain
visible. Report absolute milliseconds/seconds and allocated MiB; do not turn a
storage win into an unsupported latency claim.

## Alternatives and limits

Keeping only stock Git is simpler but provides no native CoW. Weakening tests or
accepting silent fallback would hide the feature's actual behavior. A new Linux
benchmark harness would duplicate existing worker, verification and provenance
logic. A dedicated raw device would isolate the storage stack better but introduces
unnecessary destructive setup. Privileged container setup is a local test facility,
not a deployed service. Native host-filesystem throughput, cold-cache performance,
and Linux binary release packaging remain separate work.
