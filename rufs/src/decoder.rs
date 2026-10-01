use std::io::{Error, ErrorKind, Read, Result, Seek, SeekFrom, Write};

use bincode_next::{
	config::{BigEndian, Configuration, Fixint, LittleEndian, MsbFirst, NoLimit, SkipBitPacking},
	Decode,
	Encode,
};

use crate::{
	blockreader::{Backend, BlockReader},
	buf::BlockDevice,
};

#[derive(Clone, Copy)]
pub enum Config {
	Little(Configuration<LittleEndian, Fixint, NoLimit>),
	Big(Configuration<BigEndian, Fixint, NoLimit, SkipBitPacking, MsbFirst>),
}

impl Config {
	pub const fn little() -> Self {
		let cfg = bincode_next::config::standard()
			.with_fixed_int_encoding()
			.with_little_endian();
		Self::Little(cfg)
	}

	pub const fn big() -> Self {
		let cfg = bincode_next::config::standard()
			.with_fixed_int_encoding()
			.with_big_endian();
		Self::Big(cfg)
	}

	fn decode<T: Decode<()>>(&self, rdr: &mut impl Read) -> Result<T> {
		match self {
			Self::Little(cfg) => bincode_next::decode_from_std_read(rdr, *cfg),
			Self::Big(cfg) => bincode_next::decode_from_std_read(rdr, *cfg),
		}
		.map_err(|_| Error::new(ErrorKind::InvalidInput, "failed to decode"))
	}

	fn encode(&self, wtr: &mut impl Write, x: &impl Encode) -> Result<()> {
		match self {
			Self::Little(cfg) => bincode_next::encode_into_std_write(x, wtr, *cfg),
			Self::Big(cfg) => bincode_next::encode_into_std_write(x, wtr, *cfg),
		}
		.map(|_| ())
		.map_err(|_| Error::new(ErrorKind::InvalidInput, "failed to encode"))
	}
}

pub struct Decoder<T: Read> {
	inner:  T,
	config: Config,
}

impl<T: Read> Decoder<T> {
	pub fn new(inner: T, config: Config) -> Self {
		Self { inner, config }
	}

	pub fn inner(&self) -> &T {
		&self.inner
	}

	pub fn decode<X: Decode<()>>(&mut self) -> Result<X> {
		self.config.decode(&mut self.inner)
	}

	pub fn read(&mut self, buf: &mut [u8]) -> Result<()> {
		self.inner.read_exact(buf)
	}

	pub fn config(&self) -> Config {
		self.config
	}
}

impl<T: Read + Write> Decoder<T> {
	pub fn write(&mut self, buf: &[u8]) -> Result<()> {
		self.inner.write_all(buf)
	}

	pub fn encode(&mut self, x: &impl Encode) -> Result<()> {
		self.config.encode(&mut self.inner, x)
	}

	pub fn fill(&mut self, b: u8, num: usize) -> Result<()> {
		for _ in 0..num {
			self.write(&[b])?;
		}
		Ok(())
	}
}

impl<T: Read + Seek> Decoder<T> {
	pub fn read_at(&mut self, pos: u64, buf: &mut [u8]) -> Result<()> {
		self.seek(pos)?;
		self.read(buf)
	}

	pub fn decode_at<X: Decode<()>>(&mut self, pos: u64) -> Result<X> {
		self.seek(pos)?;
		self.decode()
	}

	pub fn seek(&mut self, pos: u64) -> Result<()> {
		self.inner.seek(SeekFrom::Start(pos))?;
		Ok(())
	}

	pub fn align_to(&mut self, align: u64) -> Result<()> {
		assert_eq!(align.count_ones(), 1);
		let pos = self.inner.stream_position()?;
		let new_pos = (pos + align - 1) & !(align - 1);
		self.seek(new_pos)
	}

	pub fn pos(&mut self) -> Result<u64> {
		self.inner.stream_position()
	}

	pub fn seek_relative(&mut self, off: i64) -> Result<()> {
		self.inner.seek_relative(off)?;
		Ok(())
	}
}

impl<T: Read + Write + Seek> Decoder<T> {
	pub fn write_at(&mut self, pos: u64, buf: &[u8]) -> Result<()> {
		self.seek(pos)?;
		self.write(buf)
	}

	pub fn encode_at(&mut self, pos: u64, x: &impl Encode) -> Result<()> {
		self.seek(pos)?;
		self.encode(x)
	}

	pub fn fill_at(&mut self, pos: u64, b: u8, num: usize) -> Result<()> {
		self.seek(pos)?;
		self.fill(b, num)
	}
}

