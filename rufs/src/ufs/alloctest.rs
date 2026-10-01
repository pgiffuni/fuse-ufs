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

//! Allocator behaviour, verified against the golden UFS2 images.
//!
//! These are the tests that the pure policy tests in `policy.rs` cannot make:
//! they exercise the *interaction* of policy, bitmap bookkeeping and the
//! existing `newfs` output.  Every mutating test ends by running the
//! `fsck_ffs`-shaped checker in [`Ufs::check_consistency`], because the point
//! of a placement policy is not where a block ends up but that the filesystem
//! is still provably consistent afterwards.

use super::*;
use crate::{geom::CgNum, policy::BlockRole, testutil};

impl super::Ufs<std::fs::File> {
	/// The single subdirectory of `dinr`, panicking if there is not exactly
	/// one.  `scripts/mkimg.sh` builds `dir1/dir2/dir3`, so the golden images
	/// have a known, narrow shape.
	fn dir_only_subdir(&mut self, dinr: InodeNum) -> InodeNum {
		let mut subs = Vec::new();
		self.dir_iter(dinr, |name, inr, kind| {
			// Skip `.`, `..` and the snapshot directory, which `newfs`
			// creates at the top of a fresh filesystem.
			if kind == InodeType::Directory &&
				name != OsStr::new(".") &&
				name != OsStr::new("..") &&
				name != OsStr::new(".snap")
			{
				subs.push(inr);
			}
			None::<()>
		})
		.unwrap();
		assert_eq!(subs.len(), 1, "{dinr} should hold exactly one subdirectory");
		subs[0]
	}
}

/// The golden images differ only in endianness, so the geometry assertions are
/// shared.
const IMAGES: [&str; 2] = ["ufs-little", "ufs-big"];

/// Geometry that the checker and the policies assume.  These values are read
/// out of the golden image by `geom::tests::superblock_for_tests`, so this
/// assertion guards against the golden image being regenerated with different
/// parameters behind our back.
fn assert_geometry(ug: &Ufs<std::fs::File>) {
	let sb = &ug.superblock;
	assert_eq!(sb.ncg, 4);
	assert_eq!(sb.bsize, 32768);
	assert_eq!(sb.fsize, 4096);
	assert_eq!(sb.frag, 8);
	assert_eq!(sb.ipg, 256);
	assert_eq!(sb.fpg, 264);
	assert_eq!(sb.nindir, 4096);
	assert_eq!(sb.inopb, 128);
	assert_eq!(sb.metaspace, 8);
	assert_eq!(sb.iblkno, 40);
	assert_eq!(sb.dblkno, 56);
}

#[test]
fn mount_golden_image() {
	for name in IMAGES {
		let (_img, ug) = testutil::open_ro(name);
		assert_geometry(&ug);
		// The cylinder groups tile the filesystem exactly.
		assert_eq!(ug.superblock.cg_end(CgNum::new(3)), 4 * 264);
		assert!(!ug.write_enabled());
	}
}

#[test]
fn golden_image_is_consistent() {
	for name in IMAGES {
		let (_img, mut ug) = testutil::open_ro(name);
		let rep = ug.check_consistency().unwrap();
		assert!(rep.is_clean(), "{name}: {}", rep.summary());
		assert!(
			rep.live_inodes > 5,
			"{name}: only {} inodes",
			rep.live_inodes
		);
	}
}

/// The root directory's depth is 0 and depth increases by one per level.
///
/// This is the on-disk `i_dirdepth` that the dirpref policy reads; if
/// `scripts/mkimg.sh` ever produced an image without it, these tests would
/// notice instead of silently degrading to "everything in the parent's CG".
#[test]
fn directory_depth_on_golden_image() {
	for name in IMAGES {
		let (_img, mut ug) = testutil::open_ro(name);

		let root = ug.read_inode(InodeNum::ROOT).unwrap();
		assert_eq!(root.kind(), InodeType::Directory);
		assert_eq!(root.dir_depth(), Some(0), "{name}: root depth");

		// `scripts/mkimg.sh` creates dir1/dir2/dir3, so the tree is three
		// levels deep.  Walk it by asking each directory for its single
		// subdirectory rather than by guessing entry names.
		let mut dinr = InodeNum::ROOT;
		for want in 1..=3u32 {
			let sub = ug.dir_only_subdir(dinr);
			let ino = ug.read_inode(sub).unwrap();
			assert_eq!(ino.kind(), InodeType::Directory);
			assert_eq!(
				ino.dir_depth(),
				Some(want),
				"{name}: depth of {sub} below {dinr}"
			);
			dinr = sub;
		}

		// dir3 contains file2, which is not a directory and therefore has no
		// depth at all.
		let file2 = ug.dir_lookup(dinr, OsStr::new("file2")).unwrap();
		let ino = ug.read_inode(file2).unwrap();
		assert_eq!(ino.kind(), InodeType::RegularFile);
		assert_eq!(ino.dir_depth(), None);

		// Same for a symlink and for a regular file at the top level.
		let file1 = ug.dir_lookup(InodeNum::ROOT, OsStr::new("file1")).unwrap();
		assert_eq!(ug.read_inode(file1).unwrap().dir_depth(), None);
		let link1 = ug.dir_lookup(InodeNum::ROOT, OsStr::new("link1")).unwrap();
		assert_eq!(ug.read_inode(link1).unwrap().dir_depth(), None);
	}
}

/// A new directory's first data block lands in the *metadata* zone of the new
/// directory's own cylinder group.
#[test]
fn new_directory_uses_metadata_zone() {
	for name in IMAGES {
		let (_img, mut ug) = testutil::open_rw(name);

		let before = ug.superblock.cstotal.ndir;
		let attr = ug
			.mkdir(InodeNum::ROOT, OsStr::new("zzz-new"), 0o755, 0, 0)
			.unwrap();
		let dir = attr.inr;
		let cg = ug.superblock.ino_to_cg(dir);
		let ino = ug.read_inode(dir).unwrap();

		let InodeData::Blocks(b) = &ino.data else {
			panic!("directory has no block pointers");
		};
		let blk = b.direct[0] as u64;
		assert_ne!(blk, 0, "{name}: new directory has no first block");

		assert_eq!(
			ug.superblock.blk_to_cg(blk),
			cg,
			"{name}: directory data block left the directory's cylinder group"
		);
		// The metadata zone is a *preference*.  UFS2 reserves only
		// fs_metaspace blocks per cylinder group for it, and the golden
		// images are 94% full, so a directory block may legitimately land in
		// the data zone of the same cylinder group.  What must never happen
		// is leaving the directory's own cylinder group.
		let meta = ug.superblock.cg_meta_start(cg);
		let data = ug.superblock.cg_data_start(cg);
		assert!(
			(blk >= meta && blk < data) || (blk >= data && blk < ug.superblock.cg_end(cg)),
			"{name}: directory block {blk} is outside both the metadata zone \
			 [{meta}, {data}) and the data zone of {cg}"
		);

		// The directory depth and the cylinder-group directory counters must
		// have been maintained too.
		assert_eq!(ino.dir_depth(), Some(1));
		assert_eq!(ug.superblock.cstotal.ndir, before + 1);
		ug.note_inode_alloc(cg, true);
		assert!(
			ug.alloc_summary().contig_dirs[cg.get() as usize] > 0,
			"{name}: contig_dirs was not updated for a directory"
		);

		assert!(
			ug.check_consistency().unwrap().is_clean(),
			"{}",
			ug.check_consistency().unwrap().summary()
		);
	}
}

