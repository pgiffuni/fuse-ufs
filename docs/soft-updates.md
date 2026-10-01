# Soft Updates: architecture, status and plan

This document is the honest state of the Soft Updates work: what exists, what
each piece is for, and what is still missing.  It is written to be updated as
the work proceeds, so that the gap between the design and the code is never
papered over.

FreeBSD's `sys/ufs/ffs/ffs_softdep.c` and `sys/ufs/ffs/softdep.h` were used as
an architectural reference.  No FreeBSD code was copied; the Rust
implementation is independent.

---

## 1. The target architecture

```text
FUSE operation
    |
    v
UFS operation
    |
    +--> allocation policy            (done: rufs/src/policy.rs)
    |
    +--> metadata / inode / directory modification
    |
    v
dirty buffers                        (done: rufs/src/buf.rs, not yet in the
    |                                  write path)
    v
dependency engine                    (partial: rufs/src/softdep.rs)
    |
    v
safe write scheduling
    |
    v
block device
```

Each arrow is a place where information is *lost* in the current code.  The
whole point of the refactor is that none of them may be lost: an operation must
be able to say "this pointer is not safe yet" and have that survive until the
dependency engine has written something else first.

## 2. Why "modify then immediately write" has to go

`create()` on a UFS2 filesystem performs, in this order:

1. allocate a block (clear its bitmap bit, decrement `cs_nbfree`),
2. write the file's first bytes into it,
3. store its address in the inode's `direct[0]`,
4. write the inode,
5. add the name to the parent directory,
6. write the parent directory,
7. write the superblock, because `cs_nbfree` changed.

With immediate writes, a crash between (3) and (4)... cannot happen, because (3)
and (4) are one operation.  But a crash between (1) and (2) leaves an allocated
block containing the previous tenant's data, and a crash between (4) and (6)
leaves an inode that no directory entry can reach.

The first is benign.  The second is the interesting one, and it is what
`fs_journal` / `softupdates` exist for: with immediate writes the *only* fix is
to reorder the operations, and reordering "allocate then write then point" into
"write then point then allocate" requires holding the block's contents somewhere
other than the disk, because the block is not addressable until it is allocated.

Hence the buffer cache: the block's contents must be writable *before* the block
is addressable, which means dirty buffers rather than device writes.

## 3. What exists

### 3.1 Buffer cache — `rufs/src/buf.rs` (Phase 7)

| type | role |
|---|---|
| `BlockDevice` | the raw `read_at`/`write_at` seam |
| `Buffer` | one filesystem block: live contents, unsafe ranges, dirtiness |
| `BufferCache` | residency, dirty ordering, the only writer to the device |
| `Written` | `Clean` / `Safe` / `Full`, so a caller can tell a partial write |

The key design point is that `Buffer` carries **two** states: the *live* image,
which is what the running filesystem reads, and a set of byte ranges that are not
yet safe to write.  `safe_image()` materialises what may reach the device by
zeroing those ranges, which is the on-disk representation of "no pointer here".

```text
live indirect block:    [A B C D]
C is not yet safe:      [A B 0 D]      <- what gets written
C becomes safe:         [A B C D]      <- now the buffer is publishable
```

A dirty buffer is therefore *not* a writable buffer, and the two events are
separate calls.  `write_back_all()` writes in oldest-dirtied order, which is what
lets the engine express "bitmap before contents" without reordering anything.

**Status: implemented and tested (13 tests), not in the write path.**

### 3.2 Dependency engine — `rufs/src/softdep.rs` (Phases 8–11, partial)

| type | role |
|---|---|
| `AllocationState` | `New` → `BitmapWritten` → `ContentsWritten` → `SafeToPublish` → `Published` → `Complete` |
| `NewBlockDep` | one newly allocated block: bitmap progress, contents progress, publication progress |
| `Dependency` | a *gate*: a byte range of a buffer, plus what it waits for |
| `Gate` | what a gate is waiting for |
| `DependencyEngine` | the gates, the allocation progress, and the reverse index that makes resolution cheap |

