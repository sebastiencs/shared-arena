
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::*;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize};
use std::sync::Arc;

use super::page::{Block, Page, BLOCK_PER_PAGE};
use super::arena_arc::ArenaArc;
use super::arena_box::ArenaBox;

pub struct SharedArena<T: Sized> {
    free: Arc<AtomicPtr<Page<T>>>,
    page_list: AtomicPtr<Page<T>>,
    npages: AtomicUsize,
    writer: AtomicBool,
    shrinking: AtomicBool,
}

unsafe impl<T: Sized> Send for SharedArena<T> {}
unsafe impl<T: Sized> Sync for SharedArena<T> {}

struct WriterGuard<'a> {
    writer: &'a AtomicBool
}

impl WriterGuard<'_> {
    fn new(writer: &AtomicBool) -> Option<WriterGuard> {
        writer.compare_exchange(false, true, AcqRel, Relaxed)
              .map(|_| WriterGuard { writer })
              .ok()
    }
}

impl Drop for WriterGuard<'_> {
    fn drop(&mut self) {
        self.writer.store(false, Release);
    }
}

impl<T: Sized> SharedArena<T> {

    /// Ensure that a single thread is running `fun` at a time
    fn with_single_writer(&self, fun: impl Fn()) {
        let _writer_guard = match WriterGuard::new(&self.writer) {
            Some(guard) => guard,
            _ => return
        };

        if !self.free.load(Acquire).is_null() {
            // self.pages has already been updated
            return;
        }

        fun();
    }

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

    fn alloc_new_page(&self) -> NonNull<Page<T>> {
        let len = self.npages.load(Relaxed);

        let to_allocate = len.max(1).min(900_000);

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

        self.npages.fetch_add(to_allocate, Relaxed);

        first
    }

    fn find_place(&self) -> (NonNull<Page<T>>, NonNull<Block<T>>) {
        loop {
            while let Some(page) = unsafe { self.free.load(Acquire).as_mut() } {

                if let Some(block) = page.acquire_free_block() {
                    return (NonNull::from(page), block);
                }

                let next = page.next_free.load(Relaxed);

                // self.free might have changed here
                if self.free.compare_exchange(page, next, AcqRel, Relaxed).is_ok() {
                    page.in_free_list.store(false, Release);
                }
            }

            self.with_single_writer(|| {
                self.alloc_new_page();
            });
        }
    }

    pub fn with_capacity(cap: usize) -> SharedArena<T> {
        let npages = ((cap.max(1) - 1) / BLOCK_PER_PAGE) + 1;
        let free = Arc::new(AtomicPtr::new(std::ptr::null_mut()));

        let (mut first, _) = Self::make_new_page(npages, &free);
        let first_ref = unsafe { first.as_mut() };

        free.as_ref().store(first_ref, Relaxed);

        SharedArena {
            npages: AtomicUsize::new(npages),
            free,
            page_list: AtomicPtr::new(first_ref),
            writer: AtomicBool::new(false),
            shrinking: AtomicBool::new(false)
        }
    }

    pub fn new() -> SharedArena<T> {
        SharedArena::with_capacity(BLOCK_PER_PAGE)
    }

    pub fn alloc(&self, value: T) -> ArenaBox<T> {
        let (page, block) = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            ptr.write(value);
        }

