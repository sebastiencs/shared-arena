
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicPtr, AtomicUsize, Ordering::*};
use std::cell::UnsafeCell;
use std::sync::{Arc, Weak};

use std::ptr::NonNull;
use std::alloc::{alloc, dealloc, Layout};

use static_assertions::const_assert;

use crate::cache_line::CacheAligned;
use super::page::{Bitfield, BLOCK_PER_PAGE, Block, MASK_ARENA_BIT};

use crossbeam_epoch::{self as epoch, Owned, Shared, Guard, Atomic};

pub struct SharedPage<T> {
    pub bitfield: CacheAligned<Bitfield>,
    pub blocks: [Block<T>; BLOCK_PER_PAGE],
    pub arena_free_list: Weak<Atomic<SharedPage<T>>>,
    pub next_free: Atomic<SharedPage<T>>,
    pub next: Atomic<SharedPage<T>>,
    pub in_free_list: AtomicBool,
}

impl<T> SharedPage<T> {
    fn allocate() -> NonNull<SharedPage<T>> {
        let layout = Layout::new::<SharedPage<T>>();
        unsafe {
            let page = alloc(layout) as *const SharedPage<T>;
            NonNull::from(&*page)
        }
    }

    pub fn deallocate(&mut self) {
        let layout = Layout::new::<SharedPage<T>>();
        unsafe {
            dealloc(self as *mut SharedPage<T> as *mut u8, layout);
        }
    }

    pub fn new(
        arena_free_list: &Arc<Atomic<SharedPage<T>>>,
        next: *mut SharedPage<T>
    ) -> NonNull<SharedPage<T>>
    {
        let mut page_ptr = Self::allocate();

        let page = unsafe { page_ptr.as_mut() };

        // Initialize the page
        // Don't invoke any Drop here, the allocated page is uninitialized

        // We fill the bitfield with ones
        page.bitfield.store(!0, Relaxed);
        page.next_free = Atomic::from(next as *const _);
        page.next = Atomic::from(next as *const _);
        page.in_free_list = AtomicBool::new(true);

        let free_ptr = &mut page.arena_free_list as *mut Weak<Atomic<SharedPage<T>>>;
        unsafe {
            free_ptr.write(Arc::downgrade(arena_free_list));
        }

        // initialize the blocks
        for (index, block) in page.blocks.iter_mut().enumerate() {
            block.index_in_page = index;
            block.counter = AtomicUsize::new(0);
        }

        page_ptr
    }

    pub fn acquire_free_block(&self) -> Option<NonNull<Block<T>>> {

        let mut bitfield = self.bitfield.load(Relaxed);

        let mut index_free = bitfield.trailing_zeros() as usize;

        if index_free == BLOCK_PER_PAGE {
            return None;
        }

        // Bitfield where we clear the bit of the free node to mark
        // it as non-free
        let mut new_bitfield = bitfield & !(1 << index_free);

        while let Err(x) = self.bitfield.compare_exchange_weak(
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

impl<T> Drop for SharedPage<T> {
    fn drop(&mut self) {
        let mut bitfield = self.bitfield.load(Relaxed);

        // We clear the bit dedicated to the arena
        let mut new_bitfield = bitfield & !MASK_ARENA_BIT;

        while let Err(x) = self.bitfield.compare_exchange_weak(
            bitfield, new_bitfield, SeqCst, Relaxed
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