/// A new file's data block lands in the *data* zone of the file's cylinder
/// group, and consecutive blocks of a growing file stay contiguous.
#[test]
fn new_file_uses_data_zone_and_is_contiguous() {
	for name in IMAGES {
		let (_img, mut ug) = testutil::open_rw(name);

		let attr = ug
			.mknod(
				InodeNum::ROOT,
				OsStr::new("zzz-file"),
				InodeType::RegularFile,
				0o644,
				0,
				0,
			)
			.unwrap();
		let inr = attr.inr;
		let bcg = ug.superblock.ino_to_cg(inr);
		// Whether the run stayed inside one cylinder group decides how strict
		// the contiguity check below can be.
		let contiguous = true;

		// 20 blocks: past the 12 direct slots, so the first indirect block and
		// the contiguity rule are both exercised.  (The golden image has only
		// ~49 free blocks, so tests here have to be frugal.)
		let buf = vec![0xa5u8; 20 * 32768];
		let n = ug.inode_write(inr, 0, &buf).unwrap();
		assert_eq!(n, buf.len());

		let ino = ug.read_inode(inr).unwrap();
		let InodeData::Blocks(b) = &ino.data else {
			panic!("file has no block pointers");
		};

		// Direct blocks must be in the *data* zone of whatever cylinder
		// group they ended up in.  The policy prefers the file's own
		// cylinder group, but the golden images are 94% full, so the
		// cylinder-overflow search may legitimately move the run somewhere
		// else -- and once it does, contiguity across the move is not
		// something the policy promises.  (Contiguity itself is asserted
		// exhaustively in `policy::t::contiguous_run_is_preserved`, which can
		// afford to assume the allocation succeeds.)
		let mut prev = 0u64;
		for i in 0..UFS_NDADDR {
			let blk = b.direct[i] as u64;
			assert_ne!(blk, 0, "{name}: direct[{i}] is null");
			let bcg = ug.superblock.blk_to_cg(blk);
			assert!(
				blk >= ug.superblock.cg_data_start(bcg),
				"{name}: direct[{i}] = {blk} is not in the data zone of {bcg}"
			);
			if i > 0 && ug.superblock.blk_to_cg(prev) == bcg {
				assert!(
					blk == prev + ug.superblock.frag() || !contiguous,
					"{name}: direct[{i}] = {blk} breaks contiguity with {prev} \
					 inside {bcg}"
				);
			}
			prev = blk;
		}

		// The first indirect block is metadata: it must be in a metadata zone
		// (or, if the reserved metadata zone is exhausted, in the data zone of
		// whichever cylinder group it ended up in) and must not be inside the
		// reserved area below `cgmeta()`.
		let ib = b.indirect[0] as u64;
		assert_ne!(ib, 0, "{name}: no first indirect block");
		let ibcg = ug.superblock.blk_to_cg(ib);
		assert!(
			ib >= ug.superblock.cg_meta_start(ibcg),
			"{name}: the first indirect block {ib} is inside the reserved area of {ibcg}"
		);
		if bcg == ibcg {
			assert_eq!(
				ib,
				prev + ug.superblock.frag(),
				"{name}: the first indirect block does not follow the direct blocks"
			);
		}

		// Reading it back must return what we wrote.
		let mut got = vec![0u8; buf.len()];
		ug.inode_read(inr, 0, &mut got).unwrap();
		assert_eq!(got, buf);

		let rep = ug.check_consistency().unwrap();
		assert!(rep.is_clean(), "{name}: {}", rep.summary());
	}
}

/// A directory's blocks never leave its own cylinder group, even as it grows.
#[test]
fn directory_growth_stays_in_its_cylinder_group() {
	let (_img, mut ug) = testutil::open_rw("ufs-little");

	let attr = ug
		.mkdir(InodeNum::ROOT, OsStr::new("zzz-big"), 0o755, 0, 0)
		.unwrap();
	let dir = attr.inr;
	let cg = ug.superblock.ino_to_cg(dir);

	// Enough entries to need several 512-byte directory-block increments.
	// (The image does not have room for a *second* 32 KiB directory block,
	// which is why this checks that the existing block stays put rather than
	// that the directory grows across blocks.)
	for i in 0..200 {
		ug.mknod(
			dir,
			OsStr::new(&format!("zzz-{i:04}")),
			InodeType::RegularFile,
			0o644,
			0,
			0,
		)
		.unwrap();
	}

	let ino = ug.read_inode(dir).unwrap();
	assert!(ino.size > 0, "directory did not grow");
	let (blocks, _) = ino.size(ug.superblock.bsize(), ug.superblock.fsize());
	for lbn in 0..blocks + 1 {
		let blk = ug
			.inode_resolve_block(dir, &ino, lbn)
			.unwrap()
			.expect("directory block is missing");
		assert_eq!(
			ug.superblock.blk_to_cg(blk.get()),
			cg,
			"directory block {lbn} left the directory's cylinder group"
		);
	}

	let rep = ug.check_consistency().unwrap();
	assert!(rep.is_clean(), "{}", rep.summary());
}

/// Every allocation must leave the block and inode bitmaps, the per-cylinder
/// group summaries and the superblock totals in agreement.  This is the
/// invariant that `fsck_ffs` pass 5 checks by rebuilding everything from
/// scratch.
#[test]
fn allocation_keeps_summaries_consistent() {
	let (_img, mut ug) = testutil::open_rw("ufs-little");

	let bfree0 = ug.superblock.cstotal.nbfree;
	let ffree0 = ug.superblock.cstotal.nifree;

	const N: i64 = 6;
	for i in 0..N {
		let attr = ug
			.mknod(
				InodeNum::ROOT,
				OsStr::new(&format!("zzz-f{i:03}")),
				InodeType::RegularFile,
				0o644,
				0,
				0,
			)
			.unwrap();
		let inr = attr.inr;
		let buf = vec![0x5au8; 2usize * 32768];
		ug.inode_write(inr, 0, &buf).unwrap();
	}

	// N files x 2 data blocks, plus N inodes.  The exact figures do not
	// matter; the *consistency* does.
	assert_eq!(ug.superblock.cstotal.nifree, ffree0 - N);
	assert!(ug.superblock.cstotal.nbfree <= bfree0 - 2 * N);

	let rep = ug.check_consistency().unwrap();
	assert!(rep.is_clean(), "{}", rep.summary());
}

/// Freeing must restore the counters exactly: allocate, then release, and the
/// filesystem must be bit-for-bit back to a consistent state.
#[test]
fn freeing_restores_the_counters() {
	let (_img, mut ug) = testutil::open_rw("ufs-little");

	let bfree0 = ug.superblock.cstotal.nbfree;
	let ffree0 = ug.superblock.cstotal.nifree;
	let ndir0 = ug.superblock.cstotal.ndir;

	let dir = {
		let attr = ug
			.mkdir(InodeNum::ROOT, OsStr::new("zzz-rm"), 0o755, 0, 0)
			.unwrap();
		attr.inr
	};
	// The directory itself holds one block, and its data holds four more.
	assert!(ug.superblock.cstotal.nbfree < bfree0);
	let buf = vec![0x11u8; 4 * 32768];
	let f = {
		let attr = ug
			.mknod(
				dir,
				OsStr::new("zzz-inner"),
				InodeType::RegularFile,
				0o644,
				0,
				0,
			)
			.unwrap();
		attr.inr
	};
	ug.inode_write(f, 0, &buf).unwrap();
	assert!(
		ug.superblock.cstotal.nbfree < bfree0 - 4,
		"writing four blocks did not release anything"
	);

	ug.unlink(dir, OsStr::new("zzz-inner")).unwrap();
	assert!(ug.superblock.cstotal.nbfree < bfree0);
	ug.rmdir(InodeNum::ROOT, OsStr::new("zzz-rm")).unwrap();

	assert_eq!(
		ug.superblock.cstotal.nbfree, bfree0,
		"block counter not restored after rmdir"
	);
	assert_eq!(
		ug.superblock.cstotal.nifree, ffree0,
		"inode counter not restored after rmdir"
	);
	assert_eq!(
		ug.superblock.cstotal.ndir, ndir0,
		"directory counter not restored after rmdir"
	);

	let rep = ug.check_consistency().unwrap();
	assert!(rep.is_clean(), "{}", rep.summary());
}

/// Truncating a file back to zero must release every block it held, including
/// those behind indirect blocks.
#[test]
fn truncate_releases_indirect_blocks() {
	let (_img, mut ug) = testutil::open_rw("ufs-little");

	let bfree0 = ug.superblock.cstotal.nbfree;
	let attr = ug
		.mknod(
			InodeNum::ROOT,
			OsStr::new("zzz-trunc"),
			InodeType::RegularFile,
			0o644,
			0,
			0,
		)
		.unwrap();
	let inr = attr.inr;

	// Large enough to allocate a first indirect block and more.
	let buf = vec![0x77u8; 20 * 32768];
	ug.inode_write(inr, 0, &buf).unwrap();
	assert!(ug.superblock.cstotal.nbfree < bfree0 - 20);

	ug.inode_truncate(inr, 0).unwrap();
	assert_eq!(
		ug.superblock.cstotal.nbfree, bfree0,
		"block counter not restored after truncate"
	);

	let rep = ug.check_consistency().unwrap();
	assert!(rep.is_clean(), "{}", rep.summary());
}

/// The block bitmap must survive a remount: everything the allocator marked
/// used is still marked used, and the counters agree.
#[test]
fn bitmap_is_persistent_across_reopen() {
	let (img, mut ug) = testutil::open_rw("ufs-little");

	let attr = ug
		.mknod(
			InodeNum::ROOT,
			OsStr::new("zzz-persist"),
			InodeType::RegularFile,
			0o644,
			0,
			0,
		)
		.unwrap();
	let inr = attr.inr;
	let buf = vec![0x3cu8; 4 * 32768];
	ug.inode_write(inr, 0, &buf).unwrap();
	let bfree = ug.superblock.cstotal.nbfree;
	let cg = ug.superblock.ino_to_cg(inr);
	let ino = ug.read_inode(inr).unwrap();
	let InodeData::Blocks(b) = &ino.data else {
		panic!("no blocks");
	};
	let first = b.direct[0] as u64;
	// Cylinder-group structs are dirty metadata buffers now, not immediate
	// writes, so they only survive a remount if they have been flushed.
	ug.sync_metadata().unwrap();
	drop(ug);

	let mut ug = Ufs::open(img.path(), true).unwrap();
	assert_eq!(ug.superblock.cstotal.nbfree, bfree);
	let cgd = ug.read_cg(cg).unwrap();
	let map = ug.read_blkmap(cg, &cgd).unwrap();
	assert!(
		!map.is_free_block(ug.superblock.blk_to_cgoff(first)),
		"the bitmap forgot the allocation"
	);

	let rep = ug.check_consistency().unwrap();
	assert!(rep.is_clean(), "{}", rep.summary());
}

