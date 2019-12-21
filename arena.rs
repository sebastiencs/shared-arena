
use std::sync::Arc;
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::*;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
use std::slice;

use super::page::{Block, Page, BLOCK_PER_PAGE};
use super::arena_arc::ArenaArc;
use super::arena_box::ArenaBox;
use crate::cache_line::CacheAligned;

pub struct Arena<T: Sized> {
    last_found: usize,
    pages: Vec<NonNull<Page<T>>>,
}

unsafe impl<T: Sized> Send for Arena<T> {}

impl<T: Sized> Arena<T> {
    #[inline(never)]
    fn alloc_new_page(&mut self) -> NonNull<Page<T>> {
        let len = self.pages.len();
        let new_len = len + len.min(900_000);

        self.pages.resize_with(new_len, Page::<T>::new);

        self.pages[len]
    }

    #[inline(never)]
    fn find_place(&mut self) -> (NonNull<Page<T>>, NonNull<Block<T>>) {
        let pages_len = self.pages.len();

        let last_found = self.last_found % pages_len;

        let (before, after) = self.pages.split_at(last_found);

        for (index, page) in after.iter().chain(before).enumerate() {
            if let Some(block) = unsafe { page.as_ref() }.acquire_free_block() {
                self.last_found = last_found + index;
                return (*page, block);
            };
        }

        println!("ALLOCATING MORE {} {} {:?}", self.pages.len(), self.pages.len() * 32, self.stats());

        self.last_found = self.pages.len();

        let new_page = self.alloc_new_page();
        let node = unsafe { new_page.as_ref() }.acquire_free_block().unwrap();

        (new_page, node)
    }

    pub fn with_capacity(cap: usize) -> Arena<T> {
        let npages = ((cap.max(1) - 1) / BLOCK_PER_PAGE) + 1;

        let mut pages = Vec::with_capacity(npages);
        pages.resize_with(npages, Page::<T>::new);

        Arena { last_found: 0, pages }
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

    pub fn npages(&self) -> usize {
        self.pages.len()
    }

    pub fn stats(&self) -> usize {
        let used = self
            .pages
            .iter()
            .map(|p| unsafe { p.as_ref() }.bitfield.load(Relaxed).count_zeros() as usize)
            .sum::<usize>();

        // We don't count bits dedicated to the pages
        used - self.pages.len()
    }
}

impl<T> Drop for Arena<T> {
    fn drop(&mut self) {
        for ptr in self.pages.iter_mut() {
            unsafe { std::ptr::drop_in_place(ptr.as_mut()) };
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
        let len = self.pages.len();
        f.debug_struct("MemPool")
         .field("npages", &len)
         .finish()
    }
}
