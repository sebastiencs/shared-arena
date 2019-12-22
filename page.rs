
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering::*};
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::alloc::{alloc, dealloc, Layout};

use static_assertions::const_assert;

use crate::cache_line::CacheAligned;

// // https://stackoverflow.com/a/53646925
// const fn max(a: usize, b: usize) -> usize {
//     [a, b][(a < b) as usize]
// }

// const ALIGN_BLOCK: usize = max(128, 64);

pub type IndexInPage = usize;

pub const BITFIELD_WIDTH: usize = 64;
pub const BLOCK_PER_PAGE: usize = BITFIELD_WIDTH - 1;

type Bitfield = AtomicU64;

const_assert!(std::mem::size_of::<Bitfield>() == BITFIELD_WIDTH / 8);

// We make the struct repr(C) to ensure that the pointer to the inner
// value remains at offset 0. This is to avoid any pointer arithmetic
// when dereferencing it
#[repr(C)]
pub struct Block<T> {
    /// Inner value
    pub value: UnsafeCell<T>,
    /// Number of references to this block
    pub counter: AtomicUsize,
    /// Read only and initialized on Page creation
    /// Doesn't need to be atomic
    pub index_in_page: usize,
}

pub struct Page<T> {
    /// Bitfield representing free and non-free blocks.
    /// - 1 = free
    /// - 0 = non-free
    /// The most significant bit is dedicated to the Page itself and is
    /// used to determine when to deallocate the Page.
    /// With this bit reserved, we used the BITFIELD_WIDTH - 1 bits for
    /// the blocks.
    /// Note that the bit for the page is inversed:
    /// - 1 = Page is still referenced from an arena
    /// - 0 = The Page isn't referenced in an arena
    /// It is inversed so that Bitfield::trailing_zeros doesn't
    /// count that bit
    pub bitfield: CacheAligned<Bitfield>,
    /// Array of Block
    /// Ideally, each block would be aligned on the cache line size
    /// but this would make the Page too big
    pub blocks: [Block<T>; BLOCK_PER_PAGE],
}

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

        // initialize the blocks
        for (index, block) in page.blocks.iter_mut().enumerate() {
            block.index_in_page = index;
            block.counter = AtomicUsize::new(0);
        }

        page_ptr
    }


    /// Search for a free [`Block`] in the [`Page`] and mark it as non-free
    ///
    /// If there is no free block, it returns None
    #[inline(never)]
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

        // The bit dedicated to the Page is inversed (1 for used, 0 for free)
        if !new_bitfield == 1 << 63 {
            // No one is referencing this page anymore (neither Arena, ArenaBox or ArenaArc)
            self.deallocate();
        }
    }
}