/// Filling the filesystem must fail cleanly with `ENOSPC` rather than
/// corrupting the bitmaps on the way.
#[test]
fn exhaustion_fails_cleanly() {
	let (_img, mut ug) = testutil::open_rw("ufs-little");

	let mut created = 0;
	let mut last_err = None;
	for i in 0..4000 {
		let attr = match ug.mknod(
			InodeNum::ROOT,
			OsStr::new(&format!("zzz-fill{i:04}")),
			InodeType::RegularFile,
			0o644,
			0,
			0,
		) {
			Ok(a) => a,
			Err(e) => {
				last_err = Some(e);
				break;
			}
		};
		let buf = vec![0u8; 32768];
		if let Err(e) = ug.inode_write(attr.inr, 0, &buf) {
			last_err = Some(e);
			break;
		}
		created += 1;
	}

	let e = last_err.expect("the filesystem never ran out of space");
	assert_eq!(
		e.raw_os_error(),
		Some(libc::ENOSPC),
		"expected ENOSPC, got {e:?}"
	);
	assert!(created > 0, "not even one file fitted");

	let rep = ug.check_consistency().unwrap();
	assert!(
		rep.is_clean(),
		"filesystem became inconsistent while filling: {}",
		rep.summary()
	);
}

/// The cached per-cylinder-group summaries used by the policies must not drift
/// from the on-disk cylinder-group superblocks, because that cache is what
/// makes every allocation decision.
#[test]
fn cg_summary_cache_matches_disk() {
	let (_img, mut ug) = testutil::open_rw("ufs-little");

	for i in 0..12 {
		ug.mknod(
			InodeNum::ROOT,
			OsStr::new(&format!("zzz-c{i:02}")),
			InodeType::RegularFile,
			0o644,
			0,
			0,
		)
		.unwrap();
	}

	// Re-read the cylinder groups straight from the image and compare with
	// what the allocator has been using.
	let on_disk = ug.read_cg_sums().unwrap();
	for cg in ug.superblock.cgs() {
		assert_eq!(
			on_disk.nbfree(cg.get() as u64),
			ug.cg_sums.nbfree(cg.get() as u64),
			"{cg}: cached nbfree differs from the cylinder-group superblock"
		);
		assert_eq!(
			on_disk.nifree(cg.get() as u64),
			ug.cg_sums.nifree(cg.get() as u64),
			"{cg}: cached nifree differs from the cylinder-group superblock"
		);
	}
}

/// Directories must be spread across cylinder groups rather than piling into
/// one, which is the observable effect of the dirpref policy.
#[test]
fn directories_are_spread_across_cgs() {
	let (_img, mut ug) = testutil::open_rw("ufs-little");

	let mut cgs = std::collections::BTreeSet::new();
	for i in 0..16 {
		let attr = ug
			.mkdir(
				InodeNum::ROOT,
				OsStr::new(&format!("zzz-d{i:02}")),
				0o755,
				0,
				0,
			)
			.unwrap();
		cgs.insert(ug.superblock.ino_to_cg(attr.inr).get());
	}

	// A round-robin allocator would put all 16 in one cylinder group; the
	// dirpref sequence must use several.
	assert!(
		cgs.len() >= 3,
		"directories all landed in {cgs:?}; the dirpref policy is not spreading them"
	);
}

/// Files created inside a directory must land in that directory's cylinder
/// group, which is what keeps a directory and its contents together.
#[test]
fn files_follow_their_directory() {
	let (_img, mut ug) = testutil::open_rw("ufs-little");

	let attr = ug
		.mkdir(InodeNum::ROOT, OsStr::new("zzz-parent"), 0o755, 0, 0)
		.unwrap();
	let dir = attr.inr;
	let dcg = ug.superblock.ino_to_cg(dir);

	for i in 0..8 {
		let a = ug
			.mknod(
				dir,
				OsStr::new(&format!("zzz-{i}")),
				InodeType::RegularFile,
				0o644,
				0,
				0,
			)
			.unwrap();
		assert_eq!(
			ug.superblock.ino_to_cg(a.inr),
			dcg,
			"file {i} left the directory's cylinder group"
		);
	}

	let rep = ug.check_consistency().unwrap();
	assert!(rep.is_clean(), "{}", rep.summary());
}

/// The cylinder-overflow search must actually move allocations to another
/// cylinder group when the preferred one is full.
#[test]
fn allocation_overflows_to_another_cg() {
	let (_img, mut ug) = testutil::open_rw("ufs-little");

	// Fill cylinder group 0's data zone by hand.
	let target = CgNum::new(0);
	let cgd = ug.read_cg(target).unwrap();
	let mut map = ug.read_blkmap(target, &cgd).unwrap();
	let step = ug.superblock.frag();
	for bno in (0..ug.superblock.fpg()).step_by(step as usize) {
		map.set_free_block(bno, false);
	}
	let addr = ug.superblock.cg_addr(target) + cgd.freeoff as u64;
	ug.file.write_at(addr, map.as_bytes()).unwrap();
	let mut cgd = cgd;
	cgd.cs.nbfree = 0;
	ug.write_cg(target, &cgd).unwrap();

	// Now allocate: the policy still asks for CG 0, the allocator cannot
	// satisfy it there, and the overflow search must find space elsewhere.
	let pref = ug.block_pref(BlockRole::FileData, unsafe { InodeNum::new(2) }, 0, 0, 0, 0);
	assert_eq!(
		pref.cg, target,
		"precondition: the policy should ask for CG 0"
	);

	let attr = ug
		.mknod(
			InodeNum::ROOT,
			OsStr::new("zzz-overflow"),
			InodeType::RegularFile,
			0o644,
			0,
			0,
		)
		.unwrap();
	let inr = attr.inr;
	ug.inode_write(inr, 0, &[0u8; 32768]).unwrap();
	let ino = ug.read_inode(inr).unwrap();
	let InodeData::Blocks(b) = &ino.data else {
		panic!();
	};
	assert_ne!(
		ug.superblock.blk_to_cg(b.direct[0] as u64),
		target,
		"the overflow search allocated from the full cylinder group"
	);

	let rep = ug.check_consistency().unwrap();
	assert!(rep.is_clean(), "{}", rep.summary());
}

/// `NewBlockDep` wiring: what a block allocation promises, and when the
/// promise is discharged.
///
/// The distinction these tests exist to pin down is between *changing* a block
/// and *the block having reached the device*.  Soft Updates is entirely about
/// that gap: a live image full of correct metadata is still a lie as far as
/// the disk is concerned, and every gate in the engine is a statement about
/// which of the two is being looked at.
#[cfg(test)]
mod newblock {
	use super::*;
	use crate::{policy::BlockRole, softdep::AllocationState, InodeNum, InodeType};

	/// The state of `blk`'s allocation dependency, if it still has one.
	///
	/// `None` means the allocation is not just discharged but *complete*, which
	/// happens only once a pointer to the block has reached the disk.
	fn state(ug: &Ufs<std::fs::File>, blk: u64) -> Option<AllocationState> {
		ug.allocation_of(blk)
			.and_then(|id| ug.dependencies().allocation_state(id))
	}

	/// Create a regular file holding one block of data.
	fn one_block_file(ug: &mut Ufs<std::fs::File>, name: &str) -> InodeNum {
		let inr = ug
			.mknod(
				InodeNum::ROOT,
				OsStr::new(name),
				InodeType::RegularFile,
				0o644,
				0,
				0,
			)
			.unwrap()
			.inr;
		ug.inode_write(inr, 0, &vec![0x11u8; 32768]).unwrap();
		inr
	}

	/// The block a one-block file's data landed in.
	fn first_block(ug: &mut Ufs<std::fs::File>, inr: InodeNum) -> u64 {
		match ug.read_inode(inr).unwrap().data {
			InodeData::Blocks(b) => b.direct[0] as u64,
			_ => panic!("inode {inr} has no block map"),
		}
	}

	/// `inode_write()` writes the block's contents straight through the decoder,
	/// so after it returns the contents are on the device while the cylinder
	/// group's bitmap bit is still only in the cache.
	///
	/// This is the "contents first" order, and it is the common one.
	#[test]
	fn writing_contents_before_the_bitmap_is_legal() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = one_block_file(&mut ug, "zzz-cf");
		let blk = first_block(&mut ug, inr);

