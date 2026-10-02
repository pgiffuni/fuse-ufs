use std::io::{BufRead, Write};

use super::*;
use crate::{err, InodeNum};

/// The size of `direntry::di_ino`, the inode number a directory entry names.
///
/// `direntry` is `{ u_int32_t di_ino; u_int16_t di_reclen; u_int8_t di_type;
/// u_int8_t di_namlen; char di_name[]; }`, so the number is 4 bytes even though
/// UFS block addresses are `ufs2_daddr_t` and therefore 8.  Gating eight bytes
/// here would also zero `di_reclen`, and the rest of the record would then be
/// unparseable -- a crash would turn a half-written entry into a directory that
/// `fsck` cannot walk past.
const DIRENT_INR_LEN: u64 = 4;

#[derive(Debug, Clone, Copy)]
struct Header {
	inr:     InodeNum,
	reclen:  u16,
	kind:    Option<InodeType>,
	namelen: u8,
	name:    [u8; UFS_MAXNAMELEN + 1],
}

impl Header {
	fn new(inr: InodeNum, kind: InodeType, dname: &OsStr) -> Self {
		assert!(dname.len() <= UFS_MAXNAMELEN);
		let mut name = [0u8; UFS_MAXNAMELEN + 1];
		name[0..dname.len()].copy_from_slice(dname.as_bytes());
		let reclen = ((4 + 2 + 1 + 1 + name.len() + 3) & !3) as u16;
		Self {
			inr,
			reclen,
			kind: Some(kind),
			name,
			namelen: dname.len() as u8,
		}
	}

	fn parse<T: BufRead + Seek>(file: &mut Decoder<T>) -> IoResult<Option<Header>> {
		let inr: InodeNum = file.decode()?;
		let reclen: u16 = file.decode()?;
		if reclen == 0 {
			return Ok(None);
		}
		let kind: u8 = file.decode()?;
		let namelen: u8 = file.decode()?;
		let mut name = [0u8; UFS_MAXNAMELEN + 1];
		file.read(&mut name[0..namelen.into()])?;

		// skip remaining bytes of record, if any
		let off = reclen - (namelen as u16) - 8;
		file.seek_relative(off as i64)?;

		if inr.get() == 0 {
			return Ok(None);
		}

		log::trace!("Header::read(): {{ inr={inr}, reclen={reclen}, namelen={namelen}, name={:?}, kind={kind} }}",
					unsafe { OsStr::from_encoded_bytes_unchecked(&name[0..namelen.into()]) });

		let kind = match kind {
			DT_FIFO => Some(InodeType::NamedPipe),
			DT_CHR => Some(InodeType::CharDevice),
			DT_DIR => Some(InodeType::Directory),
			DT_BLK => Some(InodeType::BlockDevice),
			DT_REG => Some(InodeType::RegularFile),
			DT_LNK => Some(InodeType::Symlink),
			DT_SOCK => Some(InodeType::Socket),
			DT_WHT => None,
			DT_UNKNOWN => todo!("DT_UNKNOWN: {inr}"),
			_ => panic!("invalid filetype: {kind}"),
		};

		Ok(Some(Self {
			inr,
			reclen,
			kind,
			namelen,
			name,
		}))
	}

	fn write<T: Read + Write + Seek>(&self, file: &mut Decoder<T>) -> IoResult<()> {
		let kind: u8 = match self.kind {
			Some(InodeType::NamedPipe) => DT_FIFO,
			Some(InodeType::CharDevice) => DT_CHR,
			Some(InodeType::Directory) => DT_DIR,
			Some(InodeType::BlockDevice) => DT_BLK,
			Some(InodeType::RegularFile) => DT_REG,
			Some(InodeType::Symlink) => DT_LNK,
			Some(InodeType::Socket) => DT_SOCK,
			None => DT_WHT,
		};
		log::trace!(
			"Header::write(inr={}, reclen={}, namelen={}, name={:?}, kind={kind})",
			self.inr,
			self.reclen,
			self.namelen,
			self.name()
		);
		file.encode(&self.inr)?;
		file.encode(&self.reclen)?;
		file.encode(&kind)?;
		file.encode(&self.namelen)?;
		file.write(&self.name[0..self.namelen.into()])?;

		// fill the rest of the record with zeros
		file.fill(0u8, (self.reclen - (self.namelen as u16) - 8).into())?;

		Ok(())
	}

