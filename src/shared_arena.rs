
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::*;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize};
use std::sync::Arc;

use super::page::{Block, Page, BLOCK_PER_PAGE, drop_page};
use super::arena_arc::ArenaArc;
use super::arena_box::ArenaBox;

/// An arena shareable across threads
///
/// Pointers to the elements in the SharedArena are shareable as well.
pub struct SharedArena<T: Sized> {
    free_list: AtomicPtr<Page<T>>,
    pending_free_list: Arc<AtomicPtr<Page<T>>>,
    full_list: AtomicPtr<Page<T>>,
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
        if !writer.load(Relaxed) && !writer.swap(true, AcqRel) {
            Some(WriterGuard { writer })
        } else {
            None
        }
    }

    fn new_blocking(writer: &AtomicBool) -> WriterGuard {
        loop {
            if !writer.swap(true, AcqRel) {
                return WriterGuard { writer }
            }
            std::thread::yield_now();
        }
    }
}

impl Drop for WriterGuard<'_> {
    fn drop(&mut self) {
        self.writer.store(false, Release);
    }
}

impl<T: Sized> SharedArena<T> {
    fn alloc_new_page(&self) {
        let to_allocate = self.npages
                              .load(Relaxed)
                              .max(1)
                              .min(900_000);

        let (first, mut last) = Page::make_list(to_allocate, &self.pending_free_list);

        let first_ptr = first.as_ptr();
        let last_ref = unsafe { last.as_mut() };

        // We have to touch self.page_list before self.free because
        // shrink_to_fit might be running at the same time with another
        // thread.
        // If we update self.page_list _after_ self.free shrink_to_fit
        // will try to remove pages that are not yet in self.page_list

        let current = self.full_list.load(Relaxed);
        last_ref.next = AtomicPtr::new(current);
        self.full_list.swap(first_ptr, AcqRel);

        let current = self.free_list.load(Relaxed);
        assert!(current.is_null(), "Arena.free isn't null");

        let old = self.free_list.swap(first_ptr, AcqRel);
        assert!(old.is_null(), "Arena.free2 isn't null");

        self.npages.fetch_add(to_allocate, Relaxed);
    }

    fn find_place(&self) -> (NonNull<Page<T>>, NonNull<Block<T>>) {
        loop {
            while let Some(page) = unsafe { self.free_list.load(Acquire).as_mut() } {

                if let Some(block) = page.acquire_free_block() {
                    return (NonNull::from(page), block);
                }

                // No free block on the page, we remove it from the free list

                let next = page.next_free.load(Acquire);
                if self.free_list.compare_exchange(page, next, AcqRel, Relaxed).is_ok() {
                    // The page might be not full anymore since the call to
                    // acquire_free_block but that's fine because drops of
                    // an ArenaBox/Arc on that page will insert the page on
                    // self.pending_free.
                    // page.in_free_list.store(false, Release);

                    page.in_free_list.store(false, Release);
                }
            }

            // TODO: See how to reduce branches here
            if let Some(_guard) = WriterGuard::new(&self.writer) {
                if self.free_list.load(Acquire).is_null() {
                    // A single and only thread run this code at a time.

                    let pending = self.pending_free_list.load(Relaxed);
                    if !pending.is_null() {
                        // Move self.pending_free to self.free.

                        let pending = self.pending_free_list.swap(std::ptr::null_mut(), AcqRel);
                        let old = self.free_list.swap(pending, Release);
                        assert!(old.is_null(), "NOT NULL");
                    } else {
                        // No pages in self.pending_free. We allocate new pages.

                        self.alloc_new_page();
                    }
                }
                continue;
            };

            // This block is reached if an another thread is allocating or replacing
            // self.pending_free (the block just above).
            // Since allocating might take a while, some page might become free during
            // this time.
            // So instead of looping on self.free (which will stay null until allocation
            // is done), we check for pages on self.pending_free.

            let mut next = unsafe { self.pending_free_list.load(Relaxed).as_mut() };

            while let Some(page) = next {
                if let Some(block) = page.acquire_free_block() {
                    return (NonNull::from(page), block);
                }

                let next_free = page.next_free.load(Acquire);
                if self.pending_free_list.compare_exchange(page, next_free, AcqRel, Relaxed).is_ok() {
                    page.in_free_list.store(false, Release);
                }

                next = unsafe { page.next_free.load(Relaxed).as_mut() };
            }
        }
    }