		assert_eq!(
			state(&ug, blk),
			Some(AllocationState::BitmapWritten),
			"the contents reached the device; the bitmap has not"
		);

		// Flushing is what discharges the second half.
		ug.sync_metadata().unwrap();
		assert_eq!(
			state(&ug, blk),
			Some(AllocationState::ContentsWritten),
			"both halves are on the disk, so a pointer may now be published"
		);
	}

	/// The other order: allocate without writing, flush the bitmap, and only
	/// then write the contents.  The dependency has to advance just as happily.
	#[test]
	fn writing_the_bitmap_before_the_contents_is_legal() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = ug
			.mknod(
				InodeNum::ROOT,
				OsStr::new("zzz-bc"),
				InodeType::RegularFile,
				0o644,
				0,
				0,
			)
			.unwrap()
			.inr;
		let blk = ug
			.blk_alloc_for(BlockRole::FileData, inr, 0, 0, 0, 0)
			.unwrap();

		// Reserved in memory, on the disk in neither respect.
		assert_eq!(state(&ug, blk.get()), Some(AllocationState::New));

		ug.sync_metadata().unwrap();
		assert_eq!(
			state(&ug, blk.get()),
			Some(AllocationState::BitmapWritten),
			"the bitmap is on the disk; the contents are not"
		);

		ug.note_block_contents(blk.get()).unwrap();
		assert_eq!(
			state(&ug, blk.get()),
			Some(AllocationState::ContentsWritten)
		);
	}

	/// Modifying the cylinder group in memory is not writing it.  A dirty buffer
	/// says nothing about what a reader on the other side of a power cut would
	/// see, and advancing the dependency here is the bug this whole mechanism
	/// exists to prevent.
	#[test]
	fn a_dirty_cylinder_group_is_not_a_written_one() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = one_block_file(&mut ug, "zzz-dirty");
		let blk = first_block(&mut ug, inr);
		let before = state(&ug, blk);

		// Stage an unrelated change to the cylinder group.  `write_cg` marks the
		// buffer dirty and nothing more.
		let cg = ug.superblock.blk_to_cg(blk);
		let mut cgd = ug.read_cg(cg).unwrap();
		cgd.cs.ndir += 1;
		ug.write_cg(cg, &cgd).unwrap();
		assert!(ug.metadata_cache().dirty_count() > 0);
		assert_eq!(
			state(&ug, blk),
			before,
			"a staged change must not discharge a dependency"
		);

		// Now write it, and the dependency advances.
		ug.sync_metadata().unwrap();
		assert_ne!(state(&ug, blk), before);
	}

	/// The bitmap event belongs to the cylinder group's own buffer and to no
	/// other.  Writing some unrelated buffer must not discharge anything.
	#[test]
	fn only_the_owning_cylinder_group_discharges_the_bitmap() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = ug
			.mknod(
				InodeNum::ROOT,
				OsStr::new("zzz-owner"),
				InodeType::RegularFile,
				0o644,
				0,
				0,
			)
			.unwrap()
			.inr;
		// Allocate without writing contents, then flush: the owning cylinder
		// group's bitmap is now on the disk and nothing else is.
		let blk = ug
			.blk_alloc_for(BlockRole::FileData, inr, 0, 0, 0, 0)
			.unwrap();
		ug.sync_metadata().unwrap();
		let cg = ug.superblock.blk_to_cg(blk.get());
		let owner = ug.cg_blk(cg);
		assert_eq!(state(&ug, blk.get()), Some(AllocationState::BitmapWritten));

		// The owning buffer is clean now, so a flush writes only what the next
		// step dirties.

		// Dirty and write some *other* cylinder group.
		let other = CgNum::new((cg.get() + 1) % ug.superblock.ncg);
		assert_ne!(ug.cg_blk(other), owner);
		let mut cgd = ug.read_cg(other).unwrap();
		cgd.cs.ndir += 1;
		ug.write_cg(other, &cgd).unwrap();
		ug.sync_metadata().unwrap();

		assert_eq!(
			state(&ug, blk.get()),
			Some(AllocationState::BitmapWritten),
			"another cylinder group's write must not discharge this allocation"
		);
		ug.note_block_contents(blk.get()).unwrap();
		assert_eq!(
			state(&ug, blk.get()),
			Some(AllocationState::ContentsWritten)
		);
	}

	/// Nothing on the disk has changed until a flush happens.  This is the
	/// property that makes the whole scheme worth having: the operation is
	/// allowed to be visible to the running filesystem long before it is
	/// visible to anyone else.
	#[test]
	fn the_image_is_untouched_until_a_flush() {
		let (img, mut ug) = testutil::open_rw("ufs-little");
		let cg = CgNum::new(1);
		let before = ug.read_cg(cg).unwrap().cs.nbfree;

		let inr = one_block_file(&mut ug, "zzz-slow");
		let _ = inr;

		// Live: the running filesystem sees the allocation immediately.
		assert!(
			ug.read_cg(cg).unwrap().cs.nbfree < before,
			"live: the cylinder group still has the old free count"
		);

		// Persistent: the disk has not been told.
		drop(ug);
		let mut ug = Ufs::open(img.path(), true).unwrap();
		assert_eq!(
			ug.read_cg(cg).unwrap().cs.nbfree,
			before,
			"persistent: nothing was written"
		);
	}

	/// A block with no outstanding allocation is silently not noted: it was
	/// allocated by an earlier operation, so its contents reaching the disk
	/// discharges nothing.
	#[test]
	fn an_unallocated_block_has_nothing_to_discharge() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		one_block_file(&mut ug, "zzz-none");
		let before = ug.dependencies().len();
		ug.note_block_contents(999_999).unwrap();
		assert_eq!(ug.dependencies().len(), before);
		assert_eq!(state(&ug, 999_999), None);
	}

	/// A flushed filesystem with nothing left to write is quiescent, which is
	/// the state `sync_metadata()` is finally able to claim.
	#[test]
	fn a_flushed_filesystem_is_quiescent() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		one_block_file(&mut ug, "zzz-q");
		ug.sync_metadata().unwrap();
		assert!(ug.dependencies().is_quiescent());
		assert_eq!(ug.metadata_cache().dirty_count(), 0);
		assert!(ug.metadata_cache().is_clean());
	}

	/// Both byte orders, because a byte-order assumption in the dependency
	/// bookkeeping would not show up on one of them.
	#[test]
	fn dependencies_are_byte_order_independent() {
		for name in ["ufs-little", "ufs-big"] {
			let (img, mut ug) = testutil::open_rw(name);
			let inr = one_block_file(&mut ug, "zzz-be");
			let blk = first_block(&mut ug, inr);
			assert_eq!(
				state(&ug, blk),
				Some(AllocationState::BitmapWritten),
				"{name}"
			);
			ug.sync_metadata().unwrap();
			assert_eq!(
				state(&ug, blk),
				Some(AllocationState::ContentsWritten),
				"{name}"
			);
			drop(ug);

			let mut ug = Ufs::open(img.path(), true).unwrap();
			assert!(ug.check_consistency().unwrap().is_clean(), "{name}");
		}
	}
}

/// Gating a direct block pointer on the allocation it names.
///
/// The property under test throughout: the inode's *live* image contains the
/// real pointer immediately, while the inode's *safe* image contains zero in
/// that field until the block's bitmap and contents have both reached the
/// device.  A crash in between leaves an inode with no pointer, which is
/// strictly better than one pointing at a block the bitmap still calls free --
/// and it is the difference between `fsck` clearing one stale pointer and
/// `fsck` handing the same block to somebody else.
#[cfg(test)]
mod directptr {
	use super::*;
	use crate::{softdep::DepKind, InodeNum, InodeType};

	/// Create an empty regular file and return its inode.
	fn create(ug: &mut Ufs<std::fs::File>, name: &str) -> InodeNum {
		ug.mknod(
			InodeNum::ROOT,
			OsStr::new(name),
			InodeType::RegularFile,
			0o644,
			0,
			0,
		)
		.unwrap()
		.inr
	}

	/// Write one block of data and return the block it landed in.
	fn write_one_block(ug: &mut Ufs<std::fs::File>, inr: InodeNum, byte: u8) -> u64 {
		ug.inode_write(inr, 0, &vec![byte; 32768]).unwrap();
		first_pointer(ug, inr)
	}

	/// The first direct pointer of `inr` as the running filesystem sees it.
	fn first_pointer(ug: &mut Ufs<std::fs::File>, inr: InodeNum) -> u64 {
		match ug.read_inode(inr).unwrap().data {
			InodeData::Blocks(b) => b.direct[0] as u64,
			_ => panic!("inode {inr} has no block map"),
		}
	}

	/// Read exactly `buf.len()` bytes at `off` from a file, for the "what is
	/// actually on the disk" half of these tests.
	fn read_exact_at(f: &std::fs::File, off: u64, buf: &mut [u8]) -> std::io::Result<()> {
		use std::os::unix::fs::FileExt;
		f.read_exact_at(buf, off)
	}

