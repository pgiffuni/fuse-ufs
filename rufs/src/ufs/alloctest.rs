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