	fn name(&self) -> &OsStr {
		unsafe { OsStr::from_encoded_bytes_unchecked(&self.name[0..self.namelen.into()]) }
	}

	fn minlen(&self) -> u16 {
		(4 + 2 + 1 + 1 + (self.namelen as u16) + 3) & !3
	}
}

fn readdir_block<T>(
	inr: InodeNum,
	block: &[u8],
	config: Config,
	mut f: impl FnMut(&OsStr, InodeNum, InodeType) -> Option<T>,
) -> IoResult<Option<T>> {
	let mut file = Decoder::new(Cursor::new(block), config);

	while let Ok(Some(hdr)) = Header::parse(&mut file) {
		if hdr.inr.get() == 0 {
			break;
		}

		let Some(kind) = hdr.kind else {
			log::warn!(
				"readdir_block({inr}): encountered a whiteout entry: {:?}",
				hdr.name()
			);
			continue;
		};

		let res = f(hdr.name(), hdr.inr, kind);
		if res.is_some() {
			return Ok(res);
		}
	}

	Ok(None)
}

/// Where `newlink_block` put a new entry.
///
/// The offset is needed because a Soft Updates gate is a byte range, and the
/// range that matters is the entry's inode number -- four bytes -- rather than
/// the whole entry.  Gating the name and the type would delay them for no
/// reason; gating nothing would let the entry persist before the inode it
/// names.
fn newlink_block(block: &mut [u8], mut entry: Header, config: Config) -> IoResult<Option<u64>> {
	let mut file = Decoder::new(Cursor::new(block), config);

	loop {
		let pos = file.pos()?;
		let Ok(Some(mut hdr)) = Header::parse(&mut file) else {
			break;
		};
		let minlen = hdr.minlen();
		let rem = hdr.reclen - minlen;
		if rem < entry.minlen() {
			continue;
		}

		hdr.reclen = minlen;
		entry.reclen = rem;

		file.seek(pos)?;
		hdr.write(&mut file)?;
		entry.write(&mut file)?;
		return Ok(Some(pos));
	}

	Ok(None)
}

/// Remove the entry naming `name` from a directory block.
///
/// Returns the inode it named, whether the record could be reclaimed (`has`),
/// and the offset the removed record occupied.  The offset is what
/// `dir_unlink()` needs to tell the dependency engine *where* the entry was, and
/// it cannot be found afterwards: a removed record parses as end-of-directory,
/// so a second scan would stop at it and never reach the one being asked about.
fn unlink_block(
	dinr: InodeNum,
	block: &mut [u8],
	name: &OsStr,
	config: Config,
) -> IoResult<Option<(InodeNum, bool, u64)>> {
	let mut file = Decoder::new(Cursor::new(block), config);
	let mut prevpos = 0;

	loop {
		let pos = file.pos()?;
		let Ok(Some(hdr)) = Header::parse(&mut file) else {
			break;
		};

		if hdr.name() != name {
			prevpos = pos;
			continue;
		}

		log::trace!("unlink_block({dinr}, {name:?}): pos={pos}, hdr={hdr:?}");

		let has;
		if pos == 0 {
			match Header::parse(&mut file) {
				Ok(Some(next)) => {
					log::trace!("unlink_block({dinr}, {name:?}): next={next:?}");
					let new = Header {
						reclen: hdr.reclen + next.reclen,
						..next
					};
					file.seek(pos)?;
					new.write(&mut file)?;
					has = true;
				}
				_ => {
					log::trace!("unlink_block({dinr}, {name:?}): no next");
					has = false;
				}
			}
		} else {
			file.seek(prevpos)?;
			match Header::parse(&mut file)? {
				Some(mut prev) => {
					prev.reclen += hdr.reclen;
					file.seek(prevpos)?;
					prev.write(&mut file)?;
					has = true;
				}
				None => {
					log::error!(
						"unlink_block({dinr}): previous entry is bad: prevpos={prevpos}, pos={pos}"
					);
					return Err(err!(EIO));
				}
			}
		}
		return Ok(Some((hdr.inr, has, pos)));
	}

	Ok(None)
}