    /// Constructs a new SharedArena capable of holding at least `cap` elements
    ///
    /// Because the arena allocate by page of 63 elements, it might be able to
    /// hold more elements than `cap`.
    ///
    /// The Arena will reallocate itself if there is not enough space
    /// when allocating (with alloc* functions)
    ///
    /// ## Example
    ///
    /// ```ignore
    /// use shared_arena::SharedArena;
    ///
    /// let arena = SharedArena::with_capacity(2048);
    /// ```
    pub fn with_capacity(cap: usize) -> SharedArena<T> {
        let npages = ((cap.max(1) - 1) / BLOCK_PER_PAGE) + 1;
        let pending_free = Arc::new(AtomicPtr::new(std::ptr::null_mut()));

        let (first, _) = Page::make_list(npages, &pending_free);

        SharedArena {
            npages: AtomicUsize::new(npages),
            free_list: AtomicPtr::new(first.as_ptr()),
            pending_free_list: pending_free,
            full_list: AtomicPtr::new(first.as_ptr()),
            writer: AtomicBool::new(false),
            shrinking: AtomicBool::new(false)
        }
    }

    /// Constructs a new SharedArena capable of holding exactly 63 elements
    ///
    /// The Arena will reallocate itself if there is not enough space
    /// when allocating (with alloc* functions)
    ///
    /// ## Example
    ///
    /// ```ignore
    /// use shared_arena::SharedArena;
    ///
    /// let arena = SharedArena::new();
    /// ```
    pub fn new() -> SharedArena<T> {
        SharedArena::with_capacity(BLOCK_PER_PAGE)
    }

    /// Writes a value in the arena, and returns an [`ArenaBox`]
    /// pointing to that value.
    ///
    /// ## Example
    ///
    /// ```
    /// use shared_arena::{ArenaBox, SharedArena};
    ///
    /// let arena = SharedArena::new();
    /// let my_num: ArenaBox<u8> = arena.alloc(0xFF);
    /// ```
    ///
    /// [`ArenaBox`]: ./struct.ArenaBox.html
    pub fn alloc(&self, value: T) -> ArenaBox<T> {
        let (page, block) = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            ptr.write(value);
        }

