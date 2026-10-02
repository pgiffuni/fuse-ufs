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
dirty buffers                        (done: rufs/src/buf.rs, wired in for
    |                                  cylinder groups, inodes and indirect
    v                                  blocks -- rufs/src/ufs/meta.rs)
dependency engine                    (partial: rufs/src/softdep.rs, no
    |                                  dependencies raised yet)
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

**Status: implemented and tested (10 engine tests), and now in the write path.**
See below for what is wired to it and what is not.

## 3.3 What is wired to the engine

`Ufs` owns a `buf::BufferCache` and a `softdep::DependencyEngine`
(`rufs/src/ufs/meta.rs`), and `Ufs::sync_metadata()` is the only thing that
persists metadata.  What reaches it today:

| operation | behaviour |
|---|---|
| cylinder-group structs | staged, dirty until flushed |
| `cg_blksfree[]`, `cg_iused[]` | staged, and the *same buffer* as the struct |
| inodes | staged; a staged inode is visible to the running filesystem immediately |
| indirect blocks | staged; a newly allocated one is zeroed through the cache |
| directory blocks | staged; `inode_read_block`/`inode_write_block` route them here for `InodeType::Directory`, and *only* for that |
| a new direct pointer | gated on `Gate::AllocationSafe` of the block it names |
| a new indirect-block entry | gated on `Gate::AllocationSafe` of the block it names |
| the inode pointer to a new indirect block | gated, as `DepKind::IndirectPointer` |
| a new inode | `InodeDep`: its image and its bitmap bit, in two different buffers |
| a new directory entry | its four-byte `di_ino` gated on `Gate::InodeWritten` |

`sync_metadata()` drains: it publishes, writes, discharges whatever allocation
events each completed write made true, and repeats until a pass writes nothing.
One pass is not enough -- the pass that writes the cylinder group discharges the
allocation, which opens the inode's pointer range, and only the next pass writes
it.

The distinction the whole design rests on is between *changing* a block and *the
block having reached the device*.  Only `BufferCache`'s write-back report may
advance a dependency, and the two events are deliberately asymmetric: a
cylinder group's bitmap is never gated, so any write persists it, while a block's
*contents* are the live image, so only a `Written::Full` write persisted them.

### What a byte-range gate cannot express

`Buffer` holds a gated range back by **zeroing** it in the safe image.  That is
right for a pointer -- zero *is* "no pointer here" -- and wrong for anything
else, because zero is a different and wrong value rather than a stale one.

That makes the byte-range model unusable for two of the dependencies below:

* `FreeBlocksDep` -- holding `cs_nbfree` back zeroed reports a full filesystem
  as empty.
* `InodeReclaim` -- holding `cs_nifree` back has the same problem.

Both were attempted with gates, and the crash-point suite is what proved them
unsound.  The first attempt gated the whole `fs_bsize` block, and the suite came
back with `check()` rejecting an image whose CG0 magic was zero.  Narrowing it to
`cg_cs` plus the one bitmap byte was no better: "cg_cs.cs_nifree is 0 but the
inode bitmap has 248 free inodes".  Removing both gates is what made three
crash points pass.

Both need FreeBSD's *deferral*: record the free as pending against the container
and perform it in a later pass, once the container has been written.  That is a
work list rather than a gate, and it is the shape both of them have to take.
Until then both are done immediately, which keeps the image self-consistent and
leaves the crash window open.  `Ufs::blk_free()` takes the container of the
removed pointer and every caller threads it, so the work list has the argument
it needs.

The gates that *are* wired -- `NewBlockDep`, `DirectPointer`,
`IndirectPointer`, `DirectoryAdd`, `DirectoryRemove` -- all gate pointers or a
directory entry's inode number, which is exactly the case zeroing is right for.

### Still direct

| path | why |
|---|---|
| ordinary file data | the kernel page cache is buffering it; a second layer would make every writeback an ordering question |
| the superblock | `update_sb()` writes it directly, and nothing gates it |
| extended attributes | stored outside the block map |
| directory *removal* | `dir_unlink()` writes the shrunken directory immediately; `Gate::InodeLinkCounted` has no operation raising it |

### Link counts are not byte-ranges either

`i_nlink` is a counter, so `InodeLinkCounted` and `MkdirParentDep` hit the same
wall as `FreeBlocksDep`: a gate holds a range back by zeroing it, and a zeroed
`nlink` is wrong rather than stale.

The crash window is real and open.  `mkdir()` increments the parent's `nlink`
and then adds the entry; a crash between them leaves the parent counting a
directory its tree does not contain.  `unlink()` has the mirror image.

What makes the window survivable is that it is repairable, and the
crash-point suite now says so precisely: `Report` distinguishes a
*contradiction* from an *incompleteness*, and a link counter that ran ahead of
the tree is an incompleteness -- `fsck` pass 4 sets `nlink` from the tree, and
the extra count names an entry that is not there, so nothing can dangle.

The other direction is **not** an incompleteness and stays a contradiction:
two entries naming an inode whose `nlink` admits one is a reference that will
dangle when either is removed.  `fsck` cannot tell which entry is the stale
one, so it must not be handed a state where the answer is a guess.