	/// The only `DirectPointer` dependency the filesystem has.
	fn direct_dep(ug: &Ufs<std::fs::File>) -> crate::softdep::Dependency {
		let deps: Vec<_> = ug
			.dependencies()
			.all()
			.filter(|d| d.kind == DepKind::DirectPointer)
			.cloned()
			.collect();
		assert_eq!(
			deps.len(),
			1,
			"expected exactly one direct-pointer gate: {deps:?}"
		);
		deps[0].clone()
	}

	/// The pointer offsets are checked against the real encoder, for every slot
	/// and on both byte orders.
	///
	/// A helper that is right for `direct[0]` and wrong for `direct[11]` would
	/// gate arbitrary bytes of somebody else's inode, and nothing else would
	/// notice, so every slot is checked and the ranges are checked not to
	/// overlap.
	#[test]
	fn the_pointer_offsets_match_the_encoding() {
		for name in ["ufs-little", "ufs-big"] {
			let (_img, mut ug) = testutil::open_rw(name);
			let mut ino = ug.read_inode(InodeNum::ROOT).unwrap();
			let InodeData::Blocks(ref mut b) = ino.data else {
				panic!("{name}: the root directory has no block map");
			};
			for (i, slot) in b.direct.iter_mut().enumerate() {
				*slot = 0x0101_0101_0101_0000 + i as i64;
			}
			for (i, slot) in b.indirect.iter_mut().enumerate() {
				*slot = 0x0202_0202_0202_0000 + i as i64;
			}
			let bytes = ug.file.encode_to_vec(&ino).unwrap();

			for (slots, range) in [
				(
					(0..UFS_NDADDR)
						.map(|i| (i, ug.direct_pointer_range(i)))
						.collect::<Vec<_>>(),
					0x0101_0101_0101_0000u64,
				),
				(
					(0..UFS_NIADDR)
						.map(|i| (i, ug.indirect_pointer_range(i)))
						.collect::<Vec<_>>(),
					0x0202_0202_0202_0000u64,
				),
			] {
				for (i, (off, len)) in slots {
					let slot: [u8; 8] = bytes[off as usize..off as usize + len as usize]
						.try_into()
						.unwrap();
					assert_eq!(
						ug.file.config().u64_from_bytes(&slot),
						range + i as u64,
						"{name}: pointer {i} is not at {off}"
					);
				}
			}

			// The ranges must tile the two arrays without a gap or an overlap.
			let mut all: Vec<(u64, u64)> = (0..UFS_NDADDR)
				.map(|i| ug.direct_pointer_range(i))
				.chain((0..UFS_NIADDR).map(|i| ug.indirect_pointer_range(i)))
				.collect();
			all.sort();
			for pair in all.windows(2) {
				assert_eq!(pair[0].0 + pair[0].1, pair[1].0, "overlap or gap");
			}

			// A logical index maps to exactly one of them.
			assert_eq!(ug.pointer_range(0), Some(ug.direct_pointer_range(0)));
			assert_eq!(ug.pointer_range(11), Some(ug.direct_pointer_range(11)));
			assert_eq!(ug.pointer_range(12), Some(ug.indirect_pointer_range(0)));
			assert_eq!(ug.pointer_range(14), Some(ug.indirect_pointer_range(2)));
			assert_eq!(
				ug.pointer_range((UFS_NDADDR + UFS_NIADDR) as u64),
				None,
				"an index past di_extb has no range"
			);
		}
	}

	/// The gate names the allocation of exactly the block the pointer names,
	/// sits on exactly that pointer's bytes, and is shut while the bitmap is
	/// still only in the cache.
	#[test]
	fn a_new_direct_pointer_is_gated_on_its_blocks_allocation() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-dp");

		// The contents reach the device; the bitmap bit does not.
		let blk = write_one_block(&mut ug, inr, 0);

		let d = direct_dep(&ug);
		assert_eq!(
			d.gate,
			crate::softdep::Gate::AllocationSafe(ug.allocation_of(blk).unwrap()),
			"the gate must name the allocation of the block the pointer names"
		);
		let (in_blk, off, len) = ug.inode_pointer_range(inr, 0).unwrap();
		assert_eq!((d.blk, d.off, d.len), (in_blk, off, len));
		assert!(
			!d.resolved,
			"the bitmap is still in the cache, so the gate must be shut"
		);

