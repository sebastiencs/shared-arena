
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering::*};
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::ptr::NonNull;

use crate::cache_line::CacheAligned;

// https://stackoverflow.com/a/53646925
const fn max(a: usize, b: usize) -> usize {
    [a, b][(a < b) as usize]
}

const ALIGN_BLOCK: usize = max(128, 64);

pub struct Block<T> {
    /// Inner value
    pub value: UnsafeCell<T>,
    /// Number of references to this block
    pub counter: AtomicUsize,
    /// Read only and initialized on Page creation
    /// Doesn't need to be atomic
    pub index_in_page: usize,
}

pub type IndexInPage = usize;

use static_assertions::const_assert;

const BITFIELD_WIDTH: usize = 64;
pub const BLOCK_PER_PAGE: usize = BITFIELD_WIDTH - 1;

type Bitfield = AtomicU64;

pub struct Page<T> {
    pub bitfield: CacheAligned<Bitfield>,
    pub blocks: [Block<T>; BLOCK_PER_PAGE],
}

const_assert!(std::mem::size_of::<Bitfield>() == BITFIELD_WIDTH / 8);

impl<T> Page<T> {
    fn allocate() -> NonNull<Page<T>> {
        let layout = Layout::new::<Page<T>>();
        unsafe {
            let page = alloc(layout) as *const Page<T>;
            NonNull::from(&*page)
        }
    }

    pub fn deallocate(&mut self) {
        let layout = Layout::new::<Page<T>>();
        unsafe {
            dealloc(self as *mut Page<T> as *mut u8, layout);
        }
    }

    pub fn new() -> NonNull<Page<T>> {
        let mut page_ptr = Self::allocate();

        let page = unsafe { page_ptr.as_mut() };

        // We fill the bitfield with ones
        page.bitfield.store(!0, Relaxed);

        for (index, block) in page.blocks.iter_mut().enumerate() {
            block.index_in_page = index;
            block.counter = AtomicUsize::new(0);
        }

        page_ptr
    }


    /// Search for a free [`Block`] in the [`Page`] and mark it as non-free
    ///
    /// If there is no free block, it returns None
    pub fn acquire_free_block(&self) -> Option<NonNull<Block<T>>> {

        let mut bitfield = self.bitfield.load(Relaxed);

        let mut index_free = bitfield.trailing_zeros() as usize;

        if index_free == BLOCK_PER_PAGE {
            return None;
        }

        // Bitfield where we clear the bit of the free node to mark
        // it as non-free
        let mut new_bitfield = bitfield & !(1 << index_free);

        while let Err(x) = self.bitfield.compare_exchange(
            bitfield, new_bitfield, SeqCst, Relaxed
        ) {
            bitfield = x;
            index_free = bitfield.trailing_zeros() as usize;

            if index_free == BLOCK_PER_PAGE {
                return None;
            }

            new_bitfield = bitfield & !(1 << index_free);
        }

        Some(NonNull::from(&self.blocks[index_free]))
    }
}

impl<T> Drop for Page<T> {
    fn drop(&mut self) {
        let mut bitfield = self.bitfield.load(Relaxed);

        // We clear the bit dedicated to the page to mark it as free
        let mut new_bitfield = bitfield & !(1 << (BITFIELD_WIDTH - 1));

        while let Err(x) = self.bitfield.compare_exchange_weak(
            bitfield, new_bitfield, SeqCst, Relaxed
        ) {
            bitfield = x;
            new_bitfield = bitfield | (1 << (BITFIELD_WIDTH - 1));
        }

        if !new_bitfield == 1 << 63 {
            // No one is referencing this page anymore (neither Arena, ArenaBox or ArenaArc)
            self.deallocate();
        }
    }
}

use std::alloc::{alloc, dealloc, Layout};