fn newdir(dinr: InodeNum, inr: InodeNum, config: Config) -> IoResult<[u8; DIRBLKSIZE]> {
	let mut block = [0u8; DIRBLKSIZE];
	let mut file = Decoder::new(Cursor::new(&mut block as &mut [u8]), config);

	let h_self = Header::new(inr, InodeType::Directory, OsStr::new("."));
	let mut h_parent = Header::new(dinr, InodeType::Directory, OsStr::new(".."));
	h_parent.reclen = (DIRBLKSIZE as u16) - h_self.reclen;
	h_self.write(&mut file)?;
	h_parent.write(&mut file)?;

	Ok(block)
}

impl<R: Backend> Ufs<R> {
	/// Find a file named `name` in the directory referenced by `pinr`.
	pub fn dir_lookup(&mut self, pinr: InodeNum, name: &OsStr) -> IoResult<InodeNum> {
		log::trace!("dir_lookup({pinr}, {name:?});");
		self.dir_iter(
			pinr,
			|name2, inr, _kind| {
				if name == name2 {
					Some(inr)
				} else {
					None
				}
			},
		)?
		.ok_or(err!(ENOENT))
	}

	/// Iterate through a directory referenced by `inr`, and call `f` for each entry.
	pub fn dir_iter<T>(
		&mut self,
		inr: InodeNum,
		mut f: impl FnMut(&OsStr, InodeNum, InodeType) -> Option<T>,
	) -> IoResult<Option<T>> {
		let ino = self.read_inode(inr)?;
		ino.assert_dir()?;
		let mut block = [0u8; DIRBLKSIZE];
		let mut pos = 0;
		while pos < ino.size {
			let n = self.inode_read(inr, pos, &mut block)?;
			assert_eq!(n, DIRBLKSIZE);
			if let Some(x) = readdir_block(inr, &block, self.file.config(), &mut f)? {
				return Ok(Some(x));
			}

			pos += DIRBLKSIZE as u64;
		}
		Ok(None)
	}

	/// Remove a directory entry.
	///
	/// Returns the inode it named and the cache block it was removed from.  The
	/// block matters to `rmdir`: the parent's link count is not allowed to reach
	/// the disk before the removal that justifies it does, and that is a
	/// statement about one specific block.
	pub(super) fn dir_unlink(&mut self, dinr: InodeNum, name: &OsStr) -> IoResult<(InodeNum, u64)> {
		log::trace!("dir_unlink({dinr}, {name:?});");
		self.assert_rw()?;
		let dino = self.read_inode(dinr)?;
		dino.assert_dir()?;

		let mut block = vec![0u8; DIRBLKSIZE];
		let mut pos = 0;
		while pos < dino.size {
			let n = self.inode_read(dinr, pos, &mut block)?;
			assert_eq!(n, DIRBLKSIZE);

			if let Some((inr, has, at)) = unlink_block(dinr, &mut block, name, self.file.config())?
			{
				let blk = self.dirent_block(dinr, pos)?.ok_or_else(|| err!(EIO))?;
				if has {
					// The entry's inode number is already zero in `block`.  Record
					// where it was, so the inode's reclamation can wait for the
					// disk to agree the entry is gone.
					self.inode_write(dinr, pos, &block)?;
					self.note_dirent_removed(dinr, pos, at, inr)?;
				} else {
					let n =
						self.inode_copy_range(dinr, &dino, (pos + DIRBLKSIZE as u64).., pos..)?;
					assert_eq!(n, dino.size - pos - DIRBLKSIZE as u64);
					self.inode_truncate(dinr, dino.size - DIRBLKSIZE as u64)?;
				}
				return Ok((inr, blk));
			}

			pos += DIRBLKSIZE as u64;
		}

		Err(err!(ENOENT))
	}