        ArenaBox::new(page, block)
    }

    /// Finds an empty space in the arena and calls the function `initializer`
    /// with its argument pointing to that space.
    /// It returns an [`ArenaBox`] pointing to the newly initialized value.
    ///
    /// The difference with [`alloc`] is that it has the benefit of
    /// avoiding intermediate copies of the value.
    ///
    /// ## Safety
    ///
    /// It is the caller responsability to initialize properly the value.
    /// When the [`ArenaBox`] is dropped, the value is also
    /// dropped. If the value is not initialized correctly, it will
    /// drop an unitialized value, which is undefined behavior.
    ///
    /// This function is not marked as `unsafe` because the caller will have
    /// to deal itself with [`MaybeUninit`].
    ///
    /// A bad usage of this function is `unsafe` and can lead to undefined
    /// behavior !
    ///
    /// ## Example
    ///
    /// ```
    /// use shared_arena::{ArenaBox, SharedArena};
    /// use std::ptr;
    ///
    /// #[derive(Copy, Clone)]
    /// struct MyStruct { }
    ///
    /// let ref_struct: &MyStruct = &MyStruct{};
    ///
    /// let arena = SharedArena::new();
    /// let my_struct: ArenaBox<MyStruct> = arena.alloc_in_place(|place| {
    ///     unsafe {
    ///         // The type must be Copy to use ptr::copy
    ///         ptr::copy(ref_struct, place.as_mut_ptr(), 1);
    ///     }
    /// });
    /// ```
    ///
    /// [`ArenaBox`]: ./struct.ArenaBox.html
    /// [`alloc`]: struct.SharedArena.html#method.alloc
    /// [`MaybeUninit`]: https://doc.rust-lang.org/std/mem/union.MaybeUninit.html
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

    /// Writes a value in the arena, and returns an [`ArenaArc`]
    /// pointing to that value.
    ///
    /// ## Example
    ///
    /// ```
    /// use shared_arena::{ArenaArc, SharedArena};
    ///
    /// let arena = SharedArena::new();
    /// let my_num: ArenaArc<u8> = arena.alloc_arc(0xFF);
    /// ```
    ///
    /// [`ArenaArc`]: ./struct.ArenaArc.html
    pub fn alloc_arc(&self, value: T) -> ArenaArc<T> {
        let (page, block) = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            ptr.write(value);
        }

        ArenaArc::new(page, block)
    }

    /// Finds an empty space in the arena and calls the function `initializer`
    /// with its argument pointing to that space.
    /// It returns an [`ArenaArc`] pointing to the newly initialized value.
    ///
    /// The difference with [`alloc_arc`] is that it has the benefit of
    /// avoiding intermediate copies of the value.
    ///
    /// ## Safety
    ///
    /// It is the caller responsability to initialize properly the value.
    /// When all [`ArenaArc`] pointing that value are dropped, the value
    /// is also dropped. If the value is not initialized correctly, it will
    /// drop an unitialized value, which is undefined behavior.
    ///
    /// This function is not marked as `unsafe` because the caller will have
    /// to deal itself with [`MaybeUninit`].
    ///
    /// A bad usage of this function is `unsafe` and can lead to undefined
    /// behavior !
    ///
    /// ## Example
    ///
    /// ```
    /// use shared_arena::{ArenaArc, SharedArena};
    /// use std::ptr;
    ///
    /// #[derive(Copy, Clone)]
    /// struct MyStruct {}
    ///
    /// let ref_struct: &MyStruct = &MyStruct {};
    ///
    /// let arena = SharedArena::new();
    /// let my_struct: ArenaArc<MyStruct> = arena.alloc_in_place_arc(|place| {
    ///     unsafe {
    ///         // The type must be Copy to use ptr::copy
    ///         ptr::copy(ref_struct, place.as_mut_ptr(), 1);
    ///     }
    /// });
    /// ```
    ///
    /// [`ArenaArc`]: ./struct.ArenaArc.html
    /// [`alloc_arc`]: #method.alloc_arc
    /// [`MaybeUninit`]: https://doc.rust-lang.org/std/mem/union.MaybeUninit.html
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

    /// Shrinks the capacity of the arena as much as possible.
    ///
    /// It will drop all pages that are unused (no ArenaBox/ArenaArc point to
    /// it)
    /// If there is still one or more references to a page, the page won't
    /// be dropped.
    ///
    /// ## Example
    ///
    /// ```
    /// use shared_arena::SharedArena;
    ///
    /// let mut arena = SharedArena::with_capacity(2048);
    /// let mut values = Vec::new();
    ///
    /// let (used, free) = arena.stats();
    /// assert!(used == 0, free == 2048);
    ///
    /// for _ in 0..80 {
    ///     values.push(arena.alloc(0xFF));
    /// }
    ///
    /// arena.shrink_to_fit();
    ///
    /// let (used, free) = arena.stats();
    /// assert!(used == 80, free == 46);
    ///
    /// ```
    pub fn shrink_to_fit(&self) -> bool {
        if self.shrinking.swap(true, AcqRel) {
            return false;
        }

        let _guard = WriterGuard::new_blocking(&self.writer);

        let mut current: &AtomicPtr<Page<T>> = &AtomicPtr::new(self.free_list.swap(std::ptr::null_mut(), AcqRel));
        let start = current;

        let mut to_drop = Vec::with_capacity(self.npages.load(Relaxed));

        // We loop on the free list to get all pages that have 0 reference to
        // them and remove them from the free list
        while let Some(current_value) = unsafe { current.load(Relaxed).as_mut() } {
            let next = &current_value.next_free;
            let next_value = next.load(Relaxed);

            if current_value.bitfield.load(Acquire) == !0 {
                if current.compare_exchange(
                    current_value as *const _ as *mut _, next_value, AcqRel, Relaxed
                ).is_ok() {
                    to_drop.push(current_value as *const _ as *mut Page<T>);
                }
            } else {
                current = next;
            }
        }

        // Now we are 100% sure that pages in to_drop are/will not be
        // referenced anymore

        let mut current: &AtomicPtr<Page<T>> = &self.full_list;

        // Loop on the full list
        // We remove the pages from it
        while let Some(current_value) = unsafe { current.load(Relaxed).as_mut() } {
            let next = &current_value.next;
            let next_value = next.load(Relaxed);

            if to_drop.contains(&(current_value as *const _ as *mut Page<T>)) {
                current.compare_exchange(
                    current_value as *const _ as *mut _, next_value, AcqRel, Relaxed
                ).expect("Something went wrong in shrinking.");
            } else {
                current = next;
            }
        }

        for page in &to_drop {
            let page_ref = unsafe { page.as_ref().unwrap() };

            assert!(page_ref.bitfield.load(Acquire) == !0);

            drop_page(*page);
        }
        let old = self.free_list.swap(start.load(Relaxed), Release);
        assert!(old.is_null(), "OLD NOT NULL");

        self.npages.fetch_sub(to_drop.len(), Release);

        self.shrinking.store(false, Release);

        return true;
    }

    /// Returns a tuple of non-free and free spaces in the arena
    ///
    /// ## Example
    ///
    /// ```
    /// use shared_arena::SharedArena;
    ///
    /// let arena = SharedArena::new();
    /// let item = arena.alloc(1);
    /// let (used, free) = arena.stats();
    /// assert!(used == 1 && free == 62);
    /// ```
    pub fn stats(&self) -> (usize, usize) {
        let mut next = self.free_list.load(Relaxed);

        let mut free = 0;

        while let Some(next_ref) = unsafe { next.as_mut() } {
            let next_next = next_ref.next_free.load(Relaxed);
            free += next_ref.bitfield.load(Relaxed).count_ones() as usize - 1;
            next = next_next;
        }

        let mut next = self.pending_free_list.load(Relaxed);

        while let Some(next_ref) = unsafe { next.as_mut() } {
            let next_next = next_ref.next_free.load(Relaxed);
            free += next_ref.bitfield.load(Relaxed).count_ones() as usize - 1;
            next = next_next;
        }

        let used = (self.npages.load(Relaxed) * BLOCK_PER_PAGE) - free;

        (used, free)
    }

    #[cfg(test)]
    pub(crate) fn size_lists(&self) -> (usize, usize, usize) {
        let mut next = self.full_list.load(Relaxed);
        let mut size = 0;
        while let Some(next_ref) = unsafe { next.as_mut() } {
            next = next_ref.next.load(Relaxed);
            size += 1;
        }

        let mut next = self.free_list.load(Relaxed);
        let mut free = 0;
        while let Some(next_ref) = unsafe { next.as_mut() } {
            next = next_ref.next_free.load(Relaxed);
            free += 1;
        }

        let mut next = self.pending_free_list.load(Relaxed);
        let mut pending = 0;
        while let Some(next_ref) = unsafe { next.as_mut() } {
            next = next_ref.next_free.load(Relaxed);
            pending += 1;
        }

        (size, free, pending)
    }

    #[allow(dead_code)]
    #[cfg(test)]
    pub(crate) fn display_list(&self) {
        let mut full = vec![];

        let mut next = self.full_list.load(Relaxed);
        while let Some(next_ref) = unsafe { next.as_mut() } {
            full.push(next);
            next = next_ref.next.load(Relaxed);
        }

        let mut list_free = vec![];

        let mut next = self.free_list.load(Relaxed);
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
        let mut next = self.full_list.load(Relaxed);

        while let Some(next_ref) = unsafe { next.as_mut() } {
            let next_next = next_ref.next.load(Relaxed);
            // unsafe {
                // Invoke Page::drop
                drop_page(next);
                // std::ptr::drop_in_place(next);
            // }
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

        let mut next = self.full_list.load(Relaxed);

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
    use super::SharedArena;

    #[test]
    fn arena_shrink() {
        let arena = super::SharedArena::<usize>::with_capacity(1000);
        assert_eq!(arena.stats(), (0, 1008));
        arena.shrink_to_fit();
        assert_eq!(arena.stats(), (0, 0));
    }

    #[test]
    fn arena_shrink2() {
        let arena = super::SharedArena::<usize>::with_capacity(1000);

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
        let arena = super::SharedArena::<usize>::with_capacity(1000);

        assert_eq!(arena.size_lists(), (16, 16, 0));
        let a = arena.alloc(1);
        assert_eq!(arena.size_lists(), (16, 16, 0));

        let mut values = Vec::with_capacity(539);
        for _ in 0..539 {
            values.push(arena.alloc(1));
        }
        assert_eq!(arena.size_lists(), (16, 8, 0));

        arena.shrink_to_fit();

        assert_eq!(arena.size_lists(), (9, 1, 0));

        values.truncate(503);
        arena.shrink_to_fit();

        assert_eq!(arena.size_lists(), (8, 0, 0));

        std::mem::drop(a);
        for _ in 0..62 {
            values.remove(0);
        }

        assert_eq!(arena.size_lists(), (8, 0, 1));

        arena.shrink_to_fit();
        assert_eq!(arena.size_lists(), (8, 0, 1));

        values.clear();
        assert_eq!(arena.size_lists(), (8, 0, 8));

        arena.shrink_to_fit();
        assert_eq!(arena.size_lists(), (8, 0, 8));

        {
            let _a = arena.alloc(1);
            assert_eq!(arena.size_lists(), (8, 8, 0));

            println!("{:?}", arena);
            arena.display_list();
        }

        assert_eq!(arena.size_lists(), (8, 8, 0));
        arena.shrink_to_fit();
        assert_eq!(arena.size_lists(), (0, 0, 0));

        let mut values = Vec::with_capacity(126);
        for _ in 0..126 {
            values.push(arena.alloc(1));
        }
        assert_eq!(arena.size_lists(), (2, 1, 0));

        values.remove(0);
        assert_eq!(arena.size_lists(), (2, 1, 1));

        values.push(arena.alloc(1));
        assert_eq!(arena.size_lists(), (2, 1, 0));
    }

    // // #[test]
    // fn arena_size() {
    //     let arena = super::SharedArena::<usize>::with_capacity(1000);

    //     assert_eq!(arena.size_lists(), (16, 16));
    //     let a = arena.alloc(1);
    //     assert_eq!(arena.size_lists(), (16, 16));

    //     let mut values = Vec::with_capacity(539);
    //     for _ in 0..539 {
    //         values.push(arena.alloc(1));
    //     }
    //     assert_eq!(arena.size_lists(), (16, 8));

    //     arena.shrink_to_fit();

    //     assert_eq!(arena.size_lists(), (9, 1));

    //     values.truncate(503);
    //     arena.shrink_to_fit();

    //     assert_eq!(arena.size_lists(), (8, 0));

    //     std::mem::drop(a);
    //     for _ in 0..62 {
    //         values.remove(0);
    //     }

    //     assert_eq!(arena.size_lists(), (8, 1));

    //     arena.shrink_to_fit();
    //     assert_eq!(arena.size_lists(), (7, 0));

    //     values.clear();
    //     assert_eq!(arena.size_lists(), (7, 7));

    //     arena.shrink_to_fit();
    //     assert_eq!(arena.size_lists(), (0, 0));

    //     {
    //         let _a = arena.alloc(1);
    //         assert_eq!(arena.size_lists(), (1, 1));

    //         println!("{:?}", arena);
    //         arena.display_list();
    //     }

    //     assert_eq!(arena.size_lists(), (1, 1));
    //     arena.shrink_to_fit();
    //     assert_eq!(arena.size_lists(), (0, 0));

    //     let mut values = Vec::with_capacity(126);
    //     for _ in 0..126 {
    //         values.push(arena.alloc(1));
    //     }
    //     assert_eq!(arena.size_lists(), (2, 1));

    //     values.remove(0);
    //     assert_eq!(arena.size_lists(), (2, 2));

    //     values.push(arena.alloc(1));
    //     assert_eq!(arena.size_lists(), (2, 1));
    // }

    #[test]
    fn alloc_fns() {
        let arena = super::SharedArena::<usize>::new();

        use std::ptr;

        let a = arena.alloc_in_place(|place| unsafe {
            ptr::copy(&101, place.as_mut_ptr(), 1);
        });
        assert!(*a == 101);

        let a = arena.alloc_in_place_arc(|place| unsafe {
            ptr::copy(&102, place.as_mut_ptr(), 1);
        });
        assert!(*a == 102);

        let a = arena.alloc(103);
        assert!(*a == 103);

        let a = arena.alloc_arc(104);
        assert!(*a == 104);
    }

    #[test]
    fn drop_arena_with_valid_allocated() {
        let (a, b, c, d) = {
            let arena = super::SharedArena::<usize>::new();

            use std::ptr;

            let a = arena.alloc_in_place(|place| unsafe {
                ptr::copy(&101, place.as_mut_ptr(), 1);
            });
            let b = arena.alloc_in_place_arc(|place| unsafe {
                ptr::copy(&102, place.as_mut_ptr(), 1);
            });
            let c = arena.alloc(103);
            let d = arena.alloc_arc(104);

            (a, b, c, d)
        };

        assert_eq!((*a, *b, *c, *d), (101, 102, 103, 104))
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn arena_with_threads() {
        test_with_threads(12, 1024 * 64, false);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn arena_with_threads_and_shrinks() {
        test_with_threads(12, 1024 * 4, true);
    }

    #[test]
    fn miri_arena_with_threads() {
        test_with_threads(12, 128, false);
    }

    #[test]
    fn miri_arena_with_threads_and_shrinks() {
        test_with_threads(12, 64, true);
    }

    fn test_with_threads(nthreads: usize, nallocs: usize, with_shrink: bool) {
        use std::sync::{Arc, Barrier};
        use std::thread;
        use std::time::{SystemTime, UNIX_EPOCH};

        fn get_random_number(max: usize) -> usize {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .subsec_nanos() as usize;

            nanos % max
        }

        let arena = Arc::new(super::SharedArena::<usize>::default());

        let mut values = Vec::with_capacity(126);
        for _ in 0..63 {
            values.push(arena.alloc(1));
        }

        let mut handles = Vec::with_capacity(nthreads);
        let barrier = Arc::new(Barrier::new(nthreads));

        for _ in 0..nthreads {
            let c = barrier.clone();
            let arena = arena.clone();
            handles.push(thread::spawn(move|| {
                c.wait();

                arena.shrink_to_fit();

                let mut nshrink = 0;

                let mut values = Vec::with_capacity(nallocs);
                for i in 0..(nallocs) {
                    values.push(arena.alloc(1));
                    let rand = get_random_number(values.len());
                    if (i + 1) % 5 == 0 {
                        values.remove(rand);
                    }
                    if with_shrink && rand % 200 == 0 {
                        if arena.shrink_to_fit() {
                            nshrink += 1;
                        }
                    }
                }

                println!("NSHRINK: {}", nshrink);
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }
    }
}