Every `DepKind` answers the five questions the design asks of a dependency, as
methods rather than as prose, so the answers are testable and cannot drift:

```rust
kind.created_by();    // which operation creates it
kind.enforces();      // the disk ordering rule
kind.prevents();      // the crash inconsistency without it
kind.resolved_by();   // what resolves it
kind.protects();      // which invariant in docs/ufs2-invariants.md
```

Implemented:

* **Phase 9 — `NewBlockDep`.**  The bitmap transition and the contents
  transition are independent booleans, so either order is accepted, and
  publication requires both.  `softdep::t::crash_at_each_intermediate_point`
  walks the truth table and asserts, for each of the four states, whether a
  persistent pointer may exist — and asserts the implication "a persisted
  pointer implies an allocated block" for every row.
* **Phase 10 — direct pointer gates.**  A `Gate::AllocationSafe` on the inode's
  `direct[i]` byte range.  While it is closed, a flush writes zeros for the
  pointer and the rest of the inode; when the allocation completes, the real
  value goes out.
* **Phase 11 — indirect pointer gates.**  Per-entry gates, so a live indirect
  block can be written with the safe entries present and the unsafe ones zeroed.
  `softdep::t::indirect_block_safe_image` is the `[A B 0 D]` case, with A and D
  safe and B and C not.
* `Gate::InodeWritten`, `Gate::InodeLinkCounted`, `Gate::DirectoryPersisted` and
  `Gate::InodeReclaimed` exist and are honoured by `signal()`, but no operation
  raises them yet.

**Status: implemented and tested (10 tests).  The engine is not yet in the write
path**: the allocator and the inode and directory code still write through
`BlockReader`.

## 4. What is not implemented

These are the remaining phases.  For each: what has to be created, the ordering
rule it enforces, the crash it prevents, what resolves it, and the invariant it
protects.

### Phase 12 — `InodeUpdate`

* **Created by** `inode_write`/`inode_truncate`/`inode_bump`.
* **Enforces** that an inode's new link count, new size and new block pointers
  become persistent as one unit, and that the block frees it implies are not
  announced before the inode no longer references them.
* **Prevents** an inode whose `i_nlink` reached zero on disk while its
  directory entry is still present: `fsck` pass 4 sees `nlink == 0` with a
  reference and clears the reference, silently discarding the file's last name.
* **Resolved by** the inode's buffer reaching the device.
* **Protects** invariants 6 and 7.

### Phase 13 — `DirectoryAdd`

* **Created by** `dir_newlink()` after `inode_alloc()`.
* **Enforces** `allocate inode → initialise inode → safely persist inode →
  directory entry`.
* **Prevents** a directory entry naming an inode with a clear bitmap bit and
  zero contents.  `fsck` pass 2 clears the entry and the file's data is lost
  even though it is on the disk.
* **Resolved by** `Gate::InodeWritten`.
* **Protects** invariants 5 and 6.

The `Gate::DirectoryAdd` variant and its documentation exist; the operation does
not raise it yet.

### Phase 14 — `MkdirBody` and `MkdirParent`

`mkdir` is two operations wearing a trench coat:

* `MkdirBody` — the child inode's `.` and `..` block.
* `MkdirParent` — the parent's link count and its new entry.

* **Created by** `mkdir()`.
* **Enforces** the child inode *and* its `.`/`..` block *and* the parent's link
  count before the parent's entry becomes persistent.
* **Prevents** a parent directory whose `i_nlink` was incremented before the
  child's inode existed (`fsck` pass 4 decrements it and reports a lost
  directory), and a child directory that is reachable by name but whose `.`/`..`
  block was never written, which `readdir` on the child then cannot walk.
* **Resolved by** the child inode's write and the parent inode's write.
* **Protects** invariants 5, 6 and 10.

### Phase 15 — `DirectoryRemove`

