

use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::*;
use std::sync::atomic::AtomicPtr;
use std::sync::Arc;

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

    pub fn shrink_to_fit(&mut self) {

        let mut current: &AtomicPtr<Page<T>> = &*self.free;

        let mut to_drop = vec![];

        // Loop on the free list
        while let Some(current_value) = unsafe { current.load(Relaxed).as_mut() } {
            let next = &current_value.next_free;
            let next_value = next.load(Relaxed);

            if !current_value.bitfield.load(Relaxed) == 0 {
                current.store(next_value, Relaxed);
                to_drop.push(current_value as *const _ as *mut Page<T>);
            } else {
                current = next;
            }
        }

        let mut current: &AtomicPtr<Page<T>> = &self.page_list;

        // Loop on the full list
        while let Some(current_value) = unsafe { current.load(Relaxed).as_mut() } {
            let next = &current_value.next;
            let next_value = next.load(Relaxed);

            if to_drop.contains(&(current_value as *const _ as *mut Page<T>)) {
                current.store(next_value, Relaxed);
            } else {
                current = next;
            }
        }

        self.npages -= to_drop.len();

        for page in to_drop.iter().rev() {
            unsafe { std::ptr::drop_in_place(*page) }
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

    pub(crate) fn size_lists(&self) -> (usize, usize) {
        let mut next = self.page_list.load(Relaxed);
        let mut size = 0;
        while let Some(next_ref) = unsafe { next.as_mut() } {
            next = next_ref.next.load(Relaxed);
            size += 1;
        }

        let mut next = self.free.load(Relaxed);
        let mut free = 0;
        while let Some(next_ref) = unsafe { next.as_mut() } {
            next = next_ref.next_free.load(Relaxed);
            free += 1;
        }

        (size, free)
    }

    pub(crate) fn display_list(&self) {
        let mut full = vec![];

        let mut next = self.page_list.load(Relaxed);
        while let Some(next_ref) = unsafe { next.as_mut() } {
            full.push(next);
            next = next_ref.next.load(Relaxed);
        }

        let mut list_free = vec![];

        let mut next = self.free.load(Relaxed);
        while let Some(next_ref) = unsafe { next.as_mut() } {
            list_free.push(next);
            next = next_ref.next_free.load(Relaxed);
        }

        println!("FULL {} {:#?}", full.len(), full);
        println!("FREE {} {:#?}", list_free.len(), list_free);
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

#[cfg(test)]
mod tests {
    #[test]
    fn arena_shrink() {
        let mut arena = super::Arena::<usize>::with_capacity(1000);
        assert_eq!(arena.stats(), (0, 1008));
        arena.shrink_to_fit();
        assert_eq!(arena.stats(), (0, 0));
    }

    #[test]
    fn arena_shrink2() {
        let mut arena = super::Arena::<usize>::with_capacity(1000);

        let _a = arena.alloc(1);
        arena.shrink_to_fit();
        assert_eq!(arena.stats(), (1, 62));

        let _a = arena.alloc(1);
        arena.shrink_to_fit();
        assert_eq!(arena.stats(), (2, 61));

        let mut values = Vec::with_capacity(64);
        for _ in 0..64 {
            values.push(arena.alloc(1));
        }

        assert_eq!(arena.stats(), (66, 60));
        arena.shrink_to_fit();
        assert_eq!(arena.stats(), (66, 60));

        std::mem::drop(values);

        assert_eq!(arena.stats(), (2, 124));
        arena.shrink_to_fit();
        assert_eq!(arena.stats(), (2, 61));
    }

    #[test]
    fn arena_size() {
        let mut arena = super::Arena::<usize>::with_capacity(1000);

        assert_eq!(arena.size_lists(), (16, 16));
        let a = arena.alloc(1);
        assert_eq!(arena.size_lists(), (16, 16));

        let mut values = Vec::with_capacity(539);
        for _ in 0..539 {
            values.push(arena.alloc(1));
        }
        assert_eq!(arena.size_lists(), (16, 8));

        arena.shrink_to_fit();

        assert_eq!(arena.size_lists(), (9, 1));

        values.truncate(503);
        arena.shrink_to_fit();

        assert_eq!(arena.size_lists(), (8, 0));

        std::mem::drop(a);
        for _ in 0..62 {
            values.remove(0);
        }

        assert_eq!(arena.size_lists(), (8, 1));

        arena.shrink_to_fit();
        assert_eq!(arena.size_lists(), (7, 0));

        values.clear();
        assert_eq!(arena.size_lists(), (7, 7));

        arena.shrink_to_fit();
        assert_eq!(arena.size_lists(), (0, 0));
    }
}
