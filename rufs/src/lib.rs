#![cfg_attr(fuzzing, allow(dead_code, unused_imports, unused_mut))]

mod blockreader;
mod data;
mod decoder;
mod geom;
mod inode;
mod policy;
#[cfg(test)]
mod testutil;
mod ufs;

#[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "macos"))]
pub const ENOATTR: i32 = libc::ENOATTR;
#[cfg(target_os = "linux")]
pub const ENOATTR: i32 = libc::ENODATA;

pub use crate::{
	blockreader::{Backend, BlockReader},
	data::{InodeAttr, InodeNum, InodeType},
	geom::{AllocationSummary, CgNum},
	ufs::{fsck::Report, Info, Ufs},
};