/// Make the decoder stack usable as a raw persistence device.
///
/// # Why the metadata path needs this
///
/// [`crate::buf::BufferCache`] persists through a [`BlockDevice`], but `Ufs`
/// owns a `Decoder<BlockReader<R>>`, so before the cache could be introduced
/// at all there had to be a way for the two to meet.  Implementing
/// [`BlockDevice`] here is that adapter, and it is deliberately the whole of
/// it: the two random-access operations the decoder already had, forwarded.
///
/// # What it does *not* do
///
/// The decoder does **not** cache anything.  It does not keep a copy of a
/// metadata structure, it does not know that a byte range is unsafe to write,
/// and it makes no attempt to order one write against another.  `BlockDevice`
/// is the raw persistence seam; [`crate::buf::BufferCache`] decides *when* a
/// metadata image is persisted, and [`crate::softdep::DependencyEngine`]
/// decides *which parts of it* are allowed to be.
///
/// Keeping the adapter this thin is the point.  A decoder that cached would
/// give the filesystem two independent notions of what a metadata block
/// contains, and the Soft Updates rules would apply to only one of them.
///
/// # Read-only mounts
///
/// A [`crate::blockreader::BlockReader`] opened read-only panics on write, so
/// a `write_at` on a read-only mount panics rather than failing.  Callers must
/// check [`crate::Ufs::write_enabled`] first, which is the existing
/// convention throughout this crate.
impl<R: Backend> BlockDevice for Decoder<BlockReader<R>> {
	fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
		Decoder::read_at(self, off, buf)
	}

	fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<()> {
		Decoder::write_at(self, off, buf)
	}
}

#[cfg(test)]
mod t {
	use std::io::Cursor;

	use super::*;
	use crate::{
		blockreader::BlockReader,
		buf::{BufferCache, Written},
	};

	/// The `BlockReader` block size used throughout these tests; deliberately
	/// not a power of two larger than 512 so that an unaligned offset really
	/// does cross a boundary.
	const BSIZE: usize = 512;

	/// An in-memory `Decoder<BlockReader<_>>` of `len` bytes.
	fn dev(len: usize, config: Config) -> Decoder<BlockReader<Cursor<Vec<u8>>>> {
		let inner = BlockReader::new(Cursor::new(vec![0u8; len]), BSIZE, true);
		Decoder::new(inner, config)
	}

	/// The bytes the decoder stack actually holds.
	fn image(d: &Decoder<BlockReader<Cursor<Vec<u8>>>>) -> Vec<u8> {
		d.inner().inner().get_ref().clone()
	}

	/// The adapter is a working [`BlockDevice`]: a write lands where it was
	/// asked to, even when the write straddles the `BlockReader`'s block.
	#[test]
	fn block_device_round_trip_across_a_boundary() {
		let mut d = dev(4 * BSIZE as u64 as usize, Config::little());
		let off = BSIZE as u64 - 3;
		let payload: Vec<u8> = (0..16u8).collect();

		d.write_at(off, &payload).unwrap();
		assert_eq!(
			&image(&d)[off as usize..off as usize + payload.len()],
			&payload[..]
		);

		let mut back = vec![0u8; payload.len()];
		d.read_at(off, &mut back).unwrap();
		assert_eq!(back, payload);
	}

	/// A write does not disturb its neighbours: the adapter is a plain
	/// read/write pair, with no read-modify-write surprise.
	#[test]
	fn block_device_write_is_exact() {
		let mut d = dev(BSIZE, Config::little());
		d.write_at(0, &vec![0xAAu8; BSIZE]).unwrap();
		d.write_at(100, b"UFS2").unwrap();
		let img = image(&d);
		assert_eq!(&img[100..104], b"UFS2");
		assert!(img[0..100].iter().all(|&b| b == 0xAA));
		assert!(img[104..].iter().all(|&b| b == 0xAA));
	}

	/// The metadata stack works end to end: `BufferCache` over a `Decoder`
	/// over a `BlockReader`, writing the safe image and reporting what it did.
	#[test]
	fn buffer_cache_over_the_decoder_stack() {
		let mut d = dev(8 * BSIZE, Config::little());
		let mut cache = BufferCache::new(BSIZE as u64, BSIZE as u64);

		cache.get_mut(&mut d, 2).unwrap().data_mut()[0..8].copy_from_slice(b"UFS2UFS2");
		// The second half of the pattern is still waiting on a dependency.
		cache.peek_mut(2).unwrap().mark_unsafe(4, 4);

		assert_eq!(cache.write_back(&mut d, 2).unwrap(), Written::Safe);
		let img = image(&d);
		assert_eq!(&img[2 * BSIZE..2 * BSIZE + 4], b"UFS2");
		assert_eq!(
			&img[2 * BSIZE + 4..2 * BSIZE + 8],
			&[0u8; 4],
			"the gated range reaches the device as zero"
		);

		// Resolving the gate lets the live image out on the next write-back.
		cache.get_mut(&mut d, 2).unwrap().publish_all();
		assert_eq!(cache.write_back(&mut d, 2).unwrap(), Written::Full);
		assert_eq!(&image(&d)[2 * BSIZE..2 * BSIZE + 8], b"UFS2UFS2");
		assert!(cache.is_clean());
	}
}
