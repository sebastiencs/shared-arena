
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::*;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, AtomicU16};
use std::sync::Arc;

use crate::common::BLOCK_PER_PAGE;
use crate::block::Block;
use crate::page::shared_arena::{PageSharedArena, drop_page};
use crate::{ArenaArc, ArenaBox, ArenaRc};

/// An arena shareable across threads
///
/// Pointers to the elements in the `SharedArena` are shareable as well.
///
/// ## Example
///
/// ```
/// use shared_arena::SharedArena;
/// use std::sync::Arc;
///
/// let arena = Arc::new(SharedArena::new());
/// let arena2 = arena.clone();
///
/// let value = std::thread::spawn(move || {
///     arena2.alloc(100)
/// });
///
/// let item = arena.alloc(1);
///
/// assert_eq!(*item + *value.join().unwrap(), 101);
///
/// std::mem::drop(arena);
///
/// // The value is still valid, even if the arena has been dropped
/// assert_eq!(*item, 1);
/// ```
pub struct SharedArena<T: Sized> {
    free_list: AtomicPtr<PageSharedArena<T>>,
    pending_free_list: Arc<AtomicPtr<PageSharedArena<T>>>,
    full_list: AtomicPtr<PageSharedArena<T>>,
    npages: AtomicUsize,
    writer: AtomicBool,
    shrinking: AtomicBool,
    to_free: AtomicPtr<Vec<NonNull<PageSharedArena<T>>>>,
    to_free_delay: AtomicU16,
}

unsafe impl<T: Sized> Send for SharedArena<T> {}
unsafe impl<T: Sized> Sync for SharedArena<T> {}