* **Created by** `dir_try_unlink()` and `rmdir()`.
* **Enforces** the entry is gone from a *persistent* directory block before the
  target's link count is decremented.
* **Prevents** a crash that leaves the entry present and the link count already
  decremented.  The reverse order is the classic one: `fsck` pass 4 sees
  `nlink` one too low and reports a lost file; the file's data is still on the
  disk but nothing can reach it.
* **Resolved by** `Gate::DirectoryPersisted { parent, blk }`.
* **Protects** invariant 6.

### Phase 16 — `FreeBlocks` / `FreeWork`

* **Created by** `inode_truncate()` shrinking a file and by `inode_free()`.
* **Enforces**

  ```text
  remove the inode pointer
       |
       v
  release the child blocks
       |
       v
  release the indirect block
  ```

  An indirect block must not be freed while any child block still needs it as
  its persistent path.
* **Prevents** a hole in the block map that `fsck` pass 5 rebuilds differently
  from the running filesystem, and — worse — an indirect block freed while its
  children are still allocated, which leaves allocated blocks that no inode
  owns.  Pass 5 clears them; the data is lost with no record.
* **Resolved by** the parent inode's pointer removal reaching the device.
* **Protects** invariants 2, 3 and 4.

### Phase 17 — `FreeInode`

* **Created by** `inode_free()` when `i_nlink` reaches zero.
* **Enforces**

  ```text
  nlink == 0
      |
      v
  directory references removed
      |
      v
  inode contents cleared
      |
      v
  blocks released
      |
      v
  inode bitmap entry freed
  ```

  Never the bitmap entry first.
* **Prevents** the opposite of the `NewBlockDep` hazard: a bitmap that says
  "free" for an inode whose contents are still on disk lets the next allocation
  hand the inode number to somebody else, overwriting the old inode's blocks
  while they may still be referenced by a directory that has not been updated.
* **Resolved by** `Gate::InodeReclaimed`.
* **Protects** invariants 1 and 5.

## 5. Operation-by-operation mapping

For each operation: buffers modified, bitmap changes, inode changes, directory
changes, dependencies raised, legal write order, crash states, and the invariant
protected.  This is the table `docs/soft-updates.md` is required to carry; the
"dependencies" column says "planned" for everything not yet implemented.

| operation | buffers | bitmap | inode | directory | dependencies | legal order | crash states | invariant |
|---|---|---|---|---|---|---|---|---|
| `create` | new data block, inode block, parent dir block | block alloc, inode alloc, `cs_nbfree`, `cs_nifree`, `cs_ndir` if dir | new inode, parent `nlink` if dir | new entry | `NewBlockDep`, `DirectPointerDep`, `InodeUpdateDep`, `DirectoryAddDep` (planned) | bitmap+contents → inode → entry → superblock totals | inode without entry; entry without inode | 2, 3, 5, 6 |
| `mkdir` | child `.`/`..` block, child inode block, parent inode block, parent dir block | block alloc, inode alloc, `cs_ndir` | child inode, parent `nlink` | new entry | `MkdirBodyDep`, `MkdirParentDep`, `DirectoryAddDep` (planned) | child inode + body → parent link count → parent entry | parent link count up with no child; child reachable with no `.`/`..` | 5, 6, 10 |
| `link` | inode block, both dir blocks | none | `nlink++` | new entry | `InodeUpdateDep` (planned) | `nlink` → both entries | two entries, `nlink` too low | 6 |
| `unlink` | inode block, dir block | block frees, `cs_nbfree` | `nlink--`, cleared if zero | entry removed | `DirectoryRemoveDep`, `FreeBlocksDep`, `FreeInodeDep` (planned) | entry removed → `nlink--` → blocks → bitmap | entry present with `nlink` already down | 3, 6 |
| `rmdir` | child block, child inode block, parent inode block, parent dir block | block frees, inode free, `cs_ndir` | child cleared, parent `nlink--` | entry removed | `DirectoryRemoveDep`, `FreeBlocksDep`, `FreeInodeDep` (planned) | entry removed → parent `nlink--` → blocks → bitmap | parent `nlink` down with child still linked | 3, 6, 10 |
| `rename` | both dir blocks, moved inode block | none | moved inode's parent | entry moved, source parent `nlink--` | `DirectoryRemoveDep`, `DirectoryAddDep` (planned) | source entry removed → inode → target entry | source and target both have the name | 6 |
| `write` (extend) | new data blocks, indirect blocks, inode block | block alloc, `cs_nbfree` | `size`, `blocks`, indirect pointers | none | `NewBlockDep`, `DirectPointerDep`, `IndirectPointerDep`, `InodeUpdateDep` | bitmap+contents → indirect entry → inode | inode pointing past the block map | 2, 3, 7 |
| `truncate` | freed blocks, inode block | block frees, `cs_nbfree` | `size`, `blocks`, cleared pointers | none | `FreeBlocksDep`, `InodeUpdateDep` (planned) | inode pointer removed → child blocks → indirect → `cs_nbfree` | allocated block no inode owns | 2, 3, 7 |
| `file extension` | new data blocks, indirect blocks, inode block | block alloc | `size`, `blocks` | none | as `write` | as `write` | as `write` | 2, 3, 7 |

