
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;

use crate::cache_line::CacheAligned;

pub const BLOCK_PER_PAGE: usize = 32;
pub const WRONG_NODE_INDEX: usize = BLOCK_PER_PAGE + 1;

pub struct Block<T> {
    /// Read only and initialized on Page creation
    /// Doesn't need to be atomic
    pub index_in_page: usize,
    /// Number of references to this block
    pub counter: AtomicUsize,
    /// Inner value
    pub value: UnsafeCell<T>,
}

#[derive(Debug)]
pub struct IndexInPage(pub usize);

impl From<usize> for IndexInPage {
    fn from(n: usize) -> IndexInPage {
        IndexInPage(n)
    }
}

impl From<IndexInPage> for usize {
    fn from(n: IndexInPage) -> usize {
        n.0
    }
}

pub struct Page<T> {
    /// Bitfield representing the free and non-free blocks:
    /// - 1 => Free
    /// - 0 => Non free
    /// We use four u8 instead of one u32 to reduce contention
    pub bitfield: [CacheAligned<AtomicU8>; 4],

    /// Array of blocks
    pub nodes: CacheAligned<[Block<T>; BLOCK_PER_PAGE]>,
}

impl<T> Page<T> {
    pub fn new() -> Page<T> {

        // MaybeUninit doesn't allow field initialization :(
        // https://doc.rust-lang.org/std/mem/union.MaybeUninit.html#initializing-a-struct-field-by-field
        #[allow(clippy::uninit_assumed_init)]
        let mut nodes: [Block<T>; BLOCK_PER_PAGE] = unsafe { MaybeUninit::uninit().assume_init() };

        for (index, node) in nodes.iter_mut().enumerate() {
            node.index_in_page = index;
            node.counter = AtomicUsize::new(0);
        }

        Page {
            nodes: CacheAligned::new(nodes),
            bitfield: [
                CacheAligned::new(AtomicU8::new(!0)),
                CacheAligned::new(AtomicU8::new(!0)),
                CacheAligned::new(AtomicU8::new(!0)),
                CacheAligned::new(AtomicU8::new(!0)),
            ]
        }
    }

    /// Search for a free [`Block`] in the [`Page`] and mark it as non-free
    ///
    /// If there is no free block, it returns None
    pub fn acquire_free_block(&self) -> Option<IndexInPage> {
        'outer: for (index, byte) in self.bitfield.iter().enumerate() {
            let mut bitfield = byte.load(Ordering::Relaxed);

            let mut index_free = bitfield.trailing_zeros();

            if index_free == 8 {
                continue;
            }

            // Bitfield where we clear the bit of the free block to mark
            // it as non-free
            let mut new_bitfield = bitfield & !(1 << index_free);

            while let Err(x) = byte.compare_exchange_weak(
                bitfield, new_bitfield, Ordering::SeqCst, Ordering::Relaxed
            ) {
                bitfield = x;
                index_free = bitfield.trailing_zeros();

                if index_free == 8 {
                    continue 'outer;
                }

                new_bitfield = bitfield & !(1 << index_free);
            }

            return Some((index * 8 + index_free as usize).into());
        }

        None
    }
}

impl<T> Default for Page<T> {
    fn default() -> Page<T> {
        Page::new()
    }
}
