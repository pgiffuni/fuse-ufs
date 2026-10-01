#![cfg_attr(fuzzing, allow(dead_code, unused_imports, unused_mut))]

mod blockreader;
mod buf;
mod data;
mod decoder;
mod geom;
mod inode;
mod policy;
mod softdep;
#[cfg(test)]
mod testutil;
mod ufs;

#[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "macos"))]
pub const ENOATTR: i32 = libc::ENOATTR;
#[cfg(target_os = "linux")]
pub const ENOATTR: i32 = libc::ENODATA;

pub use crate::{
	blockreader::{Backend, BlockReader},
	buf::{BlockDevice, Buffer, BufferCache},
	data::{InodeAttr, InodeNum, InodeType},
	geom::{AllocationSummary, CgNum},
	softdep::{DepId, DepKind, DependencyEngine, Gate},
	ufs::{fsck::Report, Info, Ufs},
};
