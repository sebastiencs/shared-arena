use std::sync::atomic::AtomicUsize;
use std::cell::Cell;
use static_assertions::const_assert;

pub(crate) const BITFIELD_WIDTH: usize = std::mem::size_of::<AtomicUsize>() * 8;
pub(crate) const BLOCK_PER_PAGE: usize = BITFIELD_WIDTH - 1;
pub(crate) const MASK_ARENA_BIT: usize = 1 << (BITFIELD_WIDTH - 1);

pub(crate) type Bitfield = AtomicUsize;
pub(crate) type Pointer<T> = Cell<*mut T>;

const_assert!(std::mem::size_of::<Bitfield>() == BITFIELD_WIDTH / 8);
