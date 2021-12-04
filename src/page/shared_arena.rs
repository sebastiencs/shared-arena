
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering::*};
use std::sync::{Arc, Weak};

use std::ptr::NonNull;
use std::alloc::{alloc, dealloc, Layout};

use crate::cache_line::CacheAligned;
use crate::common::{BLOCK_PER_PAGE, Bitfield, MASK_ARENA_BIT};
use crate::block::{Block, PageTaggedPtr, PageKind};


pub struct PageSharedArena<T> {
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
    pub arena_pending_list: Weak<AtomicPtr<PageSharedArena<T>>>,
    pub next_free: AtomicPtr<PageSharedArena<T>>,
    pub next: AtomicPtr<PageSharedArena<T>>,
    pub in_free_list: AtomicBool,
}

impl<T> std::fmt::Debug for PageSharedArena<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageSharedArena")
         .field("next_free", &self.next_free.load(Relaxed))
         .field("next", &self.next.load(Relaxed))
         .finish()
    }
}

fn deallocate_page<T>(page: *mut PageSharedArena<T>) {
    let layout = Layout::new::<PageSharedArena<T>>();
    unsafe {
        std::ptr::drop_in_place(&mut (*page).arena_pending_list as *mut _);
        dealloc(page as *mut PageSharedArena<T> as *mut u8, layout);
    }
}

impl<T> PageSharedArena<T> {
    fn allocate() -> NonNull<PageSharedArena<T>> {
        let layout = Layout::new::<PageSharedArena<T>>();
        unsafe {
            let page = alloc(layout) as *const PageSharedArena<T>;
            NonNull::from(&*page)
        }
    }

    fn new(
        arena_pending_list: Weak<AtomicPtr<PageSharedArena<T>>>,
        next: *mut PageSharedArena<T>
    ) -> NonNull<PageSharedArena<T>>
    {
        let mut page_ptr = Self::allocate();
        let page_copy = page_ptr;

        let page = unsafe { page_ptr.as_mut() };

        // Initialize the page
        // Don't invoke any Drop here, the allocated page is uninitialized

        // We fill the bitfield with ones
        page.bitfield.store(!0, Relaxed);
        page.next_free = AtomicPtr::new(next);
        page.next = AtomicPtr::new(next);
        page.in_free_list = AtomicBool::new(true);

        let pending_ptr = &mut page.arena_pending_list as *mut Weak<AtomicPtr<PageSharedArena<T>>>;
        unsafe {
            pending_ptr.write(arena_pending_list);
        }

        // initialize the blocks
        for (index, block) in page.blocks.iter_mut().enumerate() {
            block.page = PageTaggedPtr::new(page_copy.as_ptr() as usize, index, PageKind::SharedArena);
            block.counter = AtomicUsize::new(0);
        }

        page_ptr
    }

    /// Make a new list of PageSharedArena
    ///
    /// Returns the first and last PageSharedArena in the list
    pub fn make_list(
        npages: usize,
        arena_pending_list: &Arc<AtomicPtr<PageSharedArena<T>>>
    ) -> (NonNull<PageSharedArena<T>>, NonNull<PageSharedArena<T>>)
    {
        let arena_pending_list = Arc::downgrade(arena_pending_list);

        let last = PageSharedArena::<T>::new(arena_pending_list.clone(), std::ptr::null_mut());
        let mut previous = last;

        for _ in 0..npages - 1 {
            let page = PageSharedArena::<T>::new(arena_pending_list.clone(), previous.as_ptr());
            previous = page;
        }

        (previous, last)
    }

    pub(crate) fn make_list_from_slice(
        pages: &[NonNull<PageSharedArena<T>>]
    ) -> (NonNull<PageSharedArena<T>>, NonNull<PageSharedArena<T>>) {
        for (index, page) in pages.iter().map(|p| unsafe { &mut *p.as_ptr() }).enumerate() {
            let next = pages.get(index + 1)
                            .map(|p| p.as_ptr())
                            .unwrap_or_else(std::ptr::null_mut);
            page.next_free = AtomicPtr::new(next);
            page.next = AtomicPtr::new(next);
            page.in_free_list = AtomicBool::new(true);
        }
        (
            pages.first().copied().unwrap(),
            pages.last().copied().unwrap(),
        )
    }

    /// Search for a free [`Block`] in the [`PageSharedArena`] and mark it as non-free
    ///
    /// If there is no free block, it returns None
    pub fn acquire_free_block(&self) -> Option<NonNull<Block<T>>> {
        loop {
            let bitfield = self.bitfield.load(Relaxed);

            let index_free = bitfield.trailing_zeros() as usize;

            if index_free == BLOCK_PER_PAGE {
                return None;
            }

            let bit = 1 << index_free;

            let previous_bitfield = self.bitfield.fetch_and(!bit, AcqRel);

            // We check that the bit was still set in previous_bitfield.
            // If the bit is zero, it means another thread took it.
            if previous_bitfield & bit != 0 {
                return self.blocks.get(index_free).map(NonNull::from);
            }
        }
    }

    pub(crate) fn drop_block(mut page: NonNull<PageSharedArena<T>>, block: NonNull<Block<T>>) {
        let page_ptr = page.as_ptr();
        let page = unsafe { page.as_mut() };
        let block = unsafe { block.as_ref() };

        let bit = 1 << block.page.index_block();

        // We set our bit to mark the block as free.
        // fetch_add is faster than fetch_or (xadd vs cmpxchg), and
        // we're sure to be the only thread to set this bit.
        let old_bitfield = page.bitfield.fetch_add(bit, AcqRel);

        let new_bitfield = old_bitfield | bit;

        // The bit dedicated to the Arena is inversed (1 for used, 0 for free)
        if !new_bitfield == MASK_ARENA_BIT {
            // We were the last block/arena referencing this page
            // Deallocate it
            deallocate_page(page_ptr);
            return;
        }

        // Put our page in pending_free_list of the arena, if necessary
        if !page.in_free_list.load(Relaxed) {
            // Another thread might have changed self.in_free_list
            // We could use compare_exchange here but swap is faster
            // 'lock cmpxchg' vs 'xchg' on x86
            // For self reference:
            // https://gpuopen.com/gdc-presentations/2019/gdc-2019-s2-amd-ryzen-processor-software-optimization.pdf
            if !page.in_free_list.swap(true, Acquire) {
                if let Some(arena_pending_list) = page.arena_pending_list.upgrade() {
                    loop {
                        let current = arena_pending_list.load(Relaxed);
                        page.next_free.store(current, Relaxed);

                        if arena_pending_list.compare_exchange(
                            current, page_ptr, AcqRel, Relaxed
                        ).is_ok() {
                            break;
                        }
                    }
                }
            }
        }
    }
}

pub(crate) fn drop_page<T>(page: *mut PageSharedArena<T>) {
    // We clear the bit dedicated to the arena
    let old_bitfield = {
        let page = unsafe { page.as_ref().unwrap() };
        page.bitfield.fetch_sub(MASK_ARENA_BIT, AcqRel)
    };

    if !old_bitfield == 0 {
        // No one is referencing this page anymore (neither Arena, ArenaBox or ArenaArc)
        deallocate_page(page);
    }
}

impl<T> Drop for PageSharedArena<T> {
    fn drop(&mut self) {
        panic!("PAGE");
    }
}