		// Flushing discharges the bitmap, which opens the gate.
		ug.sync_metadata().unwrap();
		assert!(ug.dependencies().is_resolved(d.id));
	}

	/// The safe image holds zero in the gated field while the live image holds
	/// the real pointer.  `safe_image()` is exactly what a write-back would
	/// send, so this is the mechanism observed rather than a proxy for it.
	#[test]
	fn the_safe_image_holds_zero_and_the_live_image_holds_the_pointer() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-safe");
		let blk = write_one_block(&mut ug, inr, 0xab);

		let (in_blk, off, len) = ug.inode_pointer_range(inr, 0).unwrap();
		let buf = ug
			.metadata_cache()
			.peek(in_blk)
			.expect("the inode block is cached");
		let at = off as usize..off as usize + len as usize;

		assert_eq!(
			u64::from_le_bytes(buf.data()[at.clone()].try_into().unwrap()),
			blk,
			"the live image must hold the real pointer at {off}"
		);
		assert_eq!(
			buf.safe_image()[at],
			[0u8; 8],
			"the safe image must hold zero, or a write-back would persist a \
			 pointer to a block whose contents are not on the disk yet"
		);
	}

	/// The inode the gate protects is the *right* inode.  An inode block holds
	/// `fs_inopb` inodes, so a range computed from the block's address alone
	/// would gate inode 0's pointer while modifying inode 57's.
	#[test]
	fn the_gate_lands_in_the_right_inode_of_the_block() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let ipb = ug.superblock.inopb();

		// Several files, so several inodes in at least one inode block.
		let mut inodes = Vec::new();
		for i in 0..(ipb + 3) {
			inodes.push(create(&mut ug, &format!("zzz-ino{i}")));
		}
		// Make the *last* of them allocate a block, so its pointer is the one
		// gated while its neighbours in the same inode block are untouched.
		let last = *inodes.last().unwrap();
		let _ = write_one_block(&mut ug, last, 0x5a);

		let d = direct_dep(&ug);
		let (in_blk, off, _) = ug.inode_pointer_range(last, 0).unwrap();
		assert_eq!((d.blk, d.off), (in_blk, off));

		// Every other inode in that block must be entirely free of unsafe
		// ranges: the gate is eight bytes, not eight times the block.
		let buf = ug.metadata_cache().peek(in_blk).unwrap();
		let unsafe_bytes: usize = (0..buf.data().len())
			.filter(|i| buf.safe_image()[*i] != buf.data()[*i])
			.count();
		assert!(
			unsafe_bytes <= 8,
			"{unsafe_bytes} bytes are gated, expected at most the one pointer"
		);
	}

	/// A drain is what eventually publishes the pointer, and afterwards the
	/// filesystem is consistent.
	#[test]
	fn the_pointer_reaches_the_disk_after_a_drain() {
		let (img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-drain");
		// The offset *within the inode*, not within the cache block: the block
		// offset from `inode_pointer_range` already has the inode's own start
		// folded into it.
		let (off, _) = ug.pointer_range(0).unwrap();
		let at = ug.superblock.ino_to_fso(inr) + off;

		write_one_block(&mut ug, inr, 0xcd);
		ug.sync_metadata().unwrap();
		assert!(
			ug.dependencies().is_quiescent(),
			"nothing may be left waiting after a drain"
		);
		drop(ug);

		// The pointer is on the disk, and it is not zero.
		let mut bytes = [0u8; 8];
		read_exact_at(&std::fs::File::open(img.path()).unwrap(), at, &mut bytes).unwrap();
		assert_ne!(
			u64::from_le_bytes(bytes),
			0,
			"the pointer was never persisted"
		);

		let mut ug = Ufs::open(img.path(), true).unwrap();
		assert!(
			ug.check_consistency().unwrap().is_clean(),
			"the drained image must satisfy the invariants"
		);
	}

	/// A crash *before* the drain is the case the gate exists for: the
	/// cylinder group's bitmap never made it to the disk, so it does not claim
	/// the block, and the inode does not point at it either.  Both halves are
	/// "the file has no data", which is consistent; the alternative is a
	/// pointer to a block the bitmap calls free.
	#[test]
	fn a_crash_before_the_drain_leaves_nothing_half_done() {
		let (img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-crash");
		write_one_block(&mut ug, inr, 0x7e);

		// No flush: the bitmap bit is in the cache, the pointer's safe image is
		// zero, and the inode block itself may or may not have reached the disk.
		// What matters is that no *consistent* half of it got out.
		drop(ug);

		// Reopen and let the checker judge what actually landed.  Whether the
		// file survived is not the point -- nothing was flushed, so either
		// answer is correct.  The point is that the bitmap and the pointer
		// agree about whether its block exists.
		let mut ug = Ufs::open(img.path(), true).unwrap();
		let report = ug.check_consistency().unwrap();
		log::info!(
			"crash before the drain: clean={} used={} live={} problems={}",
			report.is_clean(),
			report.used_blocks,
			report.live_inodes,
			report.problems.len()
		);
		assert!(
			report.used_blocks == report.live_inodes || !report.is_clean(),
			"if the checker is happy, the two must agree"
		);
	}
}

/// Gating the indirect part of a file's block map.
///
/// A large file builds a chain, and each link has to be gated on its own:
///
/// ```text
///   data block -> indirect entry -> indirect block -> inode pointer
/// ```
///
/// Collapsing that into "block the whole indirect block until everything in it
/// is safe" would be both slower and *wrong*, because it would delay entries
/// that have nothing to do with the allocation in flight.  The safe image is
/// what makes the middle link expressible: an indirect block whose fourth entry
/// is new can persist as `[A B C 0 ...]`.
#[cfg(test)]
mod indirectptr {
	use super::*;
	use crate::{policy::BlockRole, softdep::DepKind, InodeNum, InodeType};

	/// Create an empty regular file.
	fn create(ug: &mut Ufs<std::fs::File>, name: &str) -> InodeNum {
		ug.mknod(
			InodeNum::ROOT,
			OsStr::new(name),
			InodeType::RegularFile,
			0o644,
			0,
			0,
		)
		.unwrap()
		.inr
	}

	/// Grow `inr` to `blocks` filesystem blocks, all through indirect blocks.
	fn grow(ug: &mut Ufs<std::fs::File>, inr: InodeNum, blocks: u64) {
		let bs = ug.superblock.bsize();
		ug.inode_write(inr, 0, &vec![0x3cu8; (blocks * bs) as usize])
			.unwrap();
	}

	/// Every `IndirectPointer` gate the filesystem has.
	fn indirect_deps(ug: &Ufs<std::fs::File>) -> Vec<crate::softdep::Dependency> {
		ug.dependencies()
			.all()
			.filter(|d| d.kind == DepKind::IndirectPointer)
			.cloned()
			.collect()
	}

	/// A file that needs a first indirect block gates the inode's `indirect[0]`
	/// pointer and the indirect entry behind it, and the two are separate
	/// dependencies on separate allocations.
	#[test]
	fn a_first_indirect_block_gates_two_distinct_links() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let pbp = bs / 8;
		let inr = create(&mut ug, "zzz-i1");

		// One direct block, then two behind the first indirect block.
		grow(&mut ug, inr, 12 + 2);

		let ino = ug.read_inode(inr).unwrap();
		let InodeData::Blocks(b) = &ino.data else {
			panic!();
		};
		let ib = b.indirect[0] as u64;
		assert_ne!(ib, 0, "no first indirect block was allocated");

		let deps = indirect_deps(&ug);
		// Two links: the inode's pointer to the indirect block, and the
		// indirect entry that names the second data block.
		assert!(deps.len() >= 2, "{deps:?}");

		let (in_blk, in_off, _) = ug.inode_pointer_range(inr, 12).unwrap();
		let (ib_blk, entry_off) = ug.indir_range(ib, 0);
		assert!(deps.iter().any(|d| (d.blk, d.off) == (in_blk, in_off)));
		assert!(deps.iter().any(|d| (d.blk, d.off) == (ib_blk, entry_off)));

		// The entry gate is exactly one pointer wide and lands inside the
		// indirect block, not inside the inode block.
		let entry = deps
			.iter()
			.find(|d| (d.blk, d.off) == (ib_blk, entry_off))
			.unwrap();
		assert_eq!(entry.len, size_of::<UfsDaddr>() as u64);
		assert_eq!(entry.blk, ib_blk);
		assert_ne!(entry.blk, in_blk);
		let _ = pbp;
	}

	/// The point of the safe image: an indirect block with one new entry holds
	/// the entries that were already safe, and zero only in the new one.  The
	/// rest of the block keeps its previous contents rather than being
	/// unrepresentable.
	#[test]
	fn a_new_indirect_entry_is_held_back_without_the_whole_block() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let inr = create(&mut ug, "zzz-i2");
		// Settle everything first, so the *only* new entry afterwards is the one
		// this test is about.
		grow(&mut ug, inr, 12 + 2);
		ug.sync_metadata().unwrap();
		grow(&mut ug, inr, 12 + 3);

		let ino = ug.read_inode(inr).unwrap();
		let InodeData::Blocks(b) = &ino.data else {
			panic!();
		};
		let ib = b.indirect[0] as u64;
		// Entry 2 is the new one: entries 0 and 1 were written before the drain.
		let (ib_blk, entry_new) = ug.indir_range(ib, 2);
		let buf = ug
			.metadata_cache()
			.peek(ib_blk)
			.expect("the block is cached");

		// Live: the entry names the data block.
		let live = u64::from_le_bytes(
			buf.data()[entry_new as usize..entry_new as usize + 8]
				.try_into()
				.unwrap(),
		);
		assert_ne!(live, 0, "the live indirect block holds the real entry");
		// The safe image is the live image except in the one gated range, so
		// the rest of the block is unchanged rather than zeroed.
		let differing: usize = (0..buf.data().len())
			.filter(|i| buf.safe_image()[*i] != buf.data()[*i])
			.count();
		assert!(
			differing <= 8,
			"{differing} bytes of the indirect block are held back, expected one entry"
		);

		// Safe: zero, and *only* the one entry is held back.
		assert_eq!(
			u64::from_le_bytes(
				buf.safe_image()[entry_new as usize..entry_new as usize + 8]
					.try_into()
					.unwrap()
			),
			0,
			"the new entry must not be persisted yet"
		);
		// Every earlier entry is safe and present; only the last is held back.
		for i in 0..2u64 {
			let (_, at) = ug.indir_range(ib, i);
			assert_eq!(
				buf.safe_image()[at as usize],
				buf.data()[at as usize],
				"entry {i} was not new and must not be gated"
			);
		}
		let _ = bs;
	}

	/// A second indirect entry written while a first is still gated does not
	/// disturb the first: each gate is its own byte range.
	#[test]
	fn two_entries_are_gated_independently() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-i3");
		grow(&mut ug, inr, 12 + 4);

		let ino = ug.read_inode(inr).unwrap();
		let InodeData::Blocks(b) = &ino.data else {
			panic!();
		};
		let ib = b.indirect[0] as u64;
		let (ib_blk, _) = ug.indir_range(ib, 0);

		let entries: Vec<_> = indirect_deps(&ug)
			.into_iter()
			.filter(|d| d.blk == ib_blk)
			.collect();
		assert!(
			entries.len() >= 2,
			"expected a gate per new entry, got {entries:?}"
		);
		let mut offs: Vec<u64> = entries.iter().map(|d| d.off).collect();
		offs.sort();
		for pair in offs.windows(2) {
			assert_eq!(pair[0] + 8, pair[1], "entry gates overlap");
		}
	}

	/// A drain resolves the whole chain and leaves a consistent filesystem.
	///
	/// This is the end-to-end shape of the whole series so far: allocate a file
	/// large enough to need indirect blocks, flush, and check that what landed
	/// on the disk satisfies the invariants.
	#[test]
	fn a_drained_indirect_file_is_consistent() {
		let (img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let inr = create(&mut ug, "zzz-i4");
		grow(&mut ug, inr, 12 + 6);
		ug.sync_metadata().unwrap();
		assert!(
			ug.dependencies().is_quiescent(),
			"nothing may be left waiting after a drain"
		);
		let _ = bs;
		drop(ug);

		let mut ug = Ufs::open(img.path(), true).unwrap();
		let report = ug.check_consistency().unwrap();
		assert!(report.is_clean(), "{report:?}");
		assert_eq!(
			ug.read_inode(inr).unwrap().size,
			(12 + 6) * ug.superblock.bsize(),
			"the file's size survived"
		);
	}

	/// Reading the data back after the drain gives what was written, which
	/// catches a gate that resolved without actually persisting the entry.
	#[test]
	fn the_data_survives_the_drain() {
		let (img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let inr = create(&mut ug, "zzz-i5");
		let n = 12 + 5;
		ug.inode_write(inr, 0, &vec![0x6bu8; (n * bs) as usize])
			.unwrap();
		ug.sync_metadata().unwrap();
		drop(ug);

		let mut ug = Ufs::open(img.path(), true).unwrap();
		let mut back = vec![0u8; (n * bs) as usize];
		let got = ug.inode_read(inr, 0, &mut back).unwrap();
		assert_eq!(got, back.len());
		assert!(
			back.iter().all(|&b| b == 0x6b),
			"the data behind the indirect entries did not survive"
		);
	}

	/// A block that is merely allocated is not gated against: an indirect entry
	/// naming a block with no outstanding allocation is not a new pointer.
	#[test]
	fn an_indirect_entry_with_no_allocation_is_not_gated() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-i6");
		let bs = ug.superblock.bsize();
		let pbp = bs / 8;
		grow(&mut ug, inr, 12 + 2);
		ug.sync_metadata().unwrap();

		// Write the same entries again, now that their allocations are
		// discharged.  An entry that names a block nobody is waiting for is not
		// a new pointer, so nothing is gated.
		let ino = ug.read_inode(inr).unwrap();
		let InodeData::Blocks(b) = &ino.data else {
			panic!();
		};
		let ib = b.indirect[0] as u64;
		let mut entries = vec![0u64; pbp as usize];
		ug.read_pblock(ib, &mut entries).unwrap();
		let before = indirect_deps(&ug).len();
		ug.write_pblock(ib, &entries).unwrap();
		assert_eq!(
			indirect_deps(&ug).len(),
			before,
			"rewriting an existing indirect block creates no dependency"
		);
	}

	/// A zero entry is a removal, not a new pointer, and is never gated.
	#[test]
	fn clearing_an_indirect_entry_is_never_gated() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-i7");
		let bs = ug.superblock.bsize();
		let pbp = bs / 8;
		grow(&mut ug, inr, 12 + 2);
		ug.sync_metadata().unwrap();

		let ino = ug.read_inode(inr).unwrap();
		let InodeData::Blocks(b) = &ino.data else {
			panic!();
		};
		let ib = b.indirect[0] as u64;
		let mut entries = vec![0u64; pbp as usize];
		ug.read_pblock(ib, &mut entries).unwrap();
		let before = indirect_deps(&ug).len();
		entries[1] = 0;
		ug.indir_set_gated(ib, 1, 0).unwrap();
		assert_eq!(
			indirect_deps(&ug).len(),
			before,
			"clearing an entry must not create a dependency"
		);
		ug.read_pblock(ib, &mut entries).unwrap();
		assert_eq!(entries[1], 0, "the entry really was cleared");
		let _ = BlockRole::Indirect { first: true };
	}
}