const DELAY_DROP_SHRINK: u16 = 10;

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
    fn put_pages_in_lists(
        &self,
        npages: usize,
        first: NonNull<PageSharedArena<T>>,
        mut last: NonNull<PageSharedArena<T>>
    ) {
        let first_ptr = first.as_ptr();
        let last_ref = unsafe { last.as_mut() };

        // We have to touch self.page_list before self.free because
        // shrink_to_fit might be running at the same time with another
        // thread.
        // If we update self.page_list _after_ self.free shrink_to_fit
        // will try to remove pages that are not yet in self.page_list

        let current = self.full_list.load(Relaxed);
        last_ref.next = AtomicPtr::new(current);
        let old = self.full_list.swap(first_ptr, AcqRel);
        assert_eq!(current, old);

        let current = self.free_list.load(Relaxed);
        assert!(current.is_null(), "Arena.free isn't null");

        let old = self.free_list.swap(first_ptr, AcqRel);
        assert!(old.is_null(), "Arena.free2 isn't null");

        self.npages.fetch_add(npages, Relaxed);
    }

    fn alloc_new_page(&self) {
        let to_allocate = self.npages
                              .load(Relaxed)
                              .max(1)
                              .min(900_000);

        let (first, last) = PageSharedArena::make_list(to_allocate, &self.pending_free_list);
        self.put_pages_in_lists(to_allocate, first, last);
    }

    fn maybe_free_pages(&self) {
        if self.to_free_delay.load(Relaxed) < DELAY_DROP_SHRINK {
            let old = self.to_free_delay.fetch_add(1, AcqRel);
            if old == DELAY_DROP_SHRINK - 1 {
                let to_free = self.to_free.swap(std::ptr::null_mut(), AcqRel);

                if let Some(to_free) = unsafe { to_free.as_mut() } {
                    let to_free = unsafe { Box::from_raw(to_free) };
                    for page in &*to_free {
                        drop_page(page.as_ptr());
                    }
                }
            }
        }
    }

    fn find_place(&self) -> NonNull<Block<T>> {
        loop {
            while let Some(page) = unsafe { self.free_list.load(Acquire).as_mut() } {

                if let Some(block) = page.acquire_free_block() {
                    return block;
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

            if let Some(_guard) = WriterGuard::new(&self.writer) {
                if self.free_list.load(Acquire).is_null() {
                    // A single and only thread run this block at a time.
                    //
                    // 3 ways to get new pages:
                    // - take pending_free_list
                    // - reuse pages that were removed with shrink()
                    // - allocate

                    let pending = self.pending_free_list.load(Relaxed);
                    if !pending.is_null() {
                        // Move self.pending_free to self.free.

                        let pending = self.pending_free_list.swap(std::ptr::null_mut(), AcqRel);
                        let old = self.free_list.swap(pending, Release);
                        assert!(old.is_null());

                        self.maybe_free_pages();
                    } else if !self.to_free.load(Relaxed).is_null() {
                        // Take pages that were removed from shrink()

                        self.take_pages_to_be_freed();
                    } else {
                        // No pages in self.pending_free. We allocate new pages.

                        self.alloc_new_page();
                    }
                }

                continue;
            };

            if self.free_list.load(Relaxed).is_null() {
                std::thread::yield_now();
            } else {
                self.maybe_free_pages();
            }

            // // This block is reached if an another thread is allocating or replacing
            // // self.pending_free (the block just above).
            // // Since allocating might take a while, some page might become free during
            // // this time.
            // // So instead of looping on self.free (which will stay null until allocation
            // // is done), we check for pages on self.pending_free.

            // let mut next = unsafe { self.pending_free_list.load(Acquire).as_mut() };

            // while let Some(page) = next {
            //     if self.shrinking.load(Acquire) {
            //         break;
            //     }
            //     if let Some(block) = page.acquire_free_block() {
            //         return block;
            //     }
            //     if self.shrinking.load(Acquire) {
            //         break;
            //     }
            //     next = unsafe { page.next_free.load(Acquire).as_mut() };
            // }
        }
    }

    fn take_pages_to_be_freed(&self) {
        if let Some(to_free) = unsafe {
            self.to_free.swap(std::ptr::null_mut(), AcqRel).as_mut()
        } {
            let mut to_free = unsafe { Box::from_raw(to_free) };

            let npages = self.npages.load(Relaxed).max(1);
            let truncate_at = to_free.len().saturating_sub(npages);
            let to_reinsert = &to_free[truncate_at..];

            let (first, last) = PageSharedArena::make_list_from_slice(&to_reinsert);
            self.put_pages_in_lists(to_reinsert.len(), first, last);

            if truncate_at != 0 {
                to_free.truncate(truncate_at);
                let old = self.to_free.swap(Box::into_raw(to_free), Release);
                assert!(old.is_null());
                self.to_free_delay.store(0, Relaxed);
            } else {
                self.to_free_delay.store(DELAY_DROP_SHRINK, Relaxed);
            }
        }
    }

    /// Constructs a new `SharedArena` capable of holding at least `cap` elements
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
    /// # use shared_arena::SharedArena;
    /// let arena = SharedArena::with_capacity(2048);
    /// # arena.alloc(1);
    /// ```
    pub fn with_capacity(cap: usize) -> SharedArena<T> {
        let npages = ((cap.max(1) - 1) / BLOCK_PER_PAGE) + 1;
        let pending_free = Arc::new(AtomicPtr::new(std::ptr::null_mut()));

        let (first, _) = PageSharedArena::make_list(npages, &pending_free);

        SharedArena {
            npages: AtomicUsize::new(npages),
            free_list: AtomicPtr::new(first.as_ptr()),
            pending_free_list: pending_free,
            full_list: AtomicPtr::new(first.as_ptr()),
            writer: AtomicBool::new(false),
            shrinking: AtomicBool::new(false),
            to_free: AtomicPtr::new(std::ptr::null_mut()),
            to_free_delay: AtomicU16::new(DELAY_DROP_SHRINK)
        }
    }


    /// Constructs a new `SharedArena` capable of holding exactly 63 elements
    ///
    /// The Arena will reallocate itself if there is not enough space
    /// when allocating (with alloc* functions)
    ///
    /// ## Example
    ///
    /// ```
    /// # use shared_arena::SharedArena;
    /// let arena = SharedArena::new();
    /// # arena.alloc(1);
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
    /// # use shared_arena::{ArenaBox, SharedArena};
    /// let arena = SharedArena::new();
    /// let my_num: ArenaBox<u8> = arena.alloc(0xFF);
    ///
    /// assert_eq!(*my_num, 255);
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
    /// `initializer` must return `&T`, this is a way to ensure that
    /// its parameter `&mut MaybeUninit<T>` has been "consumed".
    ///
    /// If `initializer` returns a different reference than its parameter,
    /// the function will panic.
    ///
    /// When the [`ArenaBox`] is dropped, the value is also
    /// dropped. If the value is not initialized correctly, it will
    /// drop an unitialized value, which is undefined behavior.
    ///
    /// ## Example
    ///
    /// ```
    /// # use shared_arena::{ArenaBox, SharedArena};
    /// # use std::ptr;
    /// # use core::mem::MaybeUninit;
    /// struct MyData {
    ///     a: usize
    /// }
    ///
    /// fn initialize_data<'a>(uninit: &'a mut MaybeUninit<MyData>, source: &MyData) -> &'a MyData {
    ///     unsafe {
    ///         let ptr = uninit.as_mut_ptr();
    ///         ptr::copy(source, ptr, 1);
    ///         &*ptr
    ///     }
    /// }
    ///
    /// let arena = SharedArena::<MyData>::new();
    /// let source = MyData { a: 101 };
    ///
    /// let data = arena.alloc_with(|uninit| {
    ///     initialize_data(uninit, &source)
    /// });
    /// assert!(data.a == 101);
    /// ```
    ///
    /// [`ArenaBox`]: ./struct.ArenaBox.html
    /// [`alloc`]: struct.SharedArena.html#method.alloc
    /// [`MaybeUninit`]: https://doc.rust-lang.org/std/mem/union.MaybeUninit.html
    pub fn alloc_with<F>(&self, initializer: F) -> ArenaBox<T>
    where
        F: Fn(&mut MaybeUninit<T>) -> &T
    {
        let block = self.find_place();
        let result = ArenaBox::new(block);

        unsafe {
            let ptr = block.as_ref().value.get();
            let reference = initializer(&mut *(ptr as *mut std::mem::MaybeUninit<T>));
            assert_eq!(
                ptr as * const T,
                reference as * const T,
                "`initializer` must return a reference of its parameter"
            );
        }

        result
    }

    /// Writes a value in the arena, and returns an [`ArenaArc`]
    /// pointing to that value.
    ///
    /// ## Example
    ///
    /// ```
    /// # use shared_arena::{ArenaArc, SharedArena};
    /// let arena = SharedArena::new();
    /// let my_num: ArenaArc<u8> = arena.alloc_arc(0xFF);
    ///
    /// assert_eq!(*my_num, 255);
    /// ```
    ///
    /// [`ArenaArc`]: ./struct.ArenaArc.html
    pub fn alloc_arc(&self, value: T) -> ArenaArc<T> {
        let block = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            ptr.write(value);
        }

        // ArenaArc::new(|| {
        //     Page::drop_block(page, block);
        // }, block)
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
    /// `initializer` must return `&T`, this is a way to ensure that
    /// its parameter `&mut MaybeUninit<T>` has been "consumed".
    ///
    /// If `initializer` returns a different reference than its parameter,
    /// the function will panic.
    ///
    /// When all [`ArenaArc`] pointing that value are dropped, the value
    /// is also dropped. If the value is not initialized correctly, it will
    /// drop an unitialized value, which is undefined behavior.
    ///
    /// ## Example
    ///
    /// ```
    /// # use shared_arena::{ArenaBox, SharedArena};
    /// # use std::ptr;
    /// # use core::mem::MaybeUninit;
    /// struct MyData {
    ///     a: usize
    /// }
    ///
    /// fn initialize_data<'a>(uninit: &'a mut MaybeUninit<MyData>, source: &MyData) -> &'a MyData {
    ///     unsafe {
    ///         let ptr = uninit.as_mut_ptr();
    ///         ptr::copy(source, ptr, 1);
    ///         &*ptr
    ///     }
    /// }
    ///
    /// let arena = SharedArena::<MyData>::new();
    /// let source = MyData { a: 101 };
    ///
    /// let data = arena.alloc_arc_with(|uninit| {
    ///     initialize_data(uninit, &source)
    /// });
    /// assert!(data.a == 101);
    /// ```
    ///
    /// [`ArenaArc`]: ./struct.ArenaArc.html
    /// [`alloc_arc`]: #method.alloc_arc
    /// [`MaybeUninit`]: https://doc.rust-lang.org/std/mem/union.MaybeUninit.html
    pub fn alloc_arc_with<F>(&self, initializer: F) -> ArenaArc<T>
    where
        F: Fn(&mut MaybeUninit<T>) -> &T
    {
        let block = self.find_place();
        let result = ArenaArc::new(block);

        unsafe {
            let ptr = block.as_ref().value.get();
            let reference = initializer(&mut *(ptr as *mut std::mem::MaybeUninit<T>));

            assert_eq!(
                ptr as * const T,
                reference as * const T,
                "`initializer` must return a reference of its parameter"
            );
        }

        result
    }

    /// Writes a value in the arena, and returns an [`ArenaRc`]
    /// pointing to that value.
    ///
    /// ## Example
    ///
    /// ```
    /// # use shared_arena::{ArenaRc, SharedArena};
    /// let arena = SharedArena::new();
    /// let my_num: ArenaRc<u8> = arena.alloc_rc(0xFF);
    ///
    /// assert_eq!(*my_num, 255);
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
    /// `initializer` must return `&T`, this is a way to ensure that
    /// its parameter `&mut MaybeUninit<T>` has been "consumed".
    ///
    /// If `initializer` returns a different reference than its parameter,
    /// the function will panic.
    ///
    /// When all [`ArenaRc`] pointing that value are dropped, the value
    /// is also dropped. If the value is not initialized correctly, it will
    /// drop an unitialized value, which is undefined behavior.
    ///
    /// ## Example
    ///
    /// ```
    /// # use shared_arena::SharedArena;
    /// # use std::ptr;
    /// # use core::mem::MaybeUninit;
    /// struct MyData {
    ///     a: usize
    /// }
    ///
    /// fn initialize_data<'a>(uninit: &'a mut MaybeUninit<MyData>, source: &MyData) -> &'a MyData {
    ///     unsafe {
    ///         let ptr = uninit.as_mut_ptr();
    ///         ptr::copy(source, ptr, 1);
    ///         &*ptr
    ///     }
    /// }
    ///
    /// let arena = SharedArena::<MyData>::new();
    /// let source = MyData { a: 101 };
    ///
    /// let data = arena.alloc_rc_with(|uninit| {
    ///     initialize_data(uninit, &source)
    /// });
    /// assert!(data.a == 101);
    /// ```
    ///
    /// [`ArenaRc`]: ./struct.ArenaRc.html
    /// [`alloc_rc`]: #method.alloc_rc
    /// [`MaybeUninit`]: https://doc.rust-lang.org/std/mem/union.MaybeUninit.html
    pub fn alloc_rc_with<F>(&self, initializer: F) -> ArenaRc<T>
    where
        F: Fn(&mut MaybeUninit<T>) -> &T
    {
        let block = self.find_place();
        let result = ArenaRc::new(block);

        unsafe {
            let ptr = block.as_ref().value.get();
            let reference = initializer(&mut *(ptr as *mut std::mem::MaybeUninit<T>));
            assert_eq!(
                ptr as * const T,
                reference as * const T,
                "`initializer` must return a reference of its parameter"
            );
        }

        result
    }

    /// Shrinks the capacity of the arena as much as possible.
    ///
    /// It will drop all pages that are unused (no Arena{Box,Arc,Rc}
    /// points to it).  
    /// If there is still one or more references to a page, the page
    /// won't be dropped.
    ///
    /// This is a slow function and it should not be called in a hot
    /// path.
    ///
    /// The dedicated memory will be deallocated in an
    /// undetermined time in the future, not during the function call.
    /// While the time is not determined, it's guarantee that it will
    /// be deallocated.  
    /// `shrink_to_fit` on `Arena` and `Pool` don't have this behavior.
    ///
    /// Note that if `SharedArena` becomes full and one of the alloc_*
    /// function is called, it might reuses the pages freed by this
    /// function, if it has not be deallocated yet.
    ///
    /// ## Example
    ///
    /// ```
    /// # use shared_arena::SharedArena;
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

        let mut current: &AtomicPtr<PageSharedArena<T>> = &AtomicPtr::new(self.free_list.swap(std::ptr::null_mut(), AcqRel));
        let start = current;

        // let narenas = Arc::strong_count(&self.pending_free_list);

        // for _ in 0..narenas {
        //     std::thread::yield_now();
        // }

        let mut to_drop = Vec::with_capacity(self.npages.load(Relaxed));

        // We loop on the free list to get all pages that have 0 reference to
        // them and remove them from the free list
        while let Some(current_value) = unsafe { current.load(Relaxed).as_mut() } {
            let next = &current_value.next_free;
            let next_value = next.load(Acquire);

            if current_value.bitfield.load(Acquire) == !0 {
                if current.compare_exchange(
                    current_value as *const _ as *mut _, next_value, AcqRel, Relaxed
                ).is_ok() {
                    // current_value.in_free_list.store(false, Release);
                    let ptr = current_value as *const _ as *mut PageSharedArena<T>;
                    to_drop.push(NonNull::new(ptr).unwrap());
                }
            } else {
                current = next;
            }
        }

        // Check that the page hasn't been used by another thread
        to_drop.retain(|page| {
            let page_ref = unsafe { page.as_ref() };
            page_ref.bitfield.load(Acquire) == !0
        });

        let mut current: &AtomicPtr<PageSharedArena<T>> = &self.full_list;

        // Loop on the full list
        // We remove the pages from it
        while let Some(current_value) = unsafe { current.load(Relaxed).as_mut() } {
            let ptr = unsafe { NonNull::new_unchecked(current_value) };
            let next = &current_value.next;
            let next_value = next.load(Relaxed);

            if to_drop.contains(&ptr) {
                current.compare_exchange(
                    current_value as *const _ as *mut _, next_value, AcqRel, Relaxed
                ).expect("Something went wrong in shrinking.");
            } else {
                current = next;
            }
        }

        let nfreed = to_drop.len();

        if nfreed != 0 {
            self.to_free_delay.store(0, Release);
            if let Some(to_free) = unsafe { self.to_free.swap(std::ptr::null_mut(), AcqRel).as_mut() } {
                to_free.append(&mut to_drop);
                let old = self.to_free.swap(to_free, AcqRel);
                assert!(old.is_null());
            } else {
                let ptr = Box::new(to_drop);
                let old = self.to_free.swap(Box::into_raw(ptr), AcqRel);
                assert!(old.is_null());
            }
        }

        let old = self.free_list.swap(start.load(Relaxed), Release);
        assert!(old.is_null(), "OLD NOT NULL");

        self.npages.fetch_sub(nfreed, Release);

        self.shrinking.store(false, Release);

        true
    }

    /// Returns a tuple of non-free and free spaces in the arena
    ///
    /// This is a slow function and it should not be called in a hot
    /// path.
    ///
    /// ## Example
    ///
    /// ```
    /// # use shared_arena::SharedArena;
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

    #[cfg(target_pointer_width = "64") ]
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
        if let Some(to_free) = unsafe { self.to_free.load(Relaxed).as_mut() } {
            let to_free = unsafe { Box::from_raw(to_free) };
            for page in &*to_free {
                drop_page(page.as_ptr());
            }
        }

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

/// Code that should fail to compile.
/// compile_fail is supported on doc only
///
/// ```compile_fail
/// use shared_arena::SharedArena;
///
/// let arena: SharedArena<i32> = SharedArena::new();
/// arena.alloc_with(|_| {});
/// ```
///
/// ```compile_fail
/// use shared_arena::SharedArena;
///
/// let arena: SharedArena<i32> = SharedArena::new();
/// arena.alloc_rc_with(|_| {});
/// ```
///
/// ```compile_fail
/// use shared_arena::SharedArena;
///
/// let arena: SharedArena<i32> = SharedArena::new();
/// arena.alloc_arc_with(|_| {});
/// ```
///
#[allow(dead_code)]
fn arena_fail() {} // grcov_ignore

#[cfg(test)]
mod tests {
    use super::SharedArena;
    use std::mem::MaybeUninit;
    use std::ptr;

    #[cfg(target_pointer_width = "64") ]
    #[test]
    fn arena_shrink() {
        let arena = SharedArena::<usize>::with_capacity(1000);
        assert_eq!(arena.stats(), (0, 1008));
        arena.shrink_to_fit();
        assert_eq!(arena.stats(), (0, 0));
    }

    #[cfg(target_pointer_width = "64") ]
    #[test]
    fn arena_shrink2() {
        let arena = SharedArena::<usize>::with_capacity(1000);

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

    #[cfg(target_pointer_width = "64") ]
    #[test]
    fn arena_size() {
        let arena = SharedArena::<usize>::with_capacity(1000);

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

    // #[test]
    // fn arena_drop_in_to_free() {
    //     let arena = SharedArena::<usize>::new();
    //     let mut values = Vec::with_capacity(1000);

    //     for _ in 0..1000 {
    //         values.push(arena.alloc(1));
    //     }
    //     values.truncate(900);
    //     arena.shrink_to_fit();

    //     for _ in 0..100 {
    //         values.push(arena.alloc(1));
    //     }
    //     values.truncate(800);
    //     for _ in 0..64 {
    //         values.push(arena.alloc(1));
    //     }
    //     values.truncate(700);
    //     for _ in 0..200 {
    //         values.push(arena.alloc(1));
    //     }
    //     values.truncate(600);
    //     for _ in 0..300 {
    //         values.push(arena.alloc(1));
    //     }
    // }

    // // #[test]
    // fn arena_size() {
    //     let arena = SharedArena::<usize>::with_capacity(1000);

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
    fn alloc_with_initializer() {
        struct MyData {
            a: usize
        }

        fn initialize_data<'d>(uninit: &'d mut MaybeUninit<MyData>, source: &MyData) -> &'d MyData {
            unsafe {
                let ptr = uninit.as_mut_ptr();
                ptr::copy(source, ptr, 1);
                &*ptr
            }
        }

        let arena = SharedArena::<MyData>::new();

        let source = MyData { a: 101 };
        let data = arena.alloc_with(|uninit| {
            initialize_data(uninit, &source)
        });
        assert!(data.a == 101);

        let source = MyData { a: 102 };
        let data = arena.alloc_rc_with(|uninit| {
            initialize_data(uninit, &source)
        });
        assert!(data.a == 102);

        let source = MyData { a: 103 };
        let data = arena.alloc_arc_with(|uninit| {
            initialize_data(uninit, &source)
        });
        assert!(data.a == 103);
    }

    #[test]
    #[should_panic]
    fn alloc_with_panic() {
        let arena = SharedArena::<usize>::new();
        const SOURCE: usize = 10;

        let _ = arena.alloc_with(|_| {
            &SOURCE
        });
    } // grcov_ignore

    #[test]
    #[should_panic]
    fn alloc_rc_with_panic() {
        let arena = SharedArena::<usize>::new();
        const SOURCE: usize = 10;

        let _ = arena.alloc_rc_with(|_| {
            &SOURCE
        });
    } // grcov_ignore

    #[test]
    #[should_panic]
    fn alloc_arc_with_panic() {
        let arena = SharedArena::<usize>::new();
        const SOURCE: usize = 10;

        let _ = arena.alloc_arc_with(|_| {
            &SOURCE
        });
    } // grcov_ignore

    #[test]
    fn alloc_fns() {
        let arena = SharedArena::<usize>::new();

        use std::ptr;

        let a = arena.alloc_with(|place| unsafe {
            ptr::copy(&101, place.as_mut_ptr(), 1);
            &*place.as_mut_ptr()
        });
        assert!(*a == 101);

        let a = arena.alloc_arc_with(|place| unsafe {
            ptr::copy(&102, place.as_mut_ptr(), 1);
            &*place.as_mut_ptr()
        });
        assert!(*a == 102);

        let a = arena.alloc_rc_with(|place| unsafe {
            ptr::copy(&103, place.as_mut_ptr(), 1);
            &*place.as_mut_ptr()
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
            let arena = SharedArena::<usize>::new();

            use std::ptr;

            let a = arena.alloc_with(|place| unsafe {
                ptr::copy(&101, place.as_mut_ptr(), 1);
                &*place.as_mut_ptr()
            });
            let b = arena.alloc_arc_with(|place| unsafe {
                ptr::copy(&102, place.as_mut_ptr(), 1);
                &*place.as_mut_ptr()
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

        let arena = Arc::new(SharedArena::<usize>::default());

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

                // let mut nshrink = 0;

                let mut values = Vec::with_capacity(nallocs);
                for i in 0..(nallocs) {
                    values.push(arena.alloc(1));
                    let rand = get_random_number(values.len());
                    if (i + 1) % 5 == 0 {
                        values.remove(rand);
                    }
                    if with_shrink && rand % 200 == 0 {
                        if arena.shrink_to_fit() {
                            // nshrink += 1;
                        }
                    }
                }

               // println!("NSHRINK: {}", nshrink);
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }
    }
}
