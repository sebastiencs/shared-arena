
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering::*};
use std::cell::UnsafeCell;
use std::sync::{Arc, Weak};

use std::ptr::NonNull;
use std::alloc::{alloc, dealloc, Layout};

use static_assertions::const_assert;

use crate::cache_line::CacheAligned;

// // https://stackoverflow.com/a/53646925
// const fn max(a: usize, b: usize) -> usize {
//     [a, b][(a < b) as usize]
// }

// const ALIGN_BLOCK: usize = max(128, 64);

pub const BITFIELD_WIDTH: usize = std::mem::size_of::<AtomicUsize>() * 8;
pub const BLOCK_PER_PAGE: usize = BITFIELD_WIDTH - 1;
pub const MASK_ARENA_BIT: usize = 1 << (BITFIELD_WIDTH - 1);

pub type Bitfield = AtomicUsize;

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
    /// The most significant bit is dedicated to the arena and is
    /// used to determine when to deallocate the Page.
    /// With this bit reserved, we used BITFIELD_WIDTH - 1 bits for
    /// the blocks.
    /// Note that the bit for the arena is inversed:
    /// - 1 = Page is still referenced from an arena
    /// - 0 = The Page isn't referenced in an arena
    /// It is inversed so that Bitfield::trailing_zeros doesn't
    /// count that bit
    pub bitfield: CacheAligned<Bitfield>,
    /// Array of Block
    pub blocks: [Block<T>; BLOCK_PER_PAGE],
    pub arena_free_list: Weak<AtomicPtr<Page<T>>>,
    pub next_free: AtomicPtr<Page<T>>,
    pub next: AtomicPtr<Page<T>>,
    pub in_free_list: AtomicBool,
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

    fn new(
        arena_free_list: Weak<AtomicPtr<Page<T>>>,
        next: *mut Page<T>
    ) -> NonNull<Page<T>>
    {
        let mut page_ptr = Self::allocate();

        let page = unsafe { page_ptr.as_mut() };

        // Initialize the page
        // Don't invoke any Drop here, the allocated page is uninitialized

        // We fill the bitfield with ones
        page.bitfield.store(!0, Relaxed);
        page.next_free = AtomicPtr::new(next);
        page.next = AtomicPtr::new(next);
        page.in_free_list = AtomicBool::new(true);

        let free_ptr = &mut page.arena_free_list as *mut Weak<AtomicPtr<Page<T>>>;
        unsafe {
            free_ptr.write(arena_free_list);
        }

        // initialize the blocks
        for (index, block) in page.blocks.iter_mut().enumerate() {
            block.index_in_page = index;
            block.counter = AtomicUsize::new(0);
        }

        page_ptr
    }

    /// Make a new list of Page
    ///
    /// Returns the first and last Page in the list
    pub fn make_list(
        npages: usize,
        arena_free_list: &Arc<AtomicPtr<Page<T>>>
    ) -> (NonNull<Page<T>>, NonNull<Page<T>>)
    {
        let arena_free_list = Arc::downgrade(arena_free_list);

        let last = Page::<T>::new(arena_free_list.clone(), std::ptr::null_mut());
        let mut previous = last;

        for _ in 0..npages - 1 {
            let previous_ptr = unsafe { previous.as_mut() };
            let page = Page::<T>::new(arena_free_list.clone(), previous_ptr);
            previous = page;
        }

        (previous, last)
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

        // Bitfield where we clear the bit of the free block to mark
        // it as non-free
        let mut new_bitfield = bitfield & !(1 << index_free);

        while let Err(x) = self.bitfield.compare_exchange_weak(
            bitfield, new_bitfield, AcqRel, Relaxed
        ) {
            bitfield = x;
            index_free = bitfield.trailing_zeros() as usize;

            if index_free == BLOCK_PER_PAGE {
                return None;
            }

            new_bitfield = bitfield & !(1 << index_free);
        }

        self.blocks.get(index_free).map(NonNull::from)
    }

    pub(super) fn drop_block(&mut self, block: &Block<T>) {
        unsafe {
            // Drop the inner value
            std::ptr::drop_in_place(block.value.get());
        }

        let index_in_page = block.index_in_page;

        let bitfield_ref = &self.bitfield;

        let mut bitfield = bitfield_ref.load(Relaxed);

        // We set our bit to mark the block as free
        let mut new_bitfield = bitfield | (1 << index_in_page);

        while let Err(x) = bitfield_ref.compare_exchange_weak(
            bitfield, new_bitfield, AcqRel, Relaxed
        ) {
            bitfield = x;
            new_bitfield = bitfield | (1 << index_in_page);
        }

        // The bit dedicated to the Page is inversed (1 for used, 0 for free)
        if !new_bitfield == MASK_ARENA_BIT {
            // We were the last block/arena referencing this page
            // Deallocate it
            self.deallocate();
            return;
        }

        if !self.in_free_list.load(Relaxed) {
            if self.in_free_list.compare_exchange(
                false, true, Release, Relaxed
            ).is_err() {
                return;
            }

            let arena_free_list = match self.arena_free_list.upgrade() {
                Some(ptr) => ptr,
                _ => return // The arena has been dropped
            };

            let page_ptr = self as *mut Page<T>;

            loop {
                let current = arena_free_list.load(Relaxed);
                self.next_free.store(current, Relaxed);

                if arena_free_list.compare_exchange_weak(
                    current, page_ptr, AcqRel, Relaxed
                ).is_ok() {
                    break;
                }
            }
        }
    }
}

impl<T> Drop for Page<T> {
    fn drop(&mut self) {
        let mut bitfield = self.bitfield.load(Relaxed);

        // We clear the bit dedicated to the arena
        let mut new_bitfield = bitfield & !MASK_ARENA_BIT;

        while let Err(x) = self.bitfield.compare_exchange_weak(
            bitfield, new_bitfield, AcqRel, Relaxed
        ) {
            bitfield = x;
            new_bitfield = bitfield & !MASK_ARENA_BIT
        }

        // The bit dedicated to the arena is inversed (1 for used, 0 for free)
        if !new_bitfield == MASK_ARENA_BIT {
            // No one is referencing this page anymore (neither Arena, ArenaBox or ArenaArc)
            self.deallocate();
        }
    }
}