	pub(super) fn dir_newlink(
		&mut self,
		dinr: InodeNum,
		inr: InodeNum,
		name: &OsStr,
		kind: InodeType,
	) -> IoResult<()> {
		log::trace!("dir_newlink({dinr}, {inr}, {name:?}, {kind:?});");
		self.assert_rw()?;
		let dino = self.read_inode(dinr)?;
		dino.assert_dir()?;

		let mut entry = Header::new(inr, kind, name);

		let mut block = [0u8; DIRBLKSIZE];
		let mut pos = 0;
		while pos < dino.size {
			let n = self.inode_read(dinr, pos, &mut block)?;
			assert_eq!(n, DIRBLKSIZE);

			if let Some(at) = newlink_block(&mut block, entry, self.file.config())? {
				self.inode_write(dinr, pos, &block)?;
				self.gate_dirent(dinr, pos, at, inr)?;
				return Ok(());
			}

			pos += DIRBLKSIZE as u64;
		}

		log::trace!("dir_link({dinr}, {inr}, {name:?}, {kind:?}): extending directory for new entry: {entry:?}");
		self.inode_truncate(dinr, dino.size + DIRBLKSIZE as u64)?;
		entry.reclen = DIRBLKSIZE as u16;
		entry.write(&mut Decoder::new(
			Cursor::new(&mut block as &mut [u8]),
			self.file.config(),
		))?;
		self.inode_write(dinr, pos, &block)?;
		// A fresh block starts with the entry at offset zero.
		self.gate_dirent(dinr, pos, 0, inr)?;
		Ok(())
	}

	/// Hold back the inode number of a directory entry until `inr` is on the
	/// disk.
	///
	/// Created by [`Self::dir_newlink`].  Protects invariants 5 and 6 in
	/// `docs/ufs2-invariants.md`.
	///
	/// # The ordering
	///
	/// A directory entry must not reach the disk before the inode it names is
	/// there.  The reverse order -- inode first, then the entry -- is fine: a
	/// directory entry that has not been created yet costs an allocated inode
	/// that nothing points at, which `fsck` pass 2 reclaims harmlessly.  The
	/// other order is the disaster: a crash leaves a directory entry naming an
	/// inode whose bitmap bit is clear and whose contents are zeroes, so the file
	/// appears to exist, reads back as empty, and `fsck` pass 2 removes the
	/// entry -- discarding a directory whose data was never lost, only
	/// unreachable.
	///
	/// # Why four bytes
	///
	/// Only `direntry::inr` is gated, not the name or the type.  The name and
	/// the type are consequences of the inode, not references to it, and
	/// holding them back would delay a lookup finding a file that is already
	/// perfectly readable.  The whole entry is not gated for the same reason:
	/// the block it lives in usually holds other entries that have nothing to
	/// do with this allocation, and the safe image exists precisely so that
	/// they need not be delayed.
	///
	/// # What resolves it
	///
	/// The inode's own write reaching the device.  There is no "inode safe"
	/// event to reuse -- an inode's block map is a pointer, and *that* is gated
	/// on the blocks it names -- so the event is signalled from
	/// `sync_metadata()` once the inode's block has actually been written.
	fn gate_dirent(
		&mut self,
		dinr: InodeNum,
		file_pos: u64,
		block_off: u64,
		inr: InodeNum,
	) -> IoResult<()> {
		let Some(blk) = self.dirent_block(dinr, file_pos)? else {
			// The block was freed underneath us; there is nothing to gate and
			// nothing that can reach the entry.
			return Ok(());
		};
		let off = block_off;
		log::trace!("gating {dinr}'s entry for {inr} at {blk}:{off}+{DIRENT_INR_LEN}");
		self.softdep.gate(
			&mut self.buf,
			crate::softdep::DepKind::DirectoryAdd,
			blk,
			off,
			DIRENT_INR_LEN,
			crate::softdep::Gate::InodeWritten(inr),
		)?;
		Ok(())
	}