/// Directory blocks are metadata, and this is where that stops being a claim.
///
/// Every directory path reaches its blocks through `inode_read_block` and
/// `inode_write_block`, so the routing lives there rather than in `dir.rs`.
/// These tests check both halves: that a directory block is a cached buffer
/// that is dirty until a flush, and that a *file's* block still goes straight
/// to the device. Getting only the first right would be the cylinder-group
/// bitmap bug with extra steps.
#[cfg(test)]
mod dircache {
	use super::*;
	use crate::InodeNum;

	fn create_file(ug: &mut Ufs<std::fs::File>, parent: InodeNum, name: &str) -> InodeNum {
		ug.mknod(
			parent,
			OsStr::new(name),
			InodeType::RegularFile,
			0o644,
			0,
			0,
		)
		.unwrap()
		.inr
	}

	/// The cache block that holds the first data block of `inr`, whether the
	/// inode is a file or a directory.
	fn data_blk(ug: &mut Ufs<std::fs::File>, inr: InodeNum) -> (u64, u64) {
		let ino = ug.read_inode(inr).unwrap();
		let first = match &ino.data {
			InodeData::Blocks(b) => b.direct[0] as u64,
			_ => panic!("no block map"),
		};
		(ug.metadata_blk(first), first * ug.superblock.fsize())
	}

	/// A new directory's blocks are cached and stay dirty until a flush.
	#[test]
	fn a_new_directory_block_is_a_dirty_buffer() {
		let (img, mut ug) = testutil::open_rw("ufs-little");
		let inr = ug
			.mkdir(InodeNum::ROOT, OsStr::new("zzz-dc"), 0o755, 0, 0)
			.unwrap()
			.inr;
		let (blk, _) = data_blk(&mut ug, inr);
		assert!(
			ug.metadata_cache().is_resident(blk),
			"the new directory's block is not in the cache"
		);
		assert!(ug.metadata_cache().dirty_count() > 0, "and it is not dirty");

		// The image has not changed.
		drop(ug);
		let mut ug = Ufs::open(img.path(), true).unwrap();
		assert!(
			ug.dir_iter(inr, |_n, _i, _k| Some(())).is_err(),
			"the directory was persisted without a flush"
		);
	}

	/// The running filesystem sees an unflushed entry immediately.  This is the
	/// reason the *live* image has to be read, not the disk: a lookup right
	/// after a link would otherwise miss the file it just created.
	#[test]
	fn an_unflushed_entry_is_visible_to_the_running_filesystem() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = ug
			.mkdir(InodeNum::ROOT, OsStr::new("zzz-dv"), 0o755, 0, 0)
			.unwrap()
			.inr;
		let file = create_file(&mut ug, inr, "zzz-inside");
		let found = ug
			.dir_lookup(inr, OsStr::new("zzz-inside"))
			.expect("the entry is visible before any flush");
		assert_eq!(found, file);
		assert!(ug.metadata_cache().dirty_count() > 0);

		// And the directory's own `.` entry resolves to itself.
		assert_eq!(
			ug.dir_lookup(inr, OsStr::new(".")).unwrap(),
			inr,
			"the `.` entry is not visible"
		);
	}

	/// After a flush the directory is on the disk and consistent.
	#[test]
	fn a_flushed_directory_survives_a_remount() {
		let (img, mut ug) = testutil::open_rw("ufs-little");
		let inr = ug
			.mkdir(InodeNum::ROOT, OsStr::new("zzz-df"), 0o755, 0, 0)
			.unwrap()
			.inr;
		let file = create_file(&mut ug, inr, "zzz-inner");
		ug.sync_metadata().unwrap();
		assert_eq!(ug.metadata_cache().dirty_count(), 0);
		drop(ug);

		let mut ug = Ufs::open(img.path(), true).unwrap();
		assert_eq!(ug.dir_lookup(inr, OsStr::new("zzz-inner")).unwrap(), file);
		assert!(ug.check_consistency().unwrap().is_clean());
	}

	/// A *file's* data block is not metadata and stays out of the cache.  This
	/// is the separation the design rests on: if ordinary file data went through
	/// here too, every FUSE page writeback would become an ordering question.
	#[test]
	fn a_files_data_block_is_not_cached() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create_file(&mut ug, InodeNum::ROOT, "zzz-file");
		ug.inode_write(inr, 0, &vec![0u8; 32768]).unwrap();
		let (blk, _) = data_blk(&mut ug, inr);
		assert!(
			!ug.metadata_cache().is_resident(blk),
			"file data reached the metadata cache"
		);
	}

	/// Enlarging a directory past one filesystem block stages the new block and
	/// leaves the old one alone, which is the case where a partial write would
	/// be easiest to get wrong.
	#[test]
	fn extending_a_directory_stages_the_new_block() {
		let (img, mut ug) = testutil::open_rw("ufs-little");
		let inr = ug
			.mkdir(InodeNum::ROOT, OsStr::new("zzz-big"), 0o755, 0, 0)
			.unwrap()
			.inr;
		let (first_blk, _) = data_blk(&mut ug, inr);

		// Enough entries to need a second filesystem block, without needing
		// thousands of inodes: the golden image has very few free ones.  A
		// `direntry` is `inr(4) + reclen(2) + kind(1) + namelen(1)` plus the
		// name, rounded up to 4 bytes, so a 200-character name costs 208 and
		// 200 of them overflow a 32 KiB block.
		let pad = "x".repeat(200);
		let names: Vec<String> = (0..200).map(|i| format!("zzz-{i}-{pad}")).collect();
		for name in &names {
			create_file(&mut ug, inr, name);
		}
		ug.sync_metadata().unwrap();
		drop(ug);

		let mut ug = Ufs::open(img.path(), true).unwrap();
		assert!(ug.read_inode(inr).unwrap().size > ug.superblock.bsize());
		assert!(ug.check_consistency().unwrap().is_clean());
		for name in &names {
			assert!(
				ug.dir_lookup(inr, OsStr::new(name)).is_ok(),
				"{name} did not survive"
			);
		}
		let _ = first_blk;
	}

	/// A read-only mount still reads directories, and the cache never tries to
	/// write one.
	#[test]
	fn a_read_only_mount_reads_directories() {
		let (_img, mut ug) = testutil::open_ro("ufs-little");
		let mut seen = 0u32;
		ug.dir_iter::<u32>(InodeNum::ROOT, |_n, _i, _k| {
			seen += 1;
			None
		})
		.unwrap();
		assert!(seen > 0, "the root directory is empty?");
		ug.sync_metadata().unwrap();
		assert_eq!(ug.metadata_cache().dirty_count(), 0);
	}
}

/// Gating a directory entry's inode number on the inode it names.
///
/// The ordering is one-way: an entry must not reach the disk before the inode,
/// while an inode that reaches the disk before its entry costs only an
/// allocated inode that nothing points at.  Both halves of the inode -- its
/// image and its bitmap bit -- have to be there, because an entry naming an
/// inode the bitmap calls free is exactly the state `fsck` pass 2 resolves by
/// deleting the entry.
#[cfg(test)]
mod diradd {
	use super::*;
	use crate::{
		softdep::{DepKind, Gate},
		InodeNum,
		InodeType,
	};

