
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::*;
use std::sync::atomic::AtomicPtr;
use std::sync::Arc;
use std::cell::Cell;

use crate::block::Block;
use crate::page::arena::{PageArena, drop_page};
use crate::common::{Pointer, BLOCK_PER_PAGE};
use crate::{ArenaRc, ArenaBox, ArenaArc};

/// An arena shareable across threads
///
/// Pointers to the elements in the SharedArena are shareable as well.
pub struct Arena<T: Sized> {
    free_list: Pointer<PageArena<T>>,
    pending_free_list: Arc<AtomicPtr<PageArena<T>>>,
    full_list: AtomicPtr<PageArena<T>>,
    npages: Cell<usize>,
}

unsafe impl<T: Sized> Send for Arena<T> {}

impl<T: Sized> Arena<T> {
    fn alloc_new_page(&self) {
        let to_allocate = self.npages
                              .get()
                              .max(1)
                              .min(900_000);

        let (first, mut last) = PageArena::make_list(to_allocate, &self.pending_free_list);

        let first_ptr = first.as_ptr();
        let last_ref = unsafe { last.as_mut() };

        // We have to touch self.page_list before self.free because
        // shrink_to_fit might be running at the same time with another
        // thread.
        // If we update self.page_list _after_ self.free shrink_to_fit
        // will try to remove pages that are not yet in self.page_list

        let current = self.full_list.load(Relaxed);
        last_ref.next.store(current, Relaxed);
        self.full_list.swap(first_ptr, AcqRel);

        let current = self.free_list.get();
        assert!(current.is_null(), "Arena.free_list isn't null");

        self.free_list.set(first_ptr);

        // self.npages.fetch_add(to_allocate, Relaxed);
        self.npages.set(self.npages.get() + to_allocate);
    }

    fn find_place(&self) -> NonNull<Block<T>> {
        loop {
            while let Some(page) = unsafe { self.free_list.get().as_mut() } {

                if let Some(block) = page.acquire_free_block() {
                    return block;
                }

                // No free block on the page, we remove it from the free list
                let next = page.next_free.load(Acquire);
                self.free_list.set(next);
                page.in_free_list.store(false, Release);
            }

            let pending = self.pending_free_list.load(Relaxed);
            if !pending.is_null() {
                // Move self.pending_free to self.free.

                let pending = self.pending_free_list.swap(std::ptr::null_mut(), AcqRel);
                self.free_list.set(pending);
            } else {
                // No pages in self.pending_free. We allocate new pages.

                self.alloc_new_page();
            }
        }
    }

    /// Constructs a new Arena capable of holding at least `cap` elements
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
    /// use shared_arena::Arena;
    ///
    /// let arena = Arena::with_capacity(2048);
    /// ```
    pub fn with_capacity(cap: usize) -> Arena<T> {
        let npages = ((cap.max(1) - 1) / BLOCK_PER_PAGE) + 1;
        let pending_free = Arc::new(AtomicPtr::new(std::ptr::null_mut()));

        let (first, _) = PageArena::make_list(npages, &pending_free);

        Arena {
            npages: Cell::new(npages),
            free_list: Cell::new(first.as_ptr()),
            pending_free_list: pending_free,
            full_list: AtomicPtr::new(first.as_ptr()),
        }
    }

    /// Constructs a new Arena capable of holding exactly 63 elements
    ///
    /// The Arena will reallocate itself if there is not enough space
    /// when allocating (with alloc* functions)
    ///
    /// ## Example
    ///
    /// ```ignore
    /// use shared_arena::Arena;
    ///
    /// let arena = Arena::new();
    /// ```
    pub fn new() -> Arena<T> {
        Arena::with_capacity(BLOCK_PER_PAGE)
    }

    /// Writes a value in the arena, and returns an [`ArenaBox`]
    /// pointing to that value.
    ///
    /// ## Example
    ///
    /// ```
    /// use shared_arena::{ArenaBox, Arena};
    ///
    /// let arena = Arena::new();
    /// let my_num: ArenaBox<u8> = arena.alloc(0xFF);
    /// ```
    ///
    /// [`ArenaBox`]: ./struct.ArenaBox.html
    pub fn alloc(&self, value: T) -> ArenaBox<T> {
        let block = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            ptr.write(value);
        }