	/// The cache block holding the directory block at file offset `pos` of
	/// `dinr`.
	pub(super) fn dirent_block(&mut self, dinr: InodeNum, pos: u64) -> IoResult<Option<u64>> {
		let ino = self.read_inode(dinr)?;
		let Some(info) = self.inode_locate(dinr, &ino, pos) else {
			return Ok(None);
		};
		Ok(self
			.inode_resolve_block(dinr, &ino, info.blkidx)?
			.map(|b| self.metadata_blk(b.get())))
	}

	pub fn unlink(&mut self, dinr: InodeNum, name: &OsStr) -> IoResult<()> {
		log::trace!("unlink({dinr}, {name:?});");
		self.assert_rw()?;
		let inr = self.dir_lookup(dinr, name)?;

		if self.read_inode(inr)?.kind() == InodeType::Directory {
			// POSIX: `unlink` on a directory is `EISDIR` on Linux and `EPERM` on
			// the BSDs; `rmdir` is the operation for it.  Allowing it here did
			// not corrupt anything on its own -- the directory keeps its `.` and
			// `..` and merely ends up with a link count of one -- but it left the
			// filesystem reporting a link count that no longer matched anything
			// that could reach the directory, which the randomised test found as a
			// directory with no `.` at all.
			log::warn!("unlink({dinr}, {name:?}): {inr} is a directory; use rmdir");
			return Err(err!(EISDIR));
		}

		self.unlink_entry(dinr, name).map(|_| ())
	}

	/// Remove a directory entry and drop a link from whatever it named, with no
	/// restriction on what that is.
	///
	/// This is the primitive behind [`Self::unlink`], and it is also what
	/// `rmdir` uses for a directory's own `.` and `..` -- entries that *name*
	/// directories, which the public `unlink` correctly refuses.  Refusing them
	/// there is what stopped `rmdir` working at all.
	fn unlink_entry(&mut self, dinr: InodeNum, name: &OsStr) -> IoResult<()> {
		let (inr, _blk) = self.dir_unlink(dinr, name)?;
		self.inode_free(inr)?;
		Ok(())
	}

	pub fn rename(
		&mut self,
		d_dinr: InodeNum,
		d_name: &OsStr,
		s_dinr: InodeNum,
		s_name: &OsStr,
		replace: bool,
	) -> IoResult<InodeNum> {
		log::trace!("rename({d_dinr}, {d_name:?} {s_dinr}, {s_name:?}, {replace});");
		self.assert_rw()?;

		let inr = self.dir_lookup(s_dinr, s_name)?;

		if !replace {
			// The user has requested an error if the destination already exists.
			//
			// The *destination*: this used to look the *source* up, which is a
			// tautology -- `dir_lookup(s_dinr, s_name)` above already succeeded,
			// or `rename` would not have got this far -- so `replace = false`
			// failed with EEXIST for every rename, including onto a name that did
			// not exist.  The `replace` branch below got it right.
			match self.dir_lookup(d_dinr, d_name) {
				Ok(_) => return Err(err!(EEXIST)),
				// TODO: Need raw OS error here?
				Err(e) if e.kind() == ErrorKind::NotFound => {}
				// TODO: Might want to handle not a directory etc specially.
				Err(e) => return Err(e),
			}
		} else {
			// Not sure whether the kernel ensures for us that the file no longer exists,
			// if this triggers we know we guessed wrong.
			match self.unlink(d_dinr, d_name) {
				    Ok(_) => log::warn!("rename({d_dinr}, {d_name:?} {s_dinr}, {s_name:?}, {replace}): Destination already exists, unlinked"),
					// TODO: Need raw OS error here?
					Err(e) if e.kind() == ErrorKind::NotFound => { },
					// TODO: Handle not a directory etc.
					Err(e) => return Err(e),
				}
		}

		let kind = self.inode_attr(inr)?.kind;
		// unlink decrements the refcount, if it reaches 0 the file may get removed.
		// Bump the counter before to workaround.
		self.inode_bump(inr)?;
		self.dir_newlink(d_dinr, inr, d_name, kind)?;

		self.unlink(s_dinr, s_name)?;
		Ok(inr)
	}