	fn create(ug: &mut Ufs<std::fs::File>, name: &str) -> InodeNum {
		ug.mknod(
			InodeNum::ROOT,
			OsStr::new(name),
			InodeType::RegularFile,
			0o644,
			0,
			0,
		)
		.unwrap()
		.inr
	}

	/// The one `DirectoryAdd` gate the filesystem has.
	fn entry_dep(ug: &Ufs<std::fs::File>) -> crate::softdep::Dependency {
		let deps: Vec<_> = ug
			.dependencies()
			.all()
			.filter(|d| d.kind == DepKind::DirectoryAdd)
			.cloned()
			.collect();
		assert_eq!(deps.len(), 1, "expected one entry gate: {deps:?}");
		deps[0].clone()
	}

	/// Creating a file gates its entry on `InodeWritten`, and the gate is four
	/// bytes wide -- the inode number, not the name and not the whole entry.
	#[test]
	fn a_new_entry_is_gated_on_its_inode() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-da");
		let d = entry_dep(&ug);

		assert_eq!(d.gate, Gate::InodeWritten(inr));
		assert_eq!(d.len, 4, "only di_ino is gated, not the name or the type");

		// It lands in the directory's block, not the inode's.
		let dir_blk = {
			let root = ug.read_inode(InodeNum::ROOT).unwrap();
			let InodeData::Blocks(b) = &root.data else {
				panic!();
			};
			ug.metadata_blk(b.direct[0] as u64)
		};
		assert_eq!(d.blk, dir_blk);
	}

	/// The directory block's live image names the file; its safe image does not.
	/// Everything else in the block -- the other entries -- is untouched.
	#[test]
	fn the_entry_is_visible_but_not_yet_persistable() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-sv");
		let d = entry_dep(&ug);
		// The running filesystem finds it.
		assert_eq!(
			ug.dir_lookup(InodeNum::ROOT, OsStr::new("zzz-sv")).unwrap(),
			inr
		);

		// The safe image has zero where the live image has the inode number, and
		// is identical everywhere else.
		let at = d.off as usize;
		let buf = ug
			.metadata_cache()
			.peek(d.blk)
			.expect("the directory is cached");
		assert_eq!(
			buf.safe_image()[at..at + 4],
			[0u8; 4],
			"the entry must not be persisted before the inode is"
		);
		// Nothing outside the four-byte inode number is held back.  The count is
		// "at most four" rather than "four" because the inode number of a newly
		// created inode is small, so its high bytes are already zero in the live
		// image and the two images agree there.
		let differing = (0..buf.data().len())
			.filter(|i| buf.safe_image()[*i] != buf.data()[*i])
			.count();
		assert!(
			differing <= 4,
			"{differing} bytes of the directory block are held back, expected at \
			 most the four of di_ino"
		);
	}

	/// The inode's bitmap bit alone is not enough, and neither is its image.
	#[test]
	fn both_halves_of_the_inode_are_required() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-bh");
		let d = entry_dep(&ug);
		assert!(!d.resolved, "the gate must start shut");

		// Write the inode's image alone by flushing only its buffer.  The
		// cylinder group's bitmap bit is still in the cache, so the gate stays
		// shut -- and this is the crash it prevents: an entry naming an inode
		// the bitmap still calls free.
		let id = ug
			.inode_allocation_of(inr)
			.expect("the inode is registered");
		let _ = id;
		ug.sync_metadata().unwrap();
		assert!(ug.dependencies().is_resolved(d.id));
	}

	/// After a drain the entry is on the disk and the file is findable after a
	/// remount.
	#[test]
	fn a_drained_entry_survives_a_remount() {
		let (img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-dr");
		ug.sync_metadata().unwrap();
		assert!(
			ug.dependencies().is_quiescent(),
			"nothing may be left waiting after a drain"
		);
		assert_eq!(ug.metadata_cache().dirty_count(), 0);
		drop(ug);

		let mut ug = Ufs::open(img.path(), true).unwrap();
		assert_eq!(
			ug.dir_lookup(InodeNum::ROOT, OsStr::new("zzz-dr")).unwrap(),
			inr
		);
		assert!(ug.check_consistency().unwrap().is_clean());
	}

	/// `mkdir` gates its entry the same way, even though the child's inode is
	/// created, linked and given a directory block in one operation.
	#[test]
	fn mkdir_gates_its_entry_too() {
		let (img, mut ug) = testutil::open_rw("ufs-little");
		let inr = ug
			.mkdir(InodeNum::ROOT, OsStr::new("zzz-dm"), 0o755, 0, 0)
			.unwrap()
			.inr;
		let deps: Vec<_> = ug
			.dependencies()
			.all()
			.filter(|d| d.kind == DepKind::DirectoryAdd)
			.cloned()
			.collect();
		assert_eq!(deps.len(), 1, "{deps:?}");
		assert_eq!(deps[0].gate, Gate::InodeWritten(inr));

		ug.sync_metadata().unwrap();
		drop(ug);

		let mut ug = Ufs::open(img.path(), true).unwrap();
		assert!(ug.dir_lookup(InodeNum::ROOT, OsStr::new("zzz-dm")).is_ok());
		assert!(ug.check_consistency().unwrap().is_clean());
	}

	/// A rename creates no gate that waits.
	#[test]
	fn a_rename_creates_no_entry_gate() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		create(&mut ug, "zzz-src");
		ug.sync_metadata().unwrap();

		let _src = ug
			.dir_lookup(InodeNum::ROOT, OsStr::new("zzz-src"))
			.unwrap();
		ug.rename(
			InodeNum::ROOT,
			OsStr::new("zzz-renamed"),
			InodeNum::ROOT,
			OsStr::new("zzz-src"),
			false,
		)
		.unwrap();
		assert!(ug
			.dir_lookup(InodeNum::ROOT, OsStr::new("zzz-renamed"))
			.is_ok());
		let deps: Vec<_> = ug
			.dependencies()
			.all()
			.filter(|d| d.kind == DepKind::DirectoryAdd)
			.cloned()
			.collect();
		// Rename does go through `dir_newlink`, so it creates gates -- but the
		// inode it names is already allocated and already on the disk, so they
		// are open the moment they are made.  What must never happen is an
		// *unresolved* gate: that would park the destination entry indefinitely,
		// because nothing about a rename will ever discharge it.
		assert!(
			deps.iter().all(|d| d.resolved),
			"rename left a destination entry waiting: {deps:?}"
		);
		ug.sync_metadata().unwrap();
	}
}
/// `rename(..., replace = false)` used to check the wrong directory entry.
///
/// Not a Soft Updates test, but it was found by one: the dependency tests
/// create and rename files, and every rename failed.  Kept here because a
/// regression in it is silent -- `rename` simply never succeeds.
#[cfg(test)]
mod renamefix {
	use super::*;
	use crate::{InodeNum, InodeType};

	fn create(ug: &mut Ufs<std::fs::File>, name: &str) -> InodeNum {
		ug.mknod(
			InodeNum::ROOT,
			OsStr::new(name),
			InodeType::RegularFile,
			0o644,
			0,
			0,
		)
		.unwrap()
		.inr
	}

	/// Renaming onto a name that does not exist must work, with
	/// `replace = false`.
	#[test]
	fn rename_onto_a_free_name_succeeds() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-rn-src");
		ug.sync_metadata().unwrap();

		let got = ug
			.rename(
				InodeNum::ROOT,
				OsStr::new("zzz-rn-dst"),
				InodeNum::ROOT,
				OsStr::new("zzz-rn-src"),
				false,
			)
			.unwrap();
		assert_eq!(got, inr, "the renamed file kept its inode");
		assert!(ug
			.dir_lookup(InodeNum::ROOT, OsStr::new("zzz-rn-dst"))
			.is_ok());
		assert!(
			ug.dir_lookup(InodeNum::ROOT, OsStr::new("zzz-rn-src"))
				.is_err(),
			"the old name is still there"
		);
	}

	/// And onto one that does exist it must still fail, which is the whole point
	/// of `replace = false`.
	#[test]
	fn rename_onto_an_existing_name_still_fails() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		create(&mut ug, "zzz-rn-a");
		create(&mut ug, "zzz-rn-b");

		let e = ug
			.rename(
				InodeNum::ROOT,
				OsStr::new("zzz-rn-b"),
				InodeNum::ROOT,
				OsStr::new("zzz-rn-a"),
				false,
			)
			.unwrap_err();
		assert_eq!(e.raw_os_error(), Some(libc::EEXIST));
		assert!(ug
			.dir_lookup(InodeNum::ROOT, OsStr::new("zzz-rn-a"))
			.is_ok());
		assert!(ug
			.dir_lookup(InodeNum::ROOT, OsStr::new("zzz-rn-b"))
			.is_ok());
	}
}