        ArenaBox::new(block)
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
    /// use shared_arena::{ArenaBox, Arena};
    /// use std::ptr;
    ///
    /// #[derive(Copy, Clone)]
    /// struct MyStruct { }
    ///
    /// let ref_struct: &MyStruct = &MyStruct{};
    ///
    /// let arena = Arena::new();
    /// let my_struct: ArenaBox<MyStruct> = arena.alloc_with(|place| {
    ///     unsafe {
    ///         // The type must be Copy to use ptr::copy
    ///         ptr::copy(ref_struct, place.as_mut_ptr(), 1);
    ///     }
    /// });
    /// ```
    ///
    /// [`ArenaBox`]: ./struct.ArenaBox.html
    /// [`alloc`]: struct.Arena.html#method.alloc
    /// [`MaybeUninit`]: https://doc.rust-lang.org/std/mem/union.MaybeUninit.html
    pub fn alloc_with<F>(&self, initializer: F) -> ArenaBox<T>
    where
        F: Fn(&mut MaybeUninit<T>)
    {
        let block = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            initializer(&mut *(ptr as *mut std::mem::MaybeUninit<T>));
        }

        ArenaBox::new(block)
    }

    /// Writes a value in the arena, and returns an [`ArenaArc`]
    /// pointing to that value.
    ///
    /// ## Example
    ///
    /// ```
    /// use shared_arena::{ArenaArc, Arena};
    ///
    /// let arena = Arena::new();
    /// let my_num: ArenaArc<u8> = arena.alloc_arc(0xFF);
    /// ```
    ///
    /// [`ArenaArc`]: ./struct.ArenaArc.html
    pub fn alloc_arc(&self, value: T) -> ArenaArc<T> {
        let block = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            ptr.write(value);
        }

        ArenaArc::new(block)
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
    /// use shared_arena::{ArenaArc, Arena};
    /// use std::ptr;
    ///
    /// #[derive(Copy, Clone)]
    /// struct MyStruct {}
    ///
    /// let ref_struct: &MyStruct = &MyStruct {};
    ///
    /// let arena = Arena::new();
    /// let my_struct: ArenaArc<MyStruct> = arena.alloc_arc_with(|place| {
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
    pub fn alloc_arc_with<F>(&self, initializer: F) -> ArenaArc<T>
    where
        F: Fn(&mut MaybeUninit<T>)
    {
        let block = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            initializer(&mut *(ptr as *mut std::mem::MaybeUninit<T>));
        }

        ArenaArc::new(block)
    }

    /// Writes a value in the arena, and returns an [`ArenaRc`]
    /// pointing to that value.
    ///
    /// ## Example
    ///
    /// ```
    /// use shared_arena::{ArenaRc, Arena};
    ///
    /// let arena = Arena::new();
    /// let my_num: ArenaRc<u8> = arena.alloc_rc(0xFF);
    /// ```
    ///
    /// [`ArenaRc`]: ./struct.ArenaRc.html
    pub fn alloc_rc(&self, value: T) -> ArenaRc<T> {
        let block = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            ptr.write(value);
        }

        ArenaRc::new(block)
    }

    /// Finds an empty space in the arena and calls the function `initializer`
    /// with its argument pointing to that space.
    /// It returns an [`ArenaRc`] pointing to the newly initialized value.
    ///
    /// The difference with [`alloc_rc`] is that it has the benefit of
    /// avoiding intermediate copies of the value.
    ///
    /// ## Safety
    ///
    /// It is the caller responsability to initialize properly the value.
    /// When all [`ArenaRc`] pointing that value are dropped, the value
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
    /// use shared_arena::{ArenaRc, Arena};
    /// use std::ptr;
    ///
    /// #[derive(Copy, Clone)]
    /// struct MyStruct {}
    ///
    /// let ref_struct: &MyStruct = &MyStruct {};
    ///
    /// let arena = Arena::new();
    /// let my_struct: ArenaRc<MyStruct> = arena.alloc_rc_with(|place| {
    ///     unsafe {
    ///         // The type must be Copy to use ptr::copy
    ///         ptr::copy(ref_struct, place.as_mut_ptr(), 1);
    ///     }
    /// });
    /// ```
    ///
    /// [`ArenaRc`]: ./struct.ArenaRc.html
    /// [`alloc_rc`]: #method.alloc_rc
    /// [`MaybeUninit`]: https://doc.rust-lang.org/std/mem/union.MaybeUninit.html
    pub fn alloc_rc_with<F>(&self, initializer: F) -> ArenaRc<T>
    where
        F: Fn(&mut MaybeUninit<T>)
    {
        let block = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            initializer(&mut *(ptr as *mut std::mem::MaybeUninit<T>));
        }

        ArenaRc::new(block)
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
    /// use shared_arena::Arena;
    ///
    /// let mut arena = Arena::with_capacity(2048);
    /// let mut values = Vec::new();
    ///
    /// assert_eq!(arena.stats(), (0, 2079));
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
        let mut current: &AtomicPtr<PageArena<T>> = &AtomicPtr::new(self.free_list.get());
        self.free_list.set(std::ptr::null_mut());

        let start = current;

        let mut to_drop = Vec::with_capacity(self.npages.get());

        // We loop on the free list to get all pages that have 0 reference to
        // them and remove them from the free list
        while let Some(current_value) = unsafe { current.load(Relaxed).as_mut() } {
            let next = &current_value.next_free;
            let next_value = next.load(Relaxed);

            if current_value.bitfield.get() | current_value.bitfield_atomic.load(Acquire) == !0 {
                if current.compare_exchange(
                    current_value as *const _ as *mut _, next_value, AcqRel, Relaxed
                ).is_ok() {
                    to_drop.push(current_value as *const _ as *mut PageArena<T>);
                }
            } else {
                current = next;
            }
        }

        // Now we are 100% sure that pages in to_drop are/will not be
        // referenced anymore

        let mut current: &AtomicPtr<PageArena<T>> = &self.full_list;

        // Loop on the full list
        // We remove the pages from it
        while let Some(current_value) = unsafe { current.load(Relaxed).as_mut() } {
            let next = &current_value.next;
            let next_value = next.load(Relaxed);

            if to_drop.contains(&(current_value as *const _ as *mut PageArena<T>)) {
                current.compare_exchange(
                    current_value as *const _ as *mut _, next_value, AcqRel, Relaxed
                ).expect("Something went wrong in shrinking.");
            } else {
                current = next;
            }
        }

        for page in &to_drop {
            let page_ref = unsafe { page.as_ref().unwrap() };

            assert!(page_ref.bitfield.get() | page_ref.bitfield_atomic.load(Acquire) == !0);

            drop_page(*page);
        }

        self.free_list.set(start.load(Relaxed));

        self.npages.set(self.npages.get() - to_drop.len());

        true
    }

    /// Returns a tuple of non-free and free spaces in the arena
    ///
    /// ## Example
    ///
    /// ```
    /// use shared_arena::Arena;
    ///
    /// let arena = Arena::new();
    /// let item = arena.alloc(1);
    /// let (used, free) = arena.stats();
    /// assert!(used == 1 && free == 62);
    /// ```
    pub fn stats(&self) -> (usize, usize) {
        let mut next = self.full_list.load(Relaxed);
        let mut used = 0;
        let mut npages = 0;

        while let Some(next_ref) = unsafe { next.as_mut() } {
            let next_next = next_ref.next.load(Relaxed);

            let bitfield = next_ref.bitfield.get();
            let bitfield_atomic = next_ref.bitfield_atomic.load(Relaxed);
            let zeros = (bitfield | bitfield_atomic).count_zeros() as usize;
            used += zeros;
            next = next_next;

            // println!("PAGE_PTR  {:p} ZEROS={}", next_ref, zeros);
            // println!("PAGE BITFIELD {:064b}", next_ref.bitfield.get());
            // println!("PAGE BIT_ATOM {:064b}", next_ref.bitfield_atomic.load(Relaxed));
            // println!("RECONCILATION {:064b}", (bitfield | bitfield_atomic));
            npages += 1;
        }

        // println!("NPAGES_COUNT={}", npages);
        // eprintln!("USED={} NPAGES={} NPAGES*BLOCK_PER_PAGE={}", used, self.npages.get(), (self.npages.get() * BLOCK_PER_PAGE));

        assert!(npages == self.npages.get());

        let free = (npages * BLOCK_PER_PAGE) - used;

        (used, free)
    }

    #[cfg(target_pointer_width = "64") ]
    #[cfg(test)]
    pub(crate) fn size_lists(&self) -> (usize, usize, usize) {
        let mut next = self.full_list.load(Relaxed);
        let mut size = 0;
        while let Some(next_ref) = unsafe { next.as_mut() } {
            next = next_ref.next.load(Relaxed);
            size += 1;
        }

        let mut next = self.free_list.get();
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

        let mut next = self.free_list.get();
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
        let mut next = self.full_list.load(Relaxed);

        while let Some(next_ref) = unsafe { next.as_mut() } {
            let next_next = next_ref.next.load(Relaxed);
            drop_page(next);
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
        struct Page {
            free: usize,
            used: usize,
        }

        impl std::fmt::Debug for Page {
            fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "Page {{ free: {} used: {} }}", self.free, self.used)
            }
        }

        let npages = self.npages.get();

        let mut vec = Vec::with_capacity(npages);

        let mut next = self.full_list.load(Relaxed);

        while let Some(next_ref) = unsafe { next.as_mut() } {
            let used = (next_ref.bitfield.get() | next_ref.bitfield_atomic.load(Relaxed)).count_zeros() as usize;
            vec.push(Page {
                used,
                free: BLOCK_PER_PAGE - used
            });

            next = next_ref.next.load(Relaxed);
        }

        let blocks_used: usize = vec.iter().map(|p| p.used).sum();
        let blocks_free: usize = vec.iter().map(|p| p.free).sum();

        f.debug_struct("Arena")
         .field("blocks_free", &blocks_free)
         .field("blocks_used", &blocks_used)
         .field("npages", &npages)
         .field("pages", &vec)
         .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::Arena;

    #[cfg(target_pointer_width = "64") ]
    #[test]
    fn arena_shrink() {
        let arena = Arena::<usize>::with_capacity(1000);
        assert_eq!(arena.stats(), (0, 1008));
        arena.shrink_to_fit();
        assert_eq!(arena.stats(), (0, 0));
    }

    #[cfg(target_pointer_width = "64") ]
    #[test]
    fn arena_shrink2() {
        let arena = Arena::<usize>::with_capacity(1000);

        println!("A");
        let _a = arena.alloc(1);
        arena.shrink_to_fit();
        assert_eq!(arena.stats(), (1, 62));

        println!("A1");
        let _a = arena.alloc(1);
        arena.shrink_to_fit();
        assert_eq!(arena.stats(), (2, 61));

        println!("A2");
        let mut values = Vec::with_capacity(64);
        for _ in 0..64 {
            values.push(arena.alloc(1));
        }

        println!("A3");
        assert_eq!(arena.stats(), (66, 60));
        println!("A32");
        arena.shrink_to_fit();
        println!("A33");
        assert_eq!(arena.stats(), (66, 60));

        println!("A4");
        std::mem::drop(values);

        println!("A5");
        assert_eq!(arena.stats(), (2, 124));
        println!("A6");
        arena.shrink_to_fit();
        println!("A7");
        assert_eq!(arena.stats(), (2, 61));
    }

    #[cfg(target_pointer_width = "64") ]
    #[test]
    fn arena_size() {
        let arena = Arena::<usize>::with_capacity(1000);

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

        println!("LA", );
        assert_eq!(arena.size_lists(), (8, 0, 8));
        println!("LA2", );

        {
            let _a = arena.alloc(1);
            println!("LA3", );
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

    #[test]
    fn alloc_fns() {
        let arena = Arena::<usize>::new();

        use std::ptr;

        let a = arena.alloc_with(|place| unsafe {
            ptr::copy(&101, place.as_mut_ptr(), 1);
        });
        assert!(*a == 101);

        let a = arena.alloc_arc_with(|place| unsafe {
            ptr::copy(&102, place.as_mut_ptr(), 1);
        });
        assert!(*a == 102);

        let a = arena.alloc_rc_with(|place| unsafe {
            ptr::copy(&103, place.as_mut_ptr(), 1);
        });
        assert!(*a == 103);

        let a = arena.alloc(104);
        assert!(*a == 104);

        let a = arena.alloc_arc(105);
        assert!(*a == 105);

        let a = arena.alloc_rc(106);
        assert!(*a == 106);
    }

    #[test]
    fn drop_arena_with_valid_allocated() {
        let (a, b, c, d) = {
            let arena = Arena::<usize>::new();

            use std::ptr;

            let a = arena.alloc_with(|place| unsafe {
                ptr::copy(&101, place.as_mut_ptr(), 1);
            });
            let b = arena.alloc_arc_with(|place| unsafe {
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

    // #[test]
    fn test_with_threads(nthreads: usize, nallocs: usize, with_shrink: bool) {
    // fn test_with_threads() {
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

        // let mut nshrink = 0;

        let arena = Arena::<usize>::default();

        let mut values_for_threads = Vec::with_capacity(nthreads);

        for _ in 0..(nthreads + 1) {
            let mut values = Vec::with_capacity(nallocs);
            for _ in 0..values.capacity() {
                values.push(arena.alloc(1));
            }
            values_for_threads.push(values);
        }

        let mut handles = Vec::with_capacity(nthreads);
        let barrier = Arc::new(Barrier::new(nthreads + 1));

        for _ in 0..nthreads {
            let c = barrier.clone();
            let mut values = values_for_threads.pop().unwrap();

            handles.push(thread::spawn(move|| {
                c.wait();
                while values.len() > 0 {
                    values.pop();
                    // println!("POP HERE", );
                }
            }));
        }

        let mut values = values_for_threads.pop().unwrap();

        barrier.wait();
        while values.len() > 0 {

            let rand = get_random_number(values.len());

            if with_shrink && rand % 200 == 0 {
                // println!("SHRINKING", );
                if arena.shrink_to_fit() {
                    // nshrink += 1;
                }
            }
            // println!("POP THERE", );
            values.pop();
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // println!("NSHRINK={}", nshrink);
    }
}
