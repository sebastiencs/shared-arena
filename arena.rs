

use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::*;
use std::sync::atomic::AtomicPtr;
use std::pin::Pin;

use super::page::{Block, Page, BLOCK_PER_PAGE};
use super::circular_iter::CircularIterator;
use super::arena_arc::ArenaArc;
use super::arena_box::ArenaBox;


pub struct Arena<T: Sized> {
    free: Pin<Box<AtomicPtr<Page<T>>>>,
    page_list: AtomicPtr<Page<T>>,
    npages: usize,
}

unsafe impl<T: Sized> Send for Arena<T> {}

impl<T: Sized> Arena<T> {
    fn make_new_page(
        npages: usize,
        arena_free_list: &AtomicPtr<Page<T>>
    ) -> (NonNull<Page<T>>, NonNull<Page<T>>)
    {
        let mut last = Page::<T>::new(arena_free_list);
        let mut previous = last;

        for _ in 0..npages - 1 {
            let mut page = Page::<T>::new(arena_free_list);
            let page_ref = unsafe { page.as_mut() };
            page_ref.next_free = AtomicPtr::new(unsafe { previous.as_mut() });
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

        self.npages += to_allocate;

        first
    }

    fn find_place(&mut self) -> (NonNull<Page<T>>, NonNull<Block<T>>) {
        loop {
            let page = match unsafe { self.free.load(Acquire).as_ref() } {
                Some(page) => page,
                _ => break
            };

            if let Some(block) = page.acquire_free_block() {
                return (NonNull::from(page), block);
            }

            page.in_free_list.store(false, Release);
            self.free.store(page.next_free.load(Acquire), Release);
        }

        // println!("ALLOCATE MORE", );

        let new_page = self.alloc_new_page();
        let block = unsafe { new_page.as_ref() }.acquire_free_block().unwrap();

        (new_page, block)
    }

    pub fn with_capacity(cap: usize) -> Arena<T> {
        let npages = ((cap.max(1) - 1) / BLOCK_PER_PAGE) + 1;
        let free = Box::pin(AtomicPtr::new(std::ptr::null_mut()));

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

    pub fn stats(&self) -> usize {
        // let used = self
        //     .pages
        //     .iter()
        //     .map(|p| unsafe { p.as_ref() }.bitfield.load(Relaxed).count_zeros() as usize)
        //     .sum::<usize>();

        // // We don't count bits dedicated to the pages
        // used - self.pages.len()
        0
    }
}

impl<T> Drop for Arena<T> {
    fn drop(&mut self) {
        // for ptr in self.pages.iter_mut() {
        //     unsafe { std::ptr::drop_in_place(ptr.as_mut()) };
        // }
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