	pub fn rmdir(&mut self, dinr: InodeNum, name: &OsStr) -> IoResult<()> {
		self.assert_rw()?;
		let inr = self.dir_lookup(dinr, name)?;
		let x = self.dir_iter(inr, |name, _inr, kind| {
			if kind != InodeType::Directory || (name != "." && name != "..") {
				Some(name.to_os_string())
			} else {
				None
			}
		})?;

		if x.is_some() {
			log::debug!("rmdir({dinr}, {name:?}): x = {x:?}");
			return Err(err!(ENOTEMPTY));
		}

		let (child, dir_blk) = self.dir_unlink(dinr, name)?;

		self.unlink_entry(inr, OsStr::new(".."))?;
		self.unlink_entry(inr, OsStr::new("."))?;
		self.inode_free(inr)?;

		// The parent's link count comes off here -- but not directly.  It comes
		// off when `..` is unlinked above, because `..` named the parent, and
		// that is the only decrement a removed subdirectory should cause.
		//
		// What is missing is the *ordering*: that decrement used to reach the
		// disk before the entry removal did, so a crash in between left the
		// parent counting a subdirectory its tree no longer contains.  The
		// randomised crash test found it -- seed 10 -- and the repair is the same
		// one `mkdir` uses: hold the parent's inode until the directory block
		// holding the removal is on the disk.  See `Ufs::block_inode_on_dir`.
		let parent_blk = self.metadata_blk(self.superblock.ino_to_fsba(dinr));
		self.block_inode_on_dir(dinr, parent_blk, dir_blk);

		let _ = child;
		Ok(())
	}

	pub fn mknod(
		&mut self,
		dinr: InodeNum,
		name: &OsStr,
		kind: InodeType,
		perm: u16,
		uid: u32,
		gid: u32,
	) -> IoResult<InodeAttr> {
		self.assert_rw()?;
		check_name_is_legal(name, false)?;
		let mut ino = Inode::new(kind, perm, uid, gid, self.superblock.bsize as u32);
		let inr = self.inode_alloc(Some(dinr), &mut ino)?;
		self.dir_newlink(dinr, inr, name, kind)?;
		Ok(ino.as_attr(inr))
	}

	pub fn mkdir(
		&mut self,
		dinr: InodeNum,
		name: &OsStr,
		perm: u16,
		uid: u32,
		gid: u32,
	) -> IoResult<InodeAttr> {
		let inr = self
			.mknod(dinr, name, InodeType::Directory, perm, uid, gid)?
			.inr;

		// The entry `mknod()` just staged is in the last directory block, so
		// that block is the one the parent's link count has to wait for.
		let dsize = self.read_inode(dinr)?.size;
		let dir_blk = match self.dirent_block(dinr, dsize.saturating_sub(1))? {
			Some(b) => b,
			None => self.dirent_block(dinr, 0)?.ok_or_else(|| err!(EIO))?,
		};
		let parent_blk = self.metadata_blk(self.superblock.ino_to_fsba(dinr));

		let mut dino = self.read_inode(dinr)?;
		dino.nlink += 1;
		self.write_inode(dinr, &dino)?;
		self.block_inode_on_dir(dinr, parent_blk, dir_blk);

		// update nlink
		let mut ino = self.read_inode(inr)?;
		ino.nlink = 2;
		self.write_inode(inr, &ino)?;

		let block = newdir(dinr, inr, self.file.config())?;
		self.inode_truncate(inr, block.len() as u64)?;
		self.inode_write(inr, 0, &block)?;

		let ino = self.read_inode(inr)?;
		Ok(ino.as_attr(inr))
	}
}
