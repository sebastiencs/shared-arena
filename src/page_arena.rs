
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering::*};
use std::sync::{Arc, Weak};
use std::cell::Cell;

use std::ptr::NonNull;
use std::alloc::{alloc, dealloc, Layout};
use crate::cache_line::CacheAligned;
use crate::page::{Block, BLOCK_PER_PAGE, PageTaggedPtr, PageKind};

pub type Bitfield = usize;
pub type BitfieldAtomic = AtomicUsize;


pub struct PageArena<T> {
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
    pub bitfield: Cell<Bitfield>,
    pub bitfield_atomic: CacheAligned<BitfieldAtomic>,
    /// Array of Block
    pub blocks: [Block<T>; BLOCK_PER_PAGE],
    pub arena_pending_list: Weak<AtomicPtr<PageArena<T>>>,
    pub next_free: AtomicPtr<PageArena<T>>,
    pub next: AtomicPtr<PageArena<T>>,
    pub in_free_list: AtomicBool,
}

impl<T> std::fmt::Debug for PageArena<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageArena")
         .field("next_free", &self.next_free.load(Relaxed))
         .field("next", &self.next.load(Relaxed))
         .finish()
    }
}

fn deallocate_page<T>(page: *mut PageArena<T>) {
    let layout = Layout::new::<PageArena<T>>();
    unsafe {
        dealloc(page as *mut PageArena<T> as *mut u8, layout);
    }
}

impl<T> PageArena<T> {
    fn allocate() -> NonNull<PageArena<T>> {
        let layout = Layout::new::<PageArena<T>>();
        unsafe {
            let page = alloc(layout) as *const PageArena<T>;
            NonNull::from(&*page)
        }
    }

    fn new(
        arena_pending_list: Weak<AtomicPtr<PageArena<T>>>,
        next: *mut PageArena<T>
    ) -> NonNull<PageArena<T>>
    {
        let mut page_ptr = Self::allocate();
        let page_copy = page_ptr;

        let page = unsafe { page_ptr.as_mut() };

        // Initialize the page
        // Don't invoke any Drop here, the allocated page is uninitialized

        // We fill the bitfields with ones
        page.bitfield = Cell::new(!0);
        // page.bitfield = Cell::new(!0);
        page.bitfield_atomic.store(0, Relaxed);
        page.next_free = AtomicPtr::new(next);
        page.next = AtomicPtr::new(next);
        page.in_free_list = AtomicBool::new(true);

        let pending_ptr = &mut page.arena_pending_list as *mut Weak<AtomicPtr<PageArena<T>>>;
        unsafe {
            pending_ptr.write(arena_pending_list);
        }

        // initialize the blocks
        for (index, block) in page.blocks.iter_mut().enumerate() {
            block.page = PageTaggedPtr::new(page_copy.as_ptr() as usize, index, PageKind::Arena);
            block.counter = AtomicUsize::new(0);
        }

        page_ptr
    }

    /// Make a new list of PageArena
    ///
    /// Returns the first and last PageArena in the list
    pub fn make_list(
        npages: usize,
        arena_pending_list: &Arc<AtomicPtr<PageArena<T>>>
    ) -> (NonNull<PageArena<T>>, NonNull<PageArena<T>>)
    {
        let arena_pending_list = Arc::downgrade(arena_pending_list);

        let last = PageArena::<T>::new(arena_pending_list.clone(), std::ptr::null_mut());
        let mut previous = last;

        for _ in 0..npages - 1 {
            let page = PageArena::<T>::new(arena_pending_list.clone(), previous.as_ptr());
            previous = page;
        }

        (previous, last)
    }

    /// Search for a free [`Block`] in the [`PageArena`] and mark it as non-free
    ///
    /// If there is no free block, it returns None
    pub fn acquire_free_block(&self) -> Option<NonNull<Block<T>>> {
        loop {
            let index_free = self.bitfield.get().trailing_zeros() as usize;

            if index_free == BLOCK_PER_PAGE {

                if self.bitfield_atomic.load(Relaxed) != 0 {
                    self.bitfield.set(self.bitfield.get() | self.bitfield_atomic.swap(0, AcqRel));
                    continue;
                }

                return None;
            }

            // println!("BEFORE {:064b}", self.bitfield.get());
            self.bitfield.set(self.bitfield.get() & !(1 << index_free));
            // println!("AFTER  {:064b}", self.bitfield.get());

            return Some(NonNull::from(&self.blocks[index_free]))
        }
    }

    pub(super) fn drop_block(mut page: NonNull<PageArena<T>>, block: NonNull<Block<T>>) {
        let page_ptr = page.as_ptr();
        let page = unsafe { page.as_mut() };
        let block = unsafe { block.as_ref() };

        unsafe {
            // Drop the inner value
            std::ptr::drop_in_place(block.value.get());
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

        let bit = 1 << block.page.index_block();

        // We set our bit to mark the block as free.
        // fetch_add is faster than fetch_or (xadd vs cmpxchg), and
        // we're sure to be the only thread to set this bit.
        let old_bitfield = page.bitfield_atomic.fetch_add(bit, AcqRel);

        let new_bitfield = old_bitfield | bit;

        // The bit dedicated to the Arena is inversed (1 for used, 0 for free)
        if new_bitfield == !0 {
            // We were the last block/arena referencing this page
            // Deallocate it
            deallocate_page(page_ptr);
        }
    }
}

pub(super) fn drop_page<T>(page: *mut PageArena<T>) {
    let bitfield = {
        let page = unsafe { page.as_ref().unwrap() };
        let bitfield = page.bitfield.get();
        let old_bitfield = page.bitfield_atomic.fetch_or(bitfield, AcqRel);

        old_bitfield | bitfield
    };

    if bitfield == !0 {
        deallocate_page(page);
    }
}

impl<T> Drop for PageArena<T> {
    fn drop(&mut self) {
        panic!("PAGE");
    }
}