## 6. Crash-state validation (Phase 19)

`Ufs::check_consistency()` (`rufs/src/ufs/fsck.rs`) is the oracle for every
mutating test, and it is already used for all fifteen allocator tests.  Turning
it into a crash-injection harness needs:

1. A way to drop the write of the *n*-th buffer, which [`BufferCache`] makes
   easy: wrap the `BlockDevice` in a device that fails on demand.
2. A "commit as far as you can, then stop" entry point on the engine, which is
   `write_ready()` returning what it wrote — the test then replays the image
   from the beginning of `write_ready()` up to a chosen write and re-opens it.
3. The checker, run on the result.

The harness is deliberately simple: it does not model power-loss ordering beyond
"this write did not happen", because the dependency rules are supposed to make
the answer independent of *which* writes were lost, only of how many and which.

`fsck_ffs` itself is not available on the platforms this driver is built and
tested on (no `fsck_ufs` in the container, no `pkg-config`, no libfuse3), which
is why the checker is an in-crate re-derivation of the five passes rather than a
wrapper.  See [`freebsd-ufs2.md`](freebsd-ufs2.md) for the pass-by-pass mapping
and for what the checker deliberately does not implement.

## 7. Order of work

Phases 1–7 are done.  The remaining phases are ordered so that each one is
independently verifiable:

1. Wire `BufferCache` into the inode, directory and cylinder-group write paths,
   keeping `write_back()` in the driver's hands so behaviour is unchanged.
   Verifiable by the existing 92 tests.
2. Raise `NewBlockDep` from `blk_alloc_for` and `DirectPointerDep` from
   `inode_set_block`; write through the engine.  Verifiable by
   `crash_at_each_intermediate_point` end-to-end with the crash harness.
3. `InodeUpdateDep` and `DirectoryAddDep` in `dir_newlink`/`mknod`.
4. `DirectoryRemoveDep` in `dir_try_unlink`/`rmdir` — the first operation whose
   *removal* ordering matters.
5. `FreeBlocksDep` in `inode_shrink`/`inode_free`.
6. `FreeInodeDep` in `inode_free`.
7. `MkdirBodyDep`/`MkdirParentDep`.
8. Deep indirect levels (Phase 11's "initially support one indirect level
   correctly" is already done; the deeper levels need the same gates at each
   level).

Optimization, eviction policy and clustering come last, as the design requires:
they are only worth doing once the dependency correctness tests exist.

## See also

* [`freebsd-ufs2.md`](freebsd-ufs2.md) — the mined FreeBSD behaviour.
* [`ufs2-invariants.md`](ufs2-invariants.md) — what every dependency protects.