`MkdirParentDep` closes it, and *not* by gating the count.  A gate holds a
range back by zeroing it, so `i_nlink` cannot be gated; the whole inode buffer
is held back instead, which is sound for any content because the device then
keeps what was already there.  `BufferCache::block()` does that, and
`Gate::DirectoryPersisted { parent, blk }` opens on a write of the named
directory block.

`FreeBlocksDep` and `InodeReclaim` cannot use the same trick: their *own* buffer
is the cylinder group, and holding that back would freeze every other
allocation in the group.  They need the deferral proper -- perform the free in a
later pass once the container is persistent -- which is still to do.

### The deferred work queue exists; nothing is wired to it yet

`DeferredOp` and `DeferredQueue` (in `rufs/src/softdep.rs`) are the mechanism
`FreeBlocksDep` and `InodeReclaim` need, with tests and no callers.  The state
machine stores one state (`Pending`); readiness is derived from the container's
persisted state rather than stored, so it cannot go stale between a dependency
resolving and the next drain; and `Applied` is represented by absence from the
queue, so a second drain has nothing to find.

Two things the design settled that are worth keeping:

* Identity is what the operation *frees*, never when it was queued.  A block free
  is keyed by the block; an inode free by `(inode, generation)`, because inode
  numbers are reused and an old reclaim must not be satisfied by a later inode
  that happens to have the same number.
* Duplicate detection cannot use the allocation bitmap, and that is the whole
  reason it is needed: a *pending* free leaves the block reading as allocated --
  deliberately, because a crash in the window has a pointer still reaching it --
  so "freed once" and "freed twice" are indistinguishable to the bitmap.

**The wiring is not done**, after two attempts.  The useful thing both attempts
established is *where* it goes wrong.

`blk_free` decides between freeing now and queueing by asking
`DependencyEngine::container_is_persisted(container)`, and that flag is not a
first-class part of the cache's contract -- it is a side channel maintained by
two call sites in `rufs/src/ufs/meta.rs`:

  * `metadata_block()` sets it to `!buffer.is_dirty()`, on the theory that a
    clean buffer was fully written and so matches the disk;
  * `metadata_block_mut()` clears it, on the theory that a mutable borrow is the
    start of a change.

Tracing `inode_truncate` shows neither assumption holds in the case that
matters.  `inode_shrink` frees twenty data blocks and one indirect block; the
containers are the inode block and the first-level indirect block.  Both
`blk_free` calls took the **immediate** path -- `container_is_persisted` returned
true -- so nothing was queued, the drain had nothing to do, and the blocks were
released while the inode still pointed at them.  That is precisely the failure
the deferral exists to prevent, and it happened because the flag said so.

So the flag, not the queue, is what needs fixing.  Two things are wrong with it:

* "clean implies persisted" is only true if a buffer is never written *safely*.
  It is -- a safe write leaves the buffer dirty -- but the converse is not
  tracked: a buffer can be dirty, fully written, and still have gated ranges
  outstanding, and that is a state the flag cannot currently express.
* It is derived from `is_dirty()` at *access* time, which means the answer
  depends on whether something happened to touch the buffer since the last
  write.  A deferred free needs a statement about the filesystem, not about the
  last access.

The right shape is for `BufferCache` to say whether a block's *live image* is on
the device -- a property of the cache, not a side effect of who last asked --
and for `note_block_written` to keep it.  That is close to the existing
`Written::{Safe,Full}` report, but it has to survive the difference between "written
safely and still gated" and "written in full", which `needs_write()` collapses.

Do not build the deferral on the current flag.  Fix the flag first, with a test
that a buffer written safely and then gated is *not* reported as persisted.

### Two asymmetries worth remembering

**A block's contents need a full write; an inode's image does not.**  A block's
contents *are* the live image, so only `Written::Full` persisted them.  An inode
written safely with a pointer still gated is genuinely on the disk -- valid
mode, size and link count, bitmap bit set -- which is all `InodeWritten` means.
Requiring `Full` there deadlocks: a directory's inode always has a gated
pointer, so its block is only ever written safely.

**`InodeDep` exists because an inode has two halves, like a block.**  Its image
lives in an inode block and its bitmap bit in a cylinder-group block, so the
events are independent and either order is legal, exactly as for
`NewBlockDep`.  `Gate::InodeWritten` opens on their conjunction.

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

Phases 1–7 are done, and so is phase 8's first half: the `BufferCache` is wired
into the cylinder-group, inode and indirect-block write paths, and
`Ufs::sync_metadata()` is the one thing that persists metadata.  Phase 8 is
*not* finished, because directory blocks are still written directly and because
nothing raises a dependency yet -- so `sync_metadata()` currently writes
everything, ungated.  That is the same behaviour as before, minus the immediacy.

What is left, in order, each independently verifiable:

1. ~~Raise `NewBlockDep` from `blk_alloc_for` and the direct-pointer gate from
   `inode_set_block`; write through the engine.~~  **Done.**
2. ~~Directory blocks through the cache, then `DirectoryAdd`.~~  **Done.**
   Directory blocks are routed in `inode_read_block`/`inode_write_block` rather
   than in `dir.rs`, because every directory path reaches them there; deciding
   per-caller would give one block two writers, which is the bug the
   cylinder-group bitmaps already had.
3. ~~`DirectoryAdd` in `dir_newlink`.~~  **Done.**  `InodeUpdateDep` is not:
   it covers the inode's own link-count and size transitions, which no
   operation raises yet.
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