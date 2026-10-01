// Copyright (c) 2026 Pedro Giffuni
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice, this
//    list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice,
//    this list of conditions and the following disclaimer in the documentation
//    and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
// ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
// LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
// CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
// SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
// INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
// CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
// ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
// POSSIBILITY OF SUCH DAMAGE.

//! The dependency engine: what may be written, and in which order.
//!
//! # What this is for
//!
//! Soft Updates is not a feature, it is a *scheduler*.  Given the same set of
//! pending metadata changes, many different on-disk orders are crash-consistent
//! and many are not, and the difference between them is invisible until the
//! machine loses power at the wrong moment.  The engine's only job is to turn
//! "this pointer may not be trusted yet" into a statement about a byte range of
//! a buffer, so that [`crate::buf::BufferCache`] can write the safe image
//! instead of the live one.
//!
//! # The shape of it
//!
//! A [`Dependency`] is a *gate*: a byte range of a buffer that may not reach
//! the disk until some event happens.  The engine holds
//!
//! * the gates, indexed by [`DepId`];
//! * the progress of each [`AllocationState`] machine;
//! * a reverse index from a gated range to its gate, so that completing an
//!   allocation can walk the gates that were waiting on it.
//!
//! When an allocation advances, every gate that was waiting on it is
//! re-evaluated; gates that no longer have an unmet precondition are *published*,
//! which is a call to [`crate::buf::Buffer::publish`] and nothing more.  The
//! buffer cache does the rest.
//!
//! FreeBSD solves the same problem with `struct work`, `struct allocwork`,
//! `struct indirdep` and the `DAG`/`DEPS`/`ALLDONE` bit-flag soup in
//! `sys/ufs/ffs/softdep.h`, plus a `SEBUF` "safe" buffer per pending operation.
//! Those structures are a hand-rolled type system in C.  The Rust equivalent is
//! ordinary ownership: a gate owns the byte range it protects, and progress is
//! an `AllocationState` value rather than a bit that several subsystems clear in
//! different orders.
//!
//! # What is implemented
//!
//! [`NewBlockDep`] (a block's allocation bitmap and its contents), and the two
//! pointer gates that build directly on it: [`DirectPointerDep`] and
//! [`IndirectPointerDep`].  The remaining categories the design calls for —
//! `InodeUpdate`, `DirectoryAdd`, `DirectoryRemove`, `Mkdir`, `FreeBlocks`,
//! `FreeInode` — are described in `docs/soft-updates.md` together with the
//! ordering each one enforces; they are *not* implemented here, and
//! `docs/soft-updates.md` records that explicitly.

use std::collections::{BTreeMap, BTreeSet};

type IoResult<T> = std::io::Result<T>;

use crate::{
	buf::{BufferCache, Written},
	data::InodeNum,
	geom::CgNum,
};

fn invalid_dep(id: DepId) -> std::io::Error {
	std::io::Error::other(format!("no such dependency: {:?}", id.0))
}

/// An identifier for a dependency.  Stable for the lifetime of the operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DepId(pub u64);

/// The lifecycle of a newly allocated filesystem resource.
///
/// ```text
///  New ──bitmap written──> BitmapWritten ──contents written──> ContentsWritten
///   │                                │                                │
///   └────────────────────────────────┴────────────────────────────────┤
///                                                                        v
///                                                                  SafeToPublish
///                                                                        │
///                                                       pointer published │
///                                                                        v
///                                                                   Published
///                                                                        │
///                                                                       done
///                                                                        v
///                                                                   Complete
/// ```
///
/// The states are separate rather than one "done" bit because the *order* of
/// the first two transitions does not matter, while both of them must precede
/// the third.  Modelling that as two booleans ([`NewBlockDep`] has exactly that)
/// is why out-of-order progress is safe and a single enum would not be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AllocationState {
	/// Reserved: the bitmap has not been updated yet.
	New,
	/// The cylinder-group bitmap says the block is in use.
	BitmapWritten,
	/// The block's contents are on the device.
	ContentsWritten,
	/// Both of the above hold; a pointer to this block may now be persisted.
	SafeToPublish,
	/// Some pointer to this block has reached the disk.
	Published,
	/// Nothing is waiting on this allocation any more.
	Complete,
}

impl AllocationState {
	/// Whether a pointer to the resource may become persistent in this state.
	pub fn allows_publication(self) -> bool {
		self >= AllocationState::SafeToPublish
	}

	/// Whether this allocation is finished with.
	pub fn is_complete(self) -> bool {
		self == AllocationState::Complete
	}
}

/// What a gate is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Gate {
	/// The block's allocation bitmap has reached the device.
	AllocationAllocated(DepId),
	/// The block's initialised contents have reached the device.
	AllocationInitialised(DepId),
	/// The block is fully safe: bitmap *and* contents.
	AllocationSafe(DepId),
	/// The named inode has reached the device.
	InodeWritten(InodeNum),
	/// The named inode has reached the device with its link count decremented.
	InodeLinkCounted(InodeNum),
	/// A directory block in the named parent has reached the device.
	DirectoryPersisted { parent: InodeNum, blk: u64 },

	/// The directory entry at that byte offset has been removed *persistently*.
	///
	/// "Persistently" is the whole word.  The entry's inode number is zeroed in
	/// the live image the moment `dir_unlink()` runs, but the directory block
	/// may not have reached the disk yet, and until it does the directory still
	/// lists a file that is in the middle of being reclaimed.  The reclaim must
	/// wait.
	///
	/// Resolved only when the block is written *fully*, because a safe
	/// write-back leaves that block's other gated ranges behind and cannot be
	/// said to have removed this entry.
	DirectoryRemoved {
		parent: InodeNum,
		blk:    u64,
		off:    u64,
	},
	/// The named inode's cleared image is on the disk.
	///
	/// The last transition of a removal:
	///
	/// ```text
	///   directory entry removed   (Gate::DirectoryRemoved)
	///        -> inode cleared     (gated on the above)
	///        -> bitmap bit freed   (this gate)
	/// ```
	///
	/// The bitmap bit is the *last* thing released because it is the one whose
	/// early release is destructive: an inode whose bit says free while its
	/// image still reads as live is one that `fsck` pass 1 reports as
	/// allocated-but-unused and pass 4 then frees a second time.
	InodeReclaimed(InodeNum),

	/// Every pointer that used to reach a now-freed block has been removed from
	/// the structures in `container`, and those structures have reached the
	/// disk.
	///
	/// The block side of the ordering.  A block's allocation bit may not be
	/// cleared while a pointer to it can still be found on the disk, because the
	/// next allocation would hand the block to somebody else with the old
	/// pointer still in place.
	///
	/// `container` is a cache block: the inode, for a direct block or for a
	/// first-level indirect block, and the enclosing indirect block for
	/// anything deeper.
	PointersRemoved { container: u64 },
}

/// A gated byte range: "these bytes of that block may not be written until the
/// gate opens".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
	/// Which dependency this is.
	pub id:       DepId,
	/// What kind of relationship it protects; see the module documentation.
	pub kind:     DepKind,
	/// The buffer whose bytes are gated.
	pub blk:      u64,
	/// Byte offset within the buffer.
	pub off:      u64,
	/// Length of the gated range.
	pub len:      u64,
	/// What must happen first.
	pub gate:     Gate,
	/// Set once the gate has opened and the range has been published.
	pub resolved: bool,
}

/// The category of a dependency, which is what the tests and the documentation
/// are indexed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepKind {
	/// A pointer from an inode's direct array to a newly allocated block.
	DirectPointer,
	/// A pointer from an indirect block to a newly allocated block, and the
	/// pointer from the inode to the indirect block itself.
	IndirectPointer,
	/// A directory entry naming a newly initialised inode.
	DirectoryAdd,
	/// An inode that is being reclaimed, held back until the directory entry
	/// that named it is gone from the disk.
	DirectoryRemove,
	/// A cylinder group's free-block accounting, held back until the structures
	/// that pointed at the freed blocks are on the disk.
	FreeBlocks,
	/// An inode's bitmap entry, held back until the inode's cleared image is on
	/// the disk.
	InodeReclaim,
}

impl DepKind {
	/// The operation that creates this kind of dependency.
	///
	/// This is the first of the five questions the design asks of every
	/// dependency; the other four are in the rustdoc of each variant.
	pub fn created_by(self) -> &'static str {
		match self {
			Self::DirectPointer => "inode_alloc_block(): a file or directory grows a new block",
			Self::IndirectPointer => {
				"inode_set_block(): the pointer table itself is created or grows"
			}
			Self::DirectoryAdd => "dir_newlink() after inode_alloc()",
			Self::DirectoryRemove => {
				"inode_free(): the last link has gone and the inode is being cleared"
			}
			Self::FreeBlocks => {
				"blk_free(): a pointer to this block has just been removed from an inode \
				 or an indirect block"
			}
			Self::InodeReclaim => {
				"free_cg_inode(): this inode's last directory entry has just been removed"
			}
		}
	}

	/// The disk ordering rule this kind enforces.
	pub fn enforces(self) -> &'static str {
		match self {
			Self::DirectPointer => {
				"the inode's pointer must not reach the disk before the block's bitmap bit \
				 and its contents"
			}
			Self::IndirectPointer => {
				"an indirect block's entry must not name a block that is not safe, and an \
				 indirect block must not be published before every entry it gained is safe"
			}
			Self::DirectoryAdd => {
				"a directory entry must not reach the disk before the inode it names is on disk"
			}
			Self::DirectoryRemove => {
				"a cleared inode must not reach the disk before the directory entry that \
				 named it is gone"
			}
			Self::FreeBlocks => {
				"a block is returned to the free list while the disk still holds a pointer \
				 to it; the next allocation hands it to somebody else and the old file \
				 keeps reading somebody else's data"
			}
			Self::InodeReclaim => {
				"an inode's bitmap bit must not reach the disk before its cleared image, \
				 and the cleared image must not reach it before the directory entry that \
				 named it is gone"
			}
		}
	}

	/// The crash inconsistency without this dependency.
	pub fn prevents(self) -> &'static str {
		match self {
			Self::DirectPointer => {
				"an inode permanently references a block the cylinder-group bitmap still \
				 reports as free; fsck_ffs pass 1 reports a block that is owned but \
				 unallocated, and pass 5 clears the pointer, losing the file"
			}
			Self::IndirectPointer => {
				"a pointer chain reaches a block that was never initialised, so a file reads \
				 back as containing garbage offsets; fsck_ffs pass 1 reports an indirect \
				 block entry that is out of range"
			}
			Self::DirectoryAdd => {
				"a directory entry names an inode whose bitmap bit is clear and whose contents \
				 are zero; fsck_ffs pass 2 clears the entry, and the file's data is lost even \
				 though it is on the disk"
			}
			Self::DirectoryRemove => {
				"a directory entry names an inode whose fields are zero and whose bitmap bit is \
				 still set; fsck_ffs pass 2 clears the entry, and pass 4 then frees the inode's \
				 blocks -- a directory entry to a file whose data is still on the disk"
			}
			Self::FreeBlocks => {
				"a block is returned to the free list while the disk still holds a \
				 pointer to it; the next allocation hands it to somebody else and the \
				 old file keeps reading somebody else's data"
			}
			Self::InodeReclaim => {
				"an inode's bitmap bit says free while its image still reads as live; \
				 fsck_ffs pass 1 reports it as allocated but unused and pass 4 then \
				 frees its blocks a second time"
			}
		}
	}

	/// What resolves this kind of dependency.
	pub fn resolved_by(self) -> &'static str {
		match self {
			Self::DirectPointer => "the NewBlockDep for the block reaching SafeToPublish",
			Self::IndirectPointer => {
				"the NewBlockDep for the target block, and then the inode's write"
			}
			Self::DirectoryAdd => "the inode's write reaching the device",
			Self::DirectoryRemove => "the directory block being written in full",
			Self::FreeBlocks => "the inode or indirect block the pointer was removed from",
			Self::InodeReclaim => "the cleared inode image, and the directory entry before it",
		}
	}

	/// Which `fsck_ffs` invariant this protects.
	pub fn protects(self) -> &'static str {
		match self {
			Self::DirectPointer => "invariants 2, 3 and 4 (docs/ufs2-invariants.md)",
			Self::IndirectPointer => "invariants 3 and 7",
			Self::DirectoryAdd => "invariants 5 and 6",
			Self::DirectoryRemove => "invariants 1 and 6",
			Self::FreeBlocks => "invariants 1, 2 and 3",
			Self::InodeReclaim => "invariants 1 and 7",
		}
	}
}

/// A newly allocated block whose bitmap and contents must both reach the disk
/// before anything may point at it.
///
/// # The crash this prevents
///
/// `create()` allocates a block, writes the file's first bytes into it, and
/// stores its address in the inode.  If the inode reaches the disk before the
/// cylinder-group bitmap does, a crash in between leaves a filesystem where the
/// inode says "lbn 0 is block 617" while `cg_blksfree[]` says block 617 is free.
/// On the next boot `fsck_ffs` pass 5 rebuilds the bitmap from the inodes it can
/// see, does *not* find block 617 owned, and frees it — the file's first block
/// is silently returned to the free list while the inode still points at it.  The
/// next allocation hands the same block to somebody else.
///
/// If the bitmap reaches the disk before the block's *contents*, a crash leaves
/// an allocated block full of whatever the previous tenant left there, which a
/// file that has not been written past its length will happily expose.
///
/// # Ordering
///
/// ```text
/// ```text
///   allocate block
///        |
///        +--> allocation bitmap --┐
///        +--> block contents -----┴--> SafeToPublish --> Published --> Complete
/// ```
///
/// The two first steps may happen in either order.  Nothing may be published
/// until both have happened.
///
/// # Resolved by
///
/// [`DependencyEngine::note_bitmap_written`] and
/// [`DependencyEngine::note_contents_written`]; the state advances on its own
/// once both have been seen.
///
/// # Protects
///
/// Invariants 2, 3 and 4 in `docs/ufs2-invariants.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBlockDep {
	id:       DepId,
	blk:      u64,
	cg:       CgNum,
	bitmap:   bool,
	content:  bool,
	publish:  bool,
	complete: bool,
}

impl NewBlockDep {
	/// This dependency's identifier.
	pub fn id(&self) -> DepId {
		self.id
	}

	/// The block this dependency is about.
	pub fn blk(&self) -> u64 {
		self.blk
	}

	/// The cylinder group whose bitmap must be updated.
	pub fn cg(&self) -> CgNum {
		self.cg
	}

	/// The current lifecycle state.
	pub fn state(&self) -> AllocationState {
		match (self.bitmap, self.content) {
			(false, false) => AllocationState::New,
			(true, false) => AllocationState::BitmapWritten,
			(false, true) => AllocationState::BitmapWritten,
			(true, true) => {
				if self.publish {
					if self.complete {
						AllocationState::Complete
					} else {
						AllocationState::Published
					}
				} else {
					AllocationState::ContentsWritten
				}
			}
		}
	}

	/// Whether a pointer to this block may become persistent.
	pub fn allows_publication(&self) -> bool {
		self.bitmap && self.content
	}

	/// Mark the dependency finished: nothing waits on it any more.
	fn finish(&mut self) {
		self.complete = true;
	}
}

/// A newly allocated inode whose bitmap bit and image must both reach the disk
/// before anything may name it.
///
/// # The crash this prevents
///
/// `mknod()` allocates an inode and then writes a directory entry naming it.
///  If the entry reaches the disk first, a crash leaves a directory that lists
///  a file whose inode bitmap bit is still clear.  `fsck_ffs` pass 2 sees the
///  entry, resolves the inode number, finds an inode that is not allocated and
///  whose fields are zero, and *clears the entry*.  The file's data was never
///  lost -- it was never reachable again.
///
/// If the bitmap reaches the disk first, a crash leaves an allocated inode that
///  nothing points at.  That is the benign direction: `fsck` pass 2 reclaims
///  it.
///
/// # Why this is not just `NewBlockDep`
///
/// Because an inode has two halves on the disk, like a block, and only their
/// conjunction is enough: the image, and the bitmap bit that says it is
/// allocated.  The bitmap lives in the cylinder group's block, which is a
/// *different* buffer from the one holding the inode, so the two events are
/// genuinely independent -- and either order is legal, exactly as for blocks.
///
/// # Ordering
///
/// ```text
///   allocate inode
///        +-- inode bitmap bit ---+
///        +-- inode image ---------+--> safe to name
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InodeDep {
	id:     DepId,
	inr:    InodeNum,
	cg:     CgNum,
	image:  bool,
	bitmap: bool,
}

impl InodeDep {
	/// This dependency's identifier.
	pub fn id(&self) -> DepId {
		self.id
	}

	/// The inode this dependency is about.
	pub fn inr(&self) -> InodeNum {
		self.inr
	}

	/// The cylinder group whose inode bitmap must be updated.
	pub fn cg(&self) -> CgNum {
		self.cg
	}

	/// Whether something may name this inode yet.
	pub fn allows_naming(&self) -> bool {
		self.image && self.bitmap
	}
}

/// A directory entry that has been removed from the live image and is waiting to
/// reach the disk.
///
/// The counterpart of [`NewBlockDep`] on the removal side: `dir_unlink()` zeroes
/// the entry's inode number immediately, so the live directory no longer lists
/// the file while the disk may still do.  Anything that reclaims the inode has
/// to wait until the disk agrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryRemoveDep {
	blk:  u64,
	off:  u64,
	gone: bool,
}

impl DirectoryRemoveDep {
	/// Whether the directory entry is known to be gone from the disk.
	pub fn is_persisted(&self) -> bool {
		self.gone
	}
}

impl DependencyEngine {
	/// Record a directory entry as removed from the live image.
	///
	/// Created by `Ufs::dir_unlink()`, once it has zeroed the entry's inode
	/// number.  Protects invariants 1 and 6 in `docs/ufs2-invariants.md`.
	pub fn directory_removed(&mut self, parent: InodeNum, blk: u64, off: u64, inr: InodeNum) {
		let gate = Gate::DirectoryRemoved { parent, blk, off };
		self.removals.insert(
			gate,
			DirectoryRemoveDep {
				blk,
				off,
				gone: false,
			},
		);
		self.by_removed_inode.insert(inr, gate);
	}

	/// Record that the directory block `blk` has been written *in full*, which
	/// is what makes every removal in it persistent.
	///
	/// Keyed by block rather than by parent because the flush loop knows which
	/// block it wrote and not which directory owns it: the parent is part of the
	/// gate, so two directories can never be confused for one another, but it is
	/// not what the caller has.  Removals outstanding between two flushes are
	/// few, so the scan is not worth an index.
	pub fn note_directory_block_written(&mut self, blk: u64) -> usize {
		let mut gone = Vec::new();
		for (gate, dep) in &mut self.removals {
			if dep.blk == blk && !dep.gone {
				dep.gone = true;
				gone.push(*gate);
			}
		}
		let n = gone.len();
		for gate in gone {
			self.settle_removal(&gate);
		}
		n
	}

	/// The gate that opens when the most recent removal of `inr` is persistent.
	///
	/// Keyed by inode rather than by block, because the reclaiming path has an
	/// inode and not a directory.  The *newest* removal is the one that matters:
	/// a file renamed within one operation has had more than one entry removed,
	/// and only the last one is still live.
	pub fn removal_gate(&self, inr: InodeNum) -> Option<Gate> {
		self.by_removed_inode
			.get(&inr)
			.and_then(|gate| self.removals.get(gate).map(|_| *gate))
	}

	/// Record whether `blk`'s cached contents currently match the disk.
	///
	/// This is *not* "has been written at some point": a buffer that was
	/// written and then modified again is back to not matching, and a pointer
	/// removed after that write is not persistent.  `Ufs` keeps it honest from
	/// the two places it can go stale -- a fetch, which brings in what the disk
	/// holds, and a mutable borrow, which is the start of a change.
	pub fn set_container_persisted(&mut self, blk: u64, persisted: bool) {
		if persisted {
			self.written.insert(blk);
		} else {
			self.written.remove(&blk);
		}
		if persisted {
			self.settle_removal(&Gate::PointersRemoved { container: blk });
			// An inode whose cleared image has just reached the disk may have its
			// bitmap bit released, if the removal it waited on also happened.
			for inr in self.reclaimants_of(blk) {
				let gate = Gate::InodeReclaimed(inr);
				if self.gate_is_open(gate) {
					self.settle_removal(&gate);
				}
			}
		}
	}

	/// Declare that `inr` is being reclaimed.
	///
	/// `removal` is the gate the directory entry's removal waits on, and
	/// `inode_blk` the cache block its cleared image lives in.  `None` for an
	/// inode that was never linked, which has nothing behind it.
	pub fn reclaim_inode(&mut self, inr: InodeNum, removal: Option<Gate>, inode_blk: u64) {
		let Some(removal) = removal else {
			return;
		};
		self.reclaiming.insert(inr, (removal, inode_blk));
	}

	fn reclaimants_of(&self, inode_blk: u64) -> Vec<InodeNum> {
		self.reclaiming
			.iter()
			.filter(|(_, (_, blk))| *blk == inode_blk)
			.map(|(inr, _)| *inr)
			.collect()
	}

	/// Whether `container`'s current contents are on the disk.
	pub fn container_is_written(&self, container: u64) -> bool {
		self.written.contains(&container)
	}

	fn settle_removal(&mut self, gate: &Gate) {
		if !self.gate_is_open(*gate) {
			return;
		}
		if let Some(ids) = self.waiting_on.remove(gate) {
			for id in ids {
				if self.deps.get(&id).is_some_and(|d| d.resolved) {
					continue;
				}
				self.publish_dep(id);
			}
		}
	}
}

/// The write-ordering engine.
#[derive(Debug, Default)]
pub struct DependencyEngine {
	next:       u64,
	new_blocks: BTreeMap<DepId, NewBlockDep>,

	/// `InodeDep`s indexed by the inode they are about.
	inodes:   BTreeMap<DepId, InodeDep>,
	by_inode: BTreeMap<InodeNum, DepId>,

	/// Directory entries removed from the live image, waiting for the disk.
	removals: BTreeMap<Gate, DirectoryRemoveDep>,

	/// Inodes being reclaimed, with the removal each waits on and the cache
	/// block its cleared image lives in.
	reclaiming: BTreeMap<InodeNum, (Gate, u64)>,

	/// The newest removal for each removed inode.
	by_removed_inode: BTreeMap<InodeNum, Gate>,

	/// Buffers that have been written since this filesystem was opened, and so
	/// whose contents are on the disk.
	///
	/// This is what makes `Gate::PointersRemoved` decidable without a separate
	/// record per removal: "the pointer has been removed persistently" reduces to
	/// "that buffer has been written", because removing a pointer is an ordinary
	/// ungated change.  It is a set rather than a flag per gate because several
	/// frees can be waiting on the same container.
	written: BTreeSet<u64>,

	/// `NewBlockDep`s indexed by the block they are about.
	///
	/// A pointer path that has just allocated a block needs to know which
	/// allocation to gate on, and it has a block number, not a [`DepId`].
	/// Threading a `DepId` out of `Ufs::blk_alloc_for()` and back down into
	/// `Ufs::inode_set_block()` would put a dependency identifier into every
	/// allocator signature for the sake of one caller; the index answers the
	/// same question without that.
	///
	/// A block that is freed and reallocated within one operation gets a second
	/// entry, overwriting the first, which is the one that matters: the old
	/// allocation is no longer the reason any pointer is being held back.
	by_block: BTreeMap<u64, DepId>,

	deps:       BTreeMap<DepId, Dependency>,
	/// Gates keyed by what they wait for, so completing an allocation does not
	/// have to scan every dependency.
	waiting_on: BTreeMap<Gate, Vec<DepId>>,
}

impl DependencyEngine {
	/// A fresh engine with no dependencies.
	pub fn new() -> Self {
		Self::default()
	}

	/// How many dependencies are outstanding.
	pub fn len(&self) -> usize {
		self.deps.len()
	}

	/// Whether nothing is outstanding.
	pub fn is_empty(&self) -> bool {
		self.deps.is_empty() &&
			self.new_blocks.is_empty() &&
			self.inodes.is_empty() &&
			self.removals.is_empty() &&
			self.reclaiming.is_empty()
	}

	/// Register a newly allocated block.
	pub fn new_block(&mut self, blk: u64, cg: CgNum) -> DepId {
		let id = self.alloc_id();
		self.new_blocks.insert(
			id,
			NewBlockDep {
				id,
				blk,
				cg,
				bitmap: false,
				content: false,
				publish: false,
				complete: false,
			},
		);
		self.by_block.insert(blk, id);
		id
	}

	/// Look up a new-block dependency.
	pub fn new_block_dep(&self, id: DepId) -> Option<&NewBlockDep> {
		self.new_blocks.get(&id)
	}

	/// The dependency a block is waiting on, if it has one.
	///
	/// `None` once the allocation is complete, so a caller must treat "no
	/// dependency" and "already resolved" as the same answer, which they are:
	/// either way there is nothing left to wait for.
	pub fn dep_for_block(&self, blk: u64) -> Option<DepId> {
		self.by_block.get(&blk).copied()
	}

	/// Every outstanding new-block allocation.
	///
	/// The flush loop walks this after each write-back to decide which
	/// allocation events a completed write made true.
	pub fn new_blocks(&self) -> impl Iterator<Item = &NewBlockDep> {
		self.new_blocks.values()
	}

	/// Register a newly allocated inode.
	pub fn new_inode(&mut self, inr: InodeNum, cg: CgNum) -> DepId {
		let id = self.alloc_id();
		self.inodes.insert(
			id,
			InodeDep {
				id,
				inr,
				cg,
				image: false,
				bitmap: false,
			},
		);
		self.by_inode.insert(inr, id);
		id
	}

	/// Every outstanding inode allocation.
	pub fn new_inodes(&self) -> impl Iterator<Item = &InodeDep> {
		self.inodes.values()
	}

	/// Record that an inode's image has reached the device.
	pub fn note_inode_written(&mut self, id: DepId) -> IoResult<bool> {
		let dep = self.inodes.get_mut(&id).ok_or_else(|| invalid_dep(id))?;
		dep.image = true;
		let (inr, safe) = (dep.inr, dep.allows_naming());
		self.settle_inode(inr);
		Ok(safe)
	}

	/// Record that an inode's allocation bitmap bit has reached the device.
	pub fn note_inode_bitmap_written(&mut self, id: DepId) -> IoResult<bool> {
		let dep = self.inodes.get_mut(&id).ok_or_else(|| invalid_dep(id))?;
		dep.bitmap = true;
		let (inr, safe) = (dep.inr, dep.allows_naming());
		self.settle_inode(inr);
		Ok(safe)
	}

	fn settle_inode(&mut self, inr: InodeNum) {
		if !self.gate_is_open(Gate::InodeWritten(inr)) {
			return;
		}
		if let Some(ids) = self.waiting_on.remove(&Gate::InodeWritten(inr)) {
			for id in ids {
				if self.deps.get(&id).is_some_and(|d| d.resolved) {
					continue;
				}
				self.publish_dep(id);
			}
		}
	}

	/// The allocation state of a new-block dependency.
	pub fn allocation_state(&self, id: DepId) -> Option<AllocationState> {
		self.new_blocks.get(&id).map(NewBlockDep::state)
	}

	/// Record that a block's allocation bitmap has reached the device.
	pub fn note_bitmap_written(&mut self, id: DepId) -> IoResult<bool> {
		self.note(id, |d| d.bitmap = true)
	}

	/// Record that a block's initialised contents have reached the device.
	pub fn note_contents_written(&mut self, id: DepId) -> IoResult<bool> {
		self.note(id, |d| d.content = true)
	}

	/// Record that a pointer to the block has reached the device.
	pub fn note_pointer_published(&mut self, id: DepId) -> IoResult<bool> {
		self.note(id, |d| {
			d.publish = true;
			d.finish();
		})
	}

	fn note(&mut self, id: DepId, f: impl FnOnce(&mut NewBlockDep)) -> IoResult<bool> {
		let dep = self
			.new_blocks
			.get_mut(&id)
			.ok_or_else(|| invalid_dep(id))?;
		f(dep);
		let safe = dep.allows_publication();
		let (blk, done) = (dep.blk, dep.complete);
		self.settle(&id);
		if done {
			// Nothing can be waiting on a finished allocation, so there is
			// nothing to keep.  Dropping it here is what stops `dep_for_block`
			// from reporting a resolved dependency as outstanding.
			self.by_block.remove(&blk);
			self.new_blocks.remove(&id);
		}
		Ok(safe)
	}

	/// Advance a new-block allocation and re-evaluate every gate that was
	/// waiting on it.
	///
	/// This is the *only* place a resource's progress becomes visible to
	/// pointers, which is why a caller cannot accidentally forget to unblock a
	/// dependency.
	fn settle(&mut self, id: &DepId) {
		let open: Vec<Gate> = [
			Gate::AllocationAllocated(*id),
			Gate::AllocationInitialised(*id),
			Gate::AllocationSafe(*id),
		]
		.into_iter()
		.filter(|g| self.gate_is_open(*g))
		.collect();
		for g in open {
			if let Some(ids) = self.waiting_on.remove(&g) {
				for d in ids {
					if self.deps.get(&d).is_some_and(|d| d.resolved) {
						continue;
					}
					self.publish_dep(d);
				}
			}
		}
	}

	fn publish_dep(&mut self, id: DepId) {
		if let Some(d) = self.deps.get_mut(&id) {
			d.resolved = true;
		}
	}

	/// Gate a byte range of a buffer until `gate` opens.
	///
	/// The range is marked unsafe in the buffer cache immediately, so a flush
	/// before the gate opens writes the safe image.
	pub fn gate(
		&mut self,
		cache: &mut BufferCache,
		kind: DepKind,
		blk: u64,
		off: u64,
		len: u64,
		gate: Gate,
	) -> IoResult<DepId> {
		let id = self.alloc_id();
		// The buffer must be resident: a caller that has just written a pointer
		// into it necessarily has, and a gate on a block that is not in the
		// cache would be silently ineffective.
		let Some(b) = cache.peek_mut(blk) else {
			return Err(std::io::Error::other(format!(
				"cannot gate block {blk}: it is not in the buffer cache"
			)));
		};
		b.mark_unsafe(off, len);
		self.deps.insert(
			id,
			Dependency {
				id,
				kind,
				blk,
				off,
				len,
				gate,
				resolved: false,
			},
		);
		// A gate whose precondition already holds must open immediately, or the
		// range would stay unpublished forever.
		//
		// `Gate::PointersRemoved` is the exception, and deliberately so.  "The
		// container's contents are on the disk" is true at the moment the gate is
		// created -- the pointer removal has not been staged yet -- and becomes
		// false again a moment later, when it is.  Deciding here would open the
		// gate and publish the freed block's accounting before the removal it is
		// waiting for had happened.  It is decided on the *next* write or fetch of
		// that container instead, which is the first moment the answer is true.
		let eager = !matches!(gate, Gate::PointersRemoved { .. }) && self.gate_is_open(gate);
		if eager {
			self.publish_dep(id);
		} else {
			self.waiting_on.entry(gate).or_default().push(id);
		}
		Ok(id)
	}

	/// Look up a dependency.
	pub fn dep(&self, id: DepId) -> Option<&Dependency> {
		self.deps.get(&id)
	}

	/// Every outstanding dependency.
	///
	/// Test-only, like the accessors on `Ufs`: the graph is the scheduler's
	/// business, and a caller that could enumerate it could also reason about
	/// gates it has no business touching.
	#[cfg(test)]
	pub fn all(&self) -> impl Iterator<Item = &Dependency> {
		self.deps.values()
	}

	/// Whether a dependency has been published.
	pub fn is_resolved(&self, id: DepId) -> bool {
		self.deps.get(&id).is_some_and(|d| d.resolved)
	}

	/// Declare that an event has happened, opening every gate that waited on
	/// it.
	pub fn signal(&mut self, cache: &mut BufferCache, gate: Gate) -> IoResult<()> {
		let _ = cache;
		let Some(ids) = self.waiting_on.remove(&gate) else {
			return Ok(());
		};
		for id in ids {
			self.publish_dep(id);
		}
		Ok(())
	}

	/// Publish every resolved dependency's byte range into `cache`.
	///
	/// The engine keeps the gates; the cache keeps the buffer state.  This is
	/// the single point where the two meet, which is what keeps the dependency
	/// graph free of any knowledge of `Buffer` internals.
	pub fn publish_into(&self, cache: &mut BufferCache) {
		for d in self.deps.values().filter(|d| d.resolved) {
			if let Some(b) = cache.peek_mut(d.blk) {
				b.publish(d.off, d.len);
			}
		}
	}

	/// Whether a buffer has any gated ranges left.
	pub fn block_is_publishable(&self, cache: &BufferCache, blk: u64) -> bool {
		!cache.peek(blk).is_some_and(|b| b.has_unsafe_ranges())
	}

	/// Write every buffer whose gates have opened, oldest first.
	///
	/// This is the scheduler.  It writes buffers in the order they became
	/// dirty, skips any buffer that still has a gated range, and reports what
	/// each write did so a caller (or a test) can tell a full write from a safe
	/// one.
	pub fn write_ready(
		&mut self,
		cache: &mut BufferCache,
		dev: &mut dyn crate::buf::BlockDevice,
	) -> IoResult<Vec<(u64, Written)>> {
		cache.write_back_all(dev)
	}

	/// Whether every dependency is resolved: the filesystem is in a
	/// crash-consistent state and the cache can be discarded.
	pub fn is_quiescent(&self) -> bool {
		self.deps.values().all(|d| d.resolved) &&
			self.new_blocks
				.values()
				.all(NewBlockDep::allows_publication) &&
			self.inodes.values().all(InodeDep::allows_naming) &&
			self.removals.values().all(DirectoryRemoveDep::is_persisted) &&
			self.reclaiming
				.keys()
				.all(|i| self.gate_is_open(Gate::InodeReclaimed(*i)))
	}

	fn alloc_id(&mut self) -> DepId {
		self.next += 1;
		DepId(self.next)
	}

	/// Re-evaluate a new-block allocation: has it become publishable?
	fn gate_is_open(&self, gate: Gate) -> bool {
		match gate {
			Gate::AllocationAllocated(id) => self.new_blocks.get(&id).is_some_and(|n| n.bitmap),
			Gate::AllocationInitialised(id) => self.new_blocks.get(&id).is_some_and(|n| n.content),
			Gate::AllocationSafe(id) => {
				self.new_blocks
					.get(&id)
					.is_some_and(NewBlockDep::allows_publication)
			}
			Gate::InodeWritten(inr) => {
				self.by_inode
					.get(&inr)
					.and_then(|id| self.inodes.get(id))
					.is_some_and(InodeDep::allows_naming)
			}
			Gate::DirectoryRemoved { parent, blk, off } => {
				self.removals
					.get(&Gate::DirectoryRemoved { parent, blk, off })
					.is_some_and(DirectoryRemoveDep::is_persisted)
			}
			Gate::PointersRemoved { container } => self.written.contains(&container),
			// An inode nobody is reclaiming has no transition to wait for, and a
			// gate on one is never created in the first place; the safe answer
			// here is "shut", so that a gate created without a reclamation behind
			// it cannot publish anything on its own.
			Gate::InodeReclaimed(inr) => {
				self.reclaiming.get(&inr).is_some_and(|(removal, blk)| {
					// The entry is gone *and* the cleared image is on the disk.
					self.gate_is_open(*removal) && self.written.contains(blk)
				})
			}
			Gate::InodeLinkCounted(_) | Gate::DirectoryPersisted { .. } => false,
		}
	}
}

impl DepKind {
	/// Alias kept for the documentation test's readability.
	#[cfg(test)]
	fn prevented_by_check(self) -> &'static str {
		self.prevents()
	}
}

#[cfg(test)]
mod t {
	use super::*;
	use crate::buf::BlockDevice;

	const BSIZE: u64 = 4096;

	#[derive(Default)]
	struct MemDev {
		data:   Vec<u8>,
		writes: Vec<(u64, Vec<u8>)>,
	}

	impl BlockDevice for MemDev {
		fn read_at(&mut self, off: u64, buf: &mut [u8]) -> IoResult<()> {
			let end = (off as usize + buf.len()).min(self.data.len());
			let start = (off as usize).min(end);
			buf[..end - start].copy_from_slice(&self.data[start..end]);
			Ok(())
		}

		fn write_at(&mut self, off: u64, buf: &[u8]) -> IoResult<()> {
			let end = off as usize + buf.len();
			if self.data.len() < end {
				self.data.resize(end, 0);
			}
			self.data[off as usize..end].copy_from_slice(buf);
			self.writes.push((off, buf.to_vec()));
			Ok(())
		}
	}

	fn setup() -> (DependencyEngine, BufferCache, MemDev) {
		let dev = MemDev {
			data:   vec![0u8; (BSIZE * 8) as usize],
			writes: Vec::new(),
		};
		(DependencyEngine::new(), BufferCache::new(BSIZE, BSIZE), dev)
	}

	/// The two first steps of a new-block dependency may happen in either
	/// order; neither alone allows publication.
	#[test]
	fn new_block_state_machine() {
		let mut e = DependencyEngine::new();
		let cg = CgNum::new(1);
		let id = e.new_block(617, cg);
		assert_eq!(e.allocation_state(id), Some(AllocationState::New));
		assert!(!e.new_block_dep(id).unwrap().allows_publication());

		e.note_bitmap_written(id).unwrap();
		assert_eq!(e.allocation_state(id), Some(AllocationState::BitmapWritten));
		assert!(!e.new_block_dep(id).unwrap().allows_publication());

		e.note_contents_written(id).unwrap();
		assert_eq!(
			e.allocation_state(id),
			Some(AllocationState::ContentsWritten)
		);
		assert!(e.new_block_dep(id).unwrap().allows_publication());
	}

	/// Contents-first is equally valid, which is the whole reason the state is
	/// two booleans and not a linear enum.
	#[test]
	fn contents_before_bitmap_is_equally_valid() {
		let mut e = DependencyEngine::new();
		let id = e.new_block(617, CgNum::new(1));
		e.note_contents_written(id).unwrap();
		assert!(!e.new_block_dep(id).unwrap().allows_publication());
		e.note_bitmap_written(id).unwrap();
		assert!(e.new_block_dep(id).unwrap().allows_publication());
	}

	/// An unknown dependency is an error rather than a silent no-op: a typo in
	/// a caller's bookkeeping would otherwise be an invisible crash-safety bug.
	#[test]
	fn unknown_dependency_is_an_error() {
		let mut e = DependencyEngine::new();
		assert!(e.note_bitmap_written(DepId(999)).is_err());
		assert!(e.note_contents_written(DepId(999)).is_err());
		assert!(e.note_pointer_published(DepId(999)).is_err());
	}

	/// The fundamental case: a direct pointer must not be publishable until the
	/// block's bitmap *and* contents are on disk.
	#[test]
	fn direct_pointer_waits_for_bitmap_and_contents() {
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 2).unwrap();
		let blk = e.new_block(617, CgNum::new(1));

		// The inode's direct[0] slot points at the new block.
		let _ptr = e
			.gate(
				&mut cache,
				DepKind::DirectPointer,
				2, // the inode's block
				0, // direct[0]
				8,
				Gate::AllocationSafe(blk),
			)
			.unwrap();
		assert!(cache.peek(2).unwrap().has_unsafe_ranges());
		assert!(!e.is_resolved(_ptr));

		// The bitmap alone is not enough.
		e.note_bitmap_written(blk).unwrap();
		assert!(!e.is_resolved(_ptr));
		e.publish_into(&mut cache);
		assert!(!e.block_is_publishable(&cache, 2));

		// Neither is the contents alone.
		let blk2 = e.new_block(618, CgNum::new(1));
		let _p2 = e
			.gate(
				&mut cache,
				DepKind::DirectPointer,
				2,
				8,
				8,
				Gate::AllocationSafe(blk2),
			)
			.unwrap();
		e.note_contents_written(blk2).unwrap();
		assert!(!e.is_resolved(_p2), "contents alone must not publish");
		e.publish_into(&mut cache);
		assert!(!e.is_resolved(_p2));
		assert!(cache.peek(2).unwrap().has_unsafe_ranges());

		// Both, and the gates open.
		e.note_contents_written(blk).unwrap();
		assert!(e.is_resolved(_ptr));
		assert!(!e.is_resolved(_p2));
		e.publish_into(&mut cache);
		assert!(cache.peek(2).unwrap().has_unsafe_ranges());