        ArenaBox::new(page, block)
    }

    pub fn alloc_in_place<F>(&self, initializer: F) -> ArenaBox<T>
    where
        F: Fn(&mut MaybeUninit<T>)
    {
        let (page, block) = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            initializer(&mut *(ptr as *mut std::mem::MaybeUninit<T>));
        }

        ArenaBox::new(page, block)
    }

    pub fn alloc_arc(&self, value: T) -> ArenaArc<T> {
        let (page, block) = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            ptr.write(value);
        }

        ArenaArc::new(page, block)
    }

    pub fn alloc_in_place_arc<F>(&self, initializer: F) -> ArenaArc<T>
    where
        F: Fn(&mut MaybeUninit<T>)
    {
        let (page, block) = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            initializer(&mut *(ptr as *mut std::mem::MaybeUninit<T>));
        }

        ArenaArc::new(page, block)
    }

    pub fn shrink_to_fit(&self) {
        if self.shrinking.compare_exchange(
            false, true, SeqCst, Relaxed
        ).is_err() {
            return;
        }

        let mut current: &AtomicPtr<Page<T>> = &*self.free;

        let mut free_pages = vec![];

        // We loop on the free list to get all pages that have 0 reference to
        // them and remove them from the free list
        while let Some(current_value) = unsafe { current.load(Relaxed).as_mut() } {
            let next = &current_value.next_free;
            let next_value = next.load(Relaxed);

            if !current_value.bitfield.load(Relaxed) == 0 {
                if current.compare_exchange(
                    current_value as *const _ as *mut _, next_value, AcqRel, Relaxed
                ).is_err() {
                    continue;
                }
                free_pages.push(current_value as *const _ as *mut Page<T>);
            } else {
                current = next;
            }
        }

        let mut to_drop = Vec::with_capacity(free_pages.len());

        // We check that the pages still have 0 reference to them
        // because it's possible that another thread have updated the bitfield
        // between the moment when we read the bitfield and the moment we removed
        // the page from the free list.
        // If the page and its bitfield have been updated, we put it again in
        // the free list
        for page in &free_pages {
            let page_ref = unsafe { page.as_ref().unwrap() };

            if !page_ref.bitfield.load(Relaxed) == 0 {
                to_drop.push(*page);
            } else {
                'cas: loop {
                    let current = self.free.load(Relaxed);
                    page_ref.next_free.store(current, Relaxed);

                    if self.free.compare_exchange_weak(
                        current, *page, Release, Relaxed
                    ).is_ok() {
                        break 'cas;
                    }
                }
            }
        }

        // Now we are 100% sure that pages in to_drop are/will not be referenced
        // anymore

        let mut current: &AtomicPtr<Page<T>> = &self.page_list;

        // Loop on the full list
        // We remove the pages from it
        while let Some(current_value) = unsafe { current.load(Relaxed).as_mut() } {
            let next = &current_value.next;
            let next_value = next.load(Relaxed);

            if to_drop.contains(&(current_value as *const _ as *mut Page<T>)) {
                if current.compare_exchange(
                    current_value as *const _ as *mut _, next_value, AcqRel, Relaxed
                ).is_err() {
                    continue;
                }
            } else {
                current = next;
            }
        }

        self.npages.fetch_sub(to_drop.len(), Relaxed);

        for page in to_drop.iter().rev() {
            // Invoke Page::drop and deallocate it
            unsafe { std::ptr::drop_in_place(*page) }
        }

        self.shrinking.store(false, Relaxed);
    }

    pub fn stats(&self) -> (usize, usize) {
        let mut next = self.free.load(Relaxed);

        let mut free = 0;

        while let Some(next_ref) = unsafe { next.as_mut() } {
            let next_next = next_ref.next_free.load(Relaxed);
            free += next_ref.bitfield.load(Relaxed).count_ones() as usize - 1;
            next = next_next;
        }

        let used = (self.npages.load(Relaxed) * BLOCK_PER_PAGE) - free;

        (used, free)
    }

    #[cfg(test)]
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

    #[cfg(test)]
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

impl<T> Drop for SharedArena<T> {
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

impl<T: Sized> Default for SharedArena<T> {
    fn default() -> SharedArena<T> {
        SharedArena::new()
    }
}

impl<T> std::fmt::Debug for SharedArena<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        struct Page {
            free: usize,
            used: usize,
        }

        impl std::fmt::Debug for Page {
            fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "Page {{ free: {} used: {} }}", self.free, self.used)
            }
        }

        let npages = self.npages.load(Relaxed);

        let mut vec = Vec::with_capacity(npages);

        let mut next = self.page_list.load(Relaxed);

        while let Some(next_ref) = unsafe { next.as_mut() } {
            let used = next_ref.bitfield.load(Relaxed).count_zeros() as usize;
            vec.push(Page {
                used,
                free: BLOCK_PER_PAGE - used
            });

            next = next_ref.next.load(Relaxed);
        }

        let blocks_used: usize = vec.iter().map(|p| p.used).sum();
        let blocks_free: usize = vec.iter().map(|p| p.free).sum();

        f.debug_struct("SharedArena")
         .field("blocks_free", &blocks_free)
         .field("blocks_used", &blocks_used)
         .field("npages", &npages)
         .field("pages", &vec)
         .finish()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn arena_shrink() {
        let mut arena = super::SharedArena::<usize>::with_capacity(1000);
        assert_eq!(arena.stats(), (0, 1008));
        arena.shrink_to_fit();
        assert_eq!(arena.stats(), (0, 0));
    }

    #[test]
    fn arena_shrink2() {
        let mut arena = super::SharedArena::<usize>::with_capacity(1000);

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
        let mut arena = super::SharedArena::<usize>::with_capacity(1000);

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

        let a = arena.alloc(1);
        assert_eq!(arena.size_lists(), (1, 1));
    }
}
