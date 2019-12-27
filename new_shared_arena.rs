
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::*;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize};
use std::sync::Arc;

use super::page::{Block, Page, BLOCK_PER_PAGE};
use super::arena_arc::ArenaArc;
use super::arena_box::ArenaBox;

/// An arena shareable across threads
///
/// Pointers to the elements in the SharedArena are shareable as well.
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

    fn alloc_new_page(&self) -> NonNull<Page<T>> {
        let len = self.npages.load(Relaxed);

        let to_allocate = len.max(1).min(900_000);

        let (mut first, mut last) = Page::make_list(to_allocate, &self.free);

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
    /// ```
    /// use shared_arena::SharedArena;
    ///
    /// let arena = SharedArena::with_capacity(2048);
    /// ```
    pub fn with_capacity(cap: usize) -> SharedArena<T> {
        let npages = ((cap.max(1) - 1) / BLOCK_PER_PAGE) + 1;
        let free = Arc::new(AtomicPtr::new(std::ptr::null_mut()));

        let (mut first, _) = Page::make_list(npages, &free);
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

    /// Constructs a new SharedArena capable of holding exactly 63 elements
    ///
    /// The Arena will reallocate itself if there is not enough space
    /// when allocating (with alloc* functions)
    ///
    /// ## Example
    ///
    /// ```
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
    /// use shared_arena::SharedArena;
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
    /// use shared_arena::SharedArena;
    /// use std::ptr;
    ///
    /// #[derive(Copy, Clone)]
    /// struct MyStruct { .. }
    ///
    /// let ref_struct: &MyStruct = ..;
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
    /// use shared_arena::SharedArena;
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
    /// use shared_arena::SharedArena;
    /// use std::ptr;
    ///
    /// #[derive(Copy, Clone)]
    /// struct MyStruct { .. }
    ///
    /// let ref_struct: &MyStruct = ..;
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
    /// let arena = SharedArena::with_capacity(2048);
    /// let values = Vec::new();
    ///
    /// let (used, free) = arena.stats();
    /// assert!(used == 0, free == 2048)
    ///
    /// for _ in 0..80 {
    ///     values.push(arena.alloc(0xFF));
    /// }
    ///
    /// arena.shrink_to_fit();
    ///
    /// let (used, free) = arena.stats();
    /// assert!(used == 80, free == 46)
    ///
    /// ```
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
        // between the moment we read the bitfield and the moment we removed
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

    /// Returns a tuple of non-free and free spaces in the arena
    ///
    /// ## Example
    ///
    /// ```
    /// use shared_arena::SharedArena;
    ///
    /// let arena = SharedArena::new();
    /// arena.alloc(1);
    /// let (used, free) = arena.stats();
    /// assert!(used == 1 && free == 62);
    /// ```
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