		e.note_bitmap_written(blk2).unwrap();
		assert!(e.is_resolved(_p2));
		e.publish_into(&mut cache);
		assert!(e.block_is_publishable(&cache, 2));
	}

	/// A direct pointer gate on a buffer that is flushed before its gate opens
	/// writes zeros for the pointer; the rest of the inode still persists.
	#[test]
	fn partial_write_keeps_the_rest_of_the_inode() {
		let (mut e, mut cache, mut dev) = setup();
		let blk = e.new_block(617, CgNum::new(1));

		cache.get_mut(&mut dev, 2).unwrap().data_mut()[0..8].copy_from_slice(&617u64.to_le_bytes());
		cache.get_mut(&mut dev, 2).unwrap().data_mut()[8..16]
			.copy_from_slice(&4242u64.to_le_bytes());
		e.gate(
			&mut cache,
			DepKind::DirectPointer,
			2,
			0,
			8,
			Gate::AllocationSafe(blk),
		)
		.unwrap();

		let w = e.write_ready(&mut cache, &mut dev).unwrap();
		assert_eq!(w, vec![(2, Written::Safe)]);
		assert_eq!(
			&dev.writes[0].1[0..8],
			&0u64.to_le_bytes(),
			"pointer not published"
		);
		assert_eq!(
			&dev.writes[0].1[8..16],
			&4242u64.to_le_bytes(),
			"the rest of the inode must persist"
		);

		// After the allocation completes, the pointer becomes publishable.
		e.note_bitmap_written(blk).unwrap();
		e.note_contents_written(blk).unwrap();
		e.publish_into(&mut cache);
		cache.get_mut(&mut dev, 2).unwrap();
		let w = e.write_ready(&mut cache, &mut dev).unwrap();
		assert_eq!(w, vec![(2, Written::Full)]);
		assert_eq!(&dev.writes[1].1[0..8], &617u64.to_le_bytes());
	}

	/// The crash-state table: after each prefix of the required ordering, is a
	/// persistent pointer to the new block present without it being allocated?
	#[test]
	fn crash_at_each_intermediate_point() {
		// (bitmap?, contents?) -> is the pointer safe to persist?
		let table = [
			(false, false, false),
			(true, false, false),
			(false, true, false),
			(true, true, true),
		];

		for (bitmap, contents, expect_safe) in table {
			let (mut e, mut cache, mut dev) = setup();
			let blk = e.new_block(617, CgNum::new(1));
			cache.get_mut(&mut dev, 2).unwrap().data_mut()[0..8]
				.copy_from_slice(&617u64.to_le_bytes());
			let p = e
				.gate(
					&mut cache,
					DepKind::DirectPointer,
					2,
					0,
					8,
					Gate::AllocationSafe(blk),
				)
				.unwrap();
			if bitmap {
				e.note_bitmap_written(blk).unwrap();
			}
			if contents {
				e.note_contents_written(blk).unwrap();
			}
			e.publish_into(&mut cache);
			e.write_ready(&mut cache, &mut dev).unwrap();

			let persisted = dev.writes[0].1[0..8] == 617u64.to_le_bytes();
			assert_eq!(
				persisted,
				expect_safe,
				"bitmap={bitmap} contents={contents}: the pointer must {} be persistable",
				if expect_safe { "" } else { "not" }
			);
			// Invariant: a persisted pointer implies an allocated block.
			if persisted {
				assert!(bitmap && contents);
			}
			assert_eq!(e.is_resolved(p), expect_safe);
		}
	}

	/// Two pointers into the same indirect block: one safe, one not.  The safe
	/// image must keep the first and zero the second, which is the `[A B 0 D]`
	/// example from the design.
	#[test]
	fn indirect_block_safe_image() {
		let (mut e, mut cache, mut dev) = setup();
		let a = e.new_block(100, CgNum::new(0));
		let b = e.new_block(101, CgNum::new(0));
		let c = e.new_block(102, CgNum::new(0));
		let d = e.new_block(103, CgNum::new(0));

		{
			let buf = cache.get_mut(&mut dev, 5).unwrap();
			for (slot, v) in [100u64, 101, 102, 103].iter().enumerate() {
				buf.data_mut()[slot * 8..slot * 8 + 8].copy_from_slice(&v.to_le_bytes());
			}
		}

		for (blk, dep, slot) in [
			(a, DepKind::IndirectPointer, 0usize),
			(b, DepKind::IndirectPointer, 1),
			(c, DepKind::IndirectPointer, 2),
			(d, DepKind::IndirectPointer, 3),
		] {
			e.gate(
				&mut cache,
				dep,
				5,
				(slot * 8) as u64,
				8,
				Gate::AllocationSafe(blk),
			)
			.unwrap();
		}

		// Only A and D are safe.
		e.note_bitmap_written(a).unwrap();
		e.note_contents_written(a).unwrap();
		e.note_bitmap_written(d).unwrap();
		e.note_contents_written(d).unwrap();
		e.publish_into(&mut cache);

		e.write_ready(&mut cache, &mut dev).unwrap();
		let img = &dev.writes[0].1;
		assert_eq!(&img[0..8], &100u64.to_le_bytes(), "A is safe");
		assert_eq!(&img[8..16], &0u64.to_le_bytes(), "B is not");
		assert_eq!(&img[16..24], &0u64.to_le_bytes(), "C is not");
		assert_eq!(&img[24..32], &103u64.to_le_bytes(), "D is safe");

		// Now B and C become safe too.
		for dep in [b, c] {
			e.note_bitmap_written(dep).unwrap();
			e.note_contents_written(dep).unwrap();
		}
		e.publish_into(&mut cache);
		cache.get_mut(&mut dev, 5).unwrap();
		e.write_ready(&mut cache, &mut dev).unwrap();
		let img = &dev.writes[1].1;
		assert_eq!(&img[8..16], &101u64.to_le_bytes());
		assert_eq!(&img[16..24], &102u64.to_le_bytes());
	}

	/// A directory entry may not persist before the inode it names.
	#[test]
	fn directory_add_gates_on_the_inode() {
		let (mut e, mut cache, mut dev) = setup();
		let ino = unsafe { InodeNum::new(700) };
		cache.get_mut(&mut dev, 4).unwrap().data_mut()[0..8].copy_from_slice(&700u64.to_le_bytes());
		let p = e
			.gate(
				&mut cache,
				DepKind::DirectoryAdd,
				4,
				0,
				8,
				Gate::InodeWritten(ino),
			)
			.unwrap();
		assert!(!e.is_resolved(p));

		e.write_ready(&mut cache, &mut dev).unwrap();
		assert_eq!(&dev.writes[0].1[0..8], &0u64.to_le_bytes());

		// The inode write signals the gate.  (Gates on inode state are opened
		// by `signal`, not by a resource allocation.)
		e.signal(&mut cache, Gate::InodeWritten(ino)).unwrap();
		assert!(e.is_resolved(p));
		e.publish_into(&mut cache);
		cache.get_mut(&mut dev, 4).unwrap();
		e.write_ready(&mut cache, &mut dev).unwrap();
		assert_eq!(&dev.writes[1].1[0..8], &700u64.to_le_bytes());
	}

	/// Gates on inode state do not open on their own.
	#[test]
	fn inode_gates_stay_closed_until_signalled() {
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 4).unwrap();
		let ino = unsafe { InodeNum::new(700) };
		for gate in [
			Gate::InodeWritten(ino),
			Gate::InodeLinkCounted(ino),
			Gate::DirectoryPersisted {
				parent: ino,
				blk:    4,
			},
			Gate::InodeReclaimed(ino),
		] {
			let p = e
				.gate(&mut cache, DepKind::DirectoryAdd, 4, 0, 8, gate)
				.unwrap();
			assert!(!e.is_resolved(p), "{gate:?} must not open by itself");
			e.signal(&mut cache, gate).unwrap();
			assert!(e.is_resolved(p), "{gate:?} must open on its signal");
		}
	}

	/// Gates on a *different* resource must not open each other.
	#[test]
	fn gates_are_not_cross_satisfied() {
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 2).unwrap();
		let a = e.new_block(100, CgNum::new(0));
		let b = e.new_block(101, CgNum::new(0));
		let pa = e
			.gate(
				&mut cache,
				DepKind::DirectPointer,
				2,
				0,
				8,
				Gate::AllocationSafe(a),
			)
			.unwrap();
		let pb = e
			.gate(
				&mut cache,
				DepKind::DirectPointer,
				2,
				8,
				8,
				Gate::AllocationSafe(b),
			)
			.unwrap();

		e.note_bitmap_written(a).unwrap();
		e.note_contents_written(a).unwrap();
		assert!(e.is_resolved(pa));
		assert!(!e.is_resolved(pb));

		e.note_bitmap_written(b).unwrap();
		assert!(!e.is_resolved(pb), "bitmap alone is not enough");
	}

	/// Every dependency documents what creates it, what it enforces, what it
	/// prevents, what resolves it and which invariant it protects.
	#[test]
	fn every_dependency_kind_documents_itself() {
		for kind in [
			DepKind::DirectPointer,
			DepKind::IndirectPointer,
			DepKind::DirectoryAdd,
		] {
			for s in [
				kind.created_by(),
				kind.enforces(),
				kind.prevented_by_check(),
				kind.resolved_by(),
				kind.protects(),
			] {
				assert!(!s.is_empty());
			}
		}
	}
}
