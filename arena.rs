

use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::*;
use std::sync::atomic::AtomicPtr;
use std::sync::Arc;
use std::pin::Pin;

use super::page::{Block, Page, BLOCK_PER_PAGE};
use super::arena_arc::ArenaArc;
use super::arena_box::ArenaBox;

pub struct Arena<T: Sized> {
    free: Arc<AtomicPtr<Page<T>>>,
    page_list: AtomicPtr<Page<T>>,
    npages: usize,
}

unsafe impl<T: Sized> Send for Arena<T> {}

impl<T: Sized> Arena<T> {
    fn make_new_page(
        npages: usize,
        arena_free_list: &Arc<AtomicPtr<Page<T>>>
    ) -> (NonNull<Page<T>>, NonNull<Page<T>>)
    {
        let last = Page::<T>::new(arena_free_list, std::ptr::null_mut());
        let mut previous = last;

        for _ in 0..npages - 1 {
            let previous_ptr = unsafe { previous.as_mut() };
            let page = Page::<T>::new(arena_free_list, previous_ptr);
            previous = page;
        }

        (previous, last)
    }

    fn alloc_new_page(&mut self) -> NonNull<Page<T>> {
        let len = self.npages;

        let to_allocate = len.min(900_000);

        let (mut first, mut last) = Self::make_new_page(to_allocate, &self.free);

        let (first_ref, last_ref) = unsafe {
            (first.as_mut(), last.as_mut())
        };

        loop {
            let current = self.free.load(Relaxed);
            last_ref.next_free = AtomicPtr::new(current);

            if self.free.compare_exchange(
                current, first_ref, Release, Relaxed
            ).is_ok() {
                break;
            }
        }

        last_ref.next = AtomicPtr::new(self.page_list.load(Relaxed));
        self.page_list.store(first_ref, Relaxed);

        self.npages += to_allocate;

        first
    }

    #[inline(never)]
    fn find_place(&mut self) -> (NonNull<Page<T>>, NonNull<Block<T>>) {
        while let Some(page) = unsafe { self.free.load(Acquire).as_mut() } {

            if let Some(block) = page.acquire_free_block() {
                return (NonNull::from(page), block);
            }

            let next = page.next_free.load(Relaxed);

            // self.free might have changed here (with an ArenaBox/Arc drop)
            // We remove the page from the free list only when it didn't change
            if self.free.compare_exchange(page, next, AcqRel, Relaxed).is_ok() {
                page.in_free_list.store(false, Release);
            }
        }

        println!("ALLOCATE MORE", );

        let new_page = self.alloc_new_page();
        let block = unsafe { new_page.as_ref() }.acquire_free_block().unwrap();

        (new_page, block)
    }

    pub fn with_capacity(cap: usize) -> Arena<T> {
        let npages = ((cap.max(1) - 1) / BLOCK_PER_PAGE) + 1;
        let free = Arc::new(AtomicPtr::new(std::ptr::null_mut()));

        let (mut first, _) = Self::make_new_page(npages, &free);
        let first_ref = unsafe { first.as_mut() };

        free.as_ref().store(first_ref, Relaxed);

        Arena {
            npages,
            free,
            page_list: AtomicPtr::new(first_ref)
        }
    }

    pub fn new() -> Arena<T> {
        Arena::with_capacity(BLOCK_PER_PAGE)
    }

    pub fn alloc(&mut self, value: T) -> ArenaBox<T> {
        let (page, block) = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            ptr.write(value);
            ArenaBox::new(page, block)
        }
    }

    pub fn alloc_in_place<F>(&mut self, initializer: F) -> ArenaBox<T>
    where
        F: Fn(&mut MaybeUninit<T>)
    {
        let (page, block) = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            initializer(&mut *(ptr as *mut std::mem::MaybeUninit<T>));
            ArenaBox::new(page, block)
        }
    }

    pub fn alloc_arc(&mut self, value: T) -> ArenaArc<T> {
        let (page, block) = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            ptr.write(value);
            ArenaArc::new(page, block)
        }
    }

    // pub fn npages(&self) -> usize {
    //     self.pages.len()
    // }

    pub fn stats(&self) -> (usize, usize) {
        let mut next = self.free.load(Relaxed);

        let mut free = 0;

        while let Some(next_ref) = unsafe { next.as_mut() } {
            let next_next = next_ref.next_free.load(Relaxed);
            free += next_ref.bitfield.load(Relaxed).count_ones() as usize - 1;
            next = next_next;
        }

        let used = (self.npages * BLOCK_PER_PAGE) - free;
        // let used = self
        //     .pages
        //     .iter()
        //     .map(|p| unsafe { p.as_ref() }.bitfield.load(Relaxed).count_zeros() as usize)
        //     .sum::<usize>();

        // // We don't count bits dedicated to the pages
        // used - self.pages.len()
        (used, free)
    }
}

impl<T> Drop for Arena<T> {
    fn drop(&mut self) {
        let mut next = self.page_list.load(Relaxed);

        while let Some(next_ref) = unsafe { next.as_mut() } {
            let next_next = next_ref.next.load(Relaxed);
            unsafe {
                std::ptr::drop_in_place(next);
            }
            next = next_next;
        }
    }
}

impl<T: Sized> Default for Arena<T> {
    fn default() -> Arena<T> {
        Arena::new()
    }
}

impl<T> std::fmt::Debug for Arena<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        // let len = self.pages.len();
        f.debug_struct("MemPool")
         // .field("npages", &len)
         .finish()
    }
}
