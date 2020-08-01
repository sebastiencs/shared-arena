use std::cell::Cell;
use std::ptr::NonNull;
use std::alloc::{alloc, dealloc, Layout};
use std::rc::{Rc, Weak};
use std::sync::atomic::AtomicUsize;

use crate::block::{PageTaggedPtr, PageKind, Block};
use crate::common::{BLOCK_PER_PAGE, MASK_ARENA_BIT, Pointer};

pub struct PagePool<T> {
    pub(crate) bitfield: usize,
    pub(crate) blocks: [Block<T>; BLOCK_PER_PAGE],
    pub(crate) arena_free_list: Weak<Pointer<PagePool<T>>>,
    pub(crate) next_free: Pointer<PagePool<T>>,
    pub(crate) next: Pointer<PagePool<T>>,
    pub(crate) in_free_list: bool,
}

impl<T> PagePool<T> {
    fn allocate() -> NonNull<PagePool<T>> {
        let layout = Layout::new::<PagePool<T>>();
        unsafe {
            let page = alloc(layout) as *const PagePool<T>;
            NonNull::from(&*page)
        }
    }

    fn deallocate_page(page: *mut PagePool<T>) {
        let layout = Layout::new::<PagePool<T>>();
        unsafe {
            std::ptr::drop_in_place(&mut (*page).arena_free_list as *mut _);
            dealloc(page as *mut PagePool<T> as *mut u8, layout);
        }
    }

    fn new(
        arena_free_list: Weak<Pointer<PagePool<T>>>,
        next: *mut PagePool<T>
    ) -> NonNull<PagePool<T>>
    {
        let mut page_ptr = Self::allocate();
        let page_copy = page_ptr;

        let page = unsafe { page_ptr.as_mut() };

        // Initialize the page
        // Don't invoke any Drop here, the allocated page is uninitialized

        // We fill the bitfield with ones
        page.bitfield = !0;
        // page.next_free.set(next);
        // page.next.set(next);
        page.in_free_list = true;

        let free_ptr = &mut page.arena_free_list as *mut Weak<Pointer<PagePool<T>>>;
        unsafe {
            free_ptr.write(arena_free_list);
            // TODO: forget the old weak

            let next_free_ptr = &mut page.next_free as *mut Pointer<_>;
            let next_ptr = &mut page.next as *mut Pointer<_>;
            next_free_ptr.write(Cell::new(next));
            next_ptr.write(Cell::new(next));
        }

        // initialize the blocks
        for (index, block) in page.blocks.iter_mut().enumerate() {
            block.page = PageTaggedPtr::new(page_copy.as_ptr() as usize, index, PageKind::Pool);
            block.counter = AtomicUsize::new(0);
        }

        page_ptr
    }

    /// Make a new list of Page
    ///
    /// Returns the first and last Page in the list
    pub fn make_list(
        npages: usize,
        arena_free_list: &Rc<Pointer<PagePool<T>>>
    ) -> (NonNull<PagePool<T>>, NonNull<PagePool<T>>)
    {
        let arena_free_list = Rc::downgrade(arena_free_list);

        let last = PagePool::<T>::new(arena_free_list.clone(), std::ptr::null_mut());
        let mut previous = last;

        for _ in 0..npages - 1 {
            let previous_ptr = unsafe { previous.as_mut() };
            let page = PagePool::<T>::new(arena_free_list.clone(), previous_ptr);
            previous = page;
        }

        (previous, last)
    }

    /// Search for a free [`Block`] in the [`Page`] and mark it as non-free
    ///
    /// If there is no free block, it returns None
    pub(crate) fn acquire_free_block(&mut self) -> Option<NonNull<Block<T>>> {
        let index_free = self.bitfield.trailing_zeros() as usize;

        if index_free == BLOCK_PER_PAGE {
            return None;
        }

        // We clear the bit of the free block to mark it as non free
        self.bitfield &= !(1 << index_free);

        Some(NonNull::from(&self.blocks[index_free]))
    }

    pub(crate) fn drop_block(mut page: NonNull<PagePool<T>>, block: NonNull<Block<T>>) {
        let page_ptr = page.as_ptr();
        let page = unsafe { page.as_mut() };
        let block = unsafe { block.as_ref() };

        unsafe {
            // Drop the inner value
            std::ptr::drop_in_place(block.value.get());
        }

        let index_in_page = block.page.index_block();
        page.bitfield |= 1 << index_in_page;

        // The bit dedicated to the Page is inversed (1 for used, 0 for free)
        if !page.bitfield == MASK_ARENA_BIT {
            // We were the last block/arena referencing this page
            // Deallocate it
            PagePool::<T>::deallocate_page(page_ptr);
            return;
        }

        if !page.in_free_list {
            page.in_free_list = true;

            if let Some(arena_free_list) = page.arena_free_list.upgrade() {
                let current = arena_free_list.get();
                page.next_free.set(current);
                arena_free_list.set(page_ptr);
            };
        }
    }
}

pub(crate) fn drop_page<T>(page: *mut PagePool<T>) {
    // We clear the bit dedicated to the arena
    let new_bitfield = {
        let page = unsafe { page.as_mut().unwrap() };
        page.bitfield &= !MASK_ARENA_BIT;
        page.bitfield
    };

    if !new_bitfield == MASK_ARENA_BIT {
        // No one is referencing this page anymore (neither Arena, ArenaBox or ArenaArc)
        PagePool::<T>::deallocate_page(page);
    }
}

impl<T> Drop for PagePool<T> {
    fn drop(&mut self) {
        panic!("PAGE");
    }
}
