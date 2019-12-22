
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::*};
use std::ptr::NonNull;

use crate::cache_line::CacheAligned;

use super::page::{Page, BLOCK_PER_PAGE, Block};
use super::arena_arc::ArenaArc;
use super::arena_box::ArenaBox;

use crossbeam_epoch::{self as epoch, Owned, Shared, Guard};

/// An arena shareable across threads
///
/// Pointers to the elements in the SharedArena are shareable as well.
pub struct SharedArena<T: Sized> {
    last_found: CacheAligned<AtomicUsize>,
    pages: CacheAligned<epoch::Atomic<Vec<NonNull<Page<T>>>>>,
    writer: CacheAligned<AtomicBool>,
}

struct WriterGuard<'a> {
    writer: &'a AtomicBool
}

impl WriterGuard<'_> {
    fn new(writer: &AtomicBool) -> Option<WriterGuard> {
        if writer.compare_exchange(false, true, AcqRel, Relaxed).is_err() {
            return None;
        };

        Some(WriterGuard { writer })
    }
}

impl Drop for WriterGuard<'_> {
    fn drop(&mut self) {
        self.writer.store(false, Release);
    }
}

impl<T: Sized> SharedArena<T> {
    /// Ensure that a single thread is running `fun` at a time
    fn with_single_writer(&self, pages: Shared<Vec<NonNull<Page<T>>>>, guard: &epoch::Guard, fun: impl Fn()) {
        let _writer_guard = match WriterGuard::new(&self.writer) {
            Some(guard) => guard,
            _ => return
        };

        if pages != self.pages.load(Acquire, guard) {
            // self.pages has already been updated
            return;
        }

        fun();
    }

    fn find_place(&self) -> (NonNull<Page<T>>, NonNull<Block<T>>) {

        let guard = epoch::pin();

        loop {
            let last_found = self.last_found.load(Relaxed);
            let shared_pages = self.pages.load(Acquire, &guard);

            let pages = unsafe { shared_pages.as_ref().unwrap() };
            let pages_len = pages.len();

            let (before, after) = pages.split_at(last_found % pages_len);

            for (index, page) in after.iter().chain(before).enumerate() {
                if let Some(block) = unsafe { page.as_ref() }.acquire_free_block() {
                    self.last_found.store(last_found + index, Relaxed);
                    return (*page, block);
                };
            }

            println!("SHARED: ALLOCATING MORE {:#?}", self);

            // At this point, we didn't find empty space in our pages
            // We have to allocate new pages and we allow only 1 thread
            // to do it.
            // We should reach this point very occasionally, never if the
            // arena has been created with the right capacity (Self::with_capacity)
            // If other threads reach this point, they will continue the loop
            // and search for empty spaces, there might be some available
            // since the last check
            // Another way to deal with this it to make other threads spin loop
            // until self.pages has changed

            self.with_single_writer(shared_pages, &guard, || {
                // Double the number of pages
                let new_len = pages_len * 2;

                self.alloc_new_pages(shared_pages, pages, new_len, &guard);
            });
        }
    }

    fn alloc_new_pages(
        &self,
        shared_pages: Shared<'_, Vec<NonNull<Page<T>>>>,
        pages: &[NonNull<Page<T>>],
        new_len: usize,
        guard: &Guard
    ) -> bool {
        let pages_len = pages.len();
        let new_len = new_len.max(1);

        let mut new_pages = Vec::with_capacity(new_len);

        let pages_ptr = pages.as_ptr();
        let new_pages_ptr = new_pages.as_mut_ptr();

        unsafe {
            // memcpy the old pages
            std::ptr::copy_nonoverlapping(pages_ptr, new_pages_ptr, pages_len);
            new_pages.set_len(pages_len);
        }

        // Fill the rest with new pages
        new_pages.resize_with(new_len, Page::<T>::new);

        // Replace self.pages by our new Vec
        if self.pages.compare_and_set(
            shared_pages, Owned::new(new_pages), AcqRel, &guard
        ).is_err() {
            // Pages have been updated by another thread
            // - When called from find_place, this might occurs only when
            //   a thread called clear or resize, we abort the operation
            // - When called from resize, this occurs when another thread
            //   called find_place or clear, we have to reiterate the operation
            return false;
        }

        // Defer the drop because other threads might still be reading that Vec
        unsafe {
            guard.defer_destroy(shared_pages)
        };

        true
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
    ///
    /// [`alloc`]: struct.SharedArena.html#method.alloc
    /// [`alloc_with`]: struct.SharedArena.html#method.alloc_with
    /// [`alloc_maybeuninit`]: struct.SharedArena.html#method.alloc_maybeuninit
    pub fn new() -> SharedArena<T> {
        Self::with_capacity(1)
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
    ///
    /// [`alloc`]: struct.SharedArena.html#method.alloc
    /// [`alloc_with`]: struct.SharedArena.html#method.alloc_with
    /// [`alloc_maybeuninit`]: struct.SharedArena.html#method.alloc_maybeuninit
    pub fn with_capacity(cap: usize) -> SharedArena<T> {
        let npages = ((cap.max(1) - 1) / BLOCK_PER_PAGE) + 1;

        let mut pages = Vec::with_capacity(npages);
        pages.resize_with(npages, Page::<T>::new);

        SharedArena {
            last_found: CacheAligned::new(AtomicUsize::new(0)),
            writer: CacheAligned::new(AtomicBool::new(false)),
            pages: CacheAligned::new(epoch::Atomic::new(pages)),
        }
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

    /// Clears the arena and delete its pages.
    ///
    /// After calling this function, the arena will have a capacity
    /// of 63 elements (1 page).
    ///
    /// Note that all [`ArenaArc`] and [`ArenaBox`] created before this
    /// function will still be valid.
    ///
    /// ## Example
    ///
    /// ```
    /// use shared_arena::SharedArena;
    ///
    /// let arena = SharedArena::with_capacity(100);
    /// let mut values = Vec::new();
    ///
    /// for i in 0..100 {
    ///     values.push(arena.alloc(i));
    /// }
    ///
    /// arena.clear();
    ///
    /// for i in 0..100 {
    ///     // pointers are still valid
    ///     println!("values[{}] = {:?}", i, values[i]);
    /// }
    ///
    /// ```
    ///
    /// [`ArenaBox`]: ./struct.ArenaBox.html
    /// [`ArenaArc`]: ./struct.ArenaArc.html
    pub fn clear(&self) {
        let guard = epoch::pin();

        let new_pages = vec![Page::<T>::new()];

        let old = self.pages.swap(Owned::new(new_pages), AcqRel, &guard);

        unsafe {
            guard.defer_unchecked(move || {
                // Drop the vec and its pages
                let mut old = old.into_owned();
                for elem in old.iter_mut() {
                    std::ptr::drop_in_place(elem.as_mut());
                }
            })
        }
    }

    /// Resizes the arena to hold at least `new_cap` elements.
    ///
    /// Because the arena allocates by page of 63 elements, it might
    /// be able to hold more elements than `new_cap`.
    ///
    /// If `new_cap` is greater than the current size, new pages will be
    /// allocated.
    ///
    /// If `new_cap` is smaller, the arena will be truncated.
    ///
    /// Note that even if `new_cap` is smaller than the current size of
    /// the arena, all [`ArenaArc`] and [`ArenaBox`] created before this
    /// function will still be valid.
    ///
    /// ## Example
    ///
    /// ```
    /// use shared_arena::SharedArena;
    ///
    /// let arena = SharedArena::with_capacity(100);
    /// arena.resize(200);
    /// ```
    ///
    /// [`ArenaBox`]: ./struct.ArenaBox.html
    /// [`ArenaArc`]: ./struct.ArenaArc.html
    #[allow(clippy::comparison_chain)]
    pub fn resize(&self, new_cap: usize) {
        let guard = epoch::pin();
        let new_size = ((new_cap.max(1) - 1) / BLOCK_PER_PAGE) + 1;

        loop {
            let shared_pages = self.pages.load(Acquire, &guard);
            let pages = unsafe { shared_pages.as_ref().unwrap() };

            let pages_len = pages.len();

            if new_size > pages_len {
                let succeed = self.alloc_new_pages(shared_pages, pages, new_size, &guard);

                if !succeed {
                    // self.pages have been modified by clear, resize of find_place
                    // in another thread
                    // Retry the operation because there might be new pages to drop/copy
                    continue;
                }
            } else if new_size < pages_len {
                // We have to drop the elements between new_size..pages_len

                let to_drop_len = pages_len - new_size;

                // We can't modify self.pages directly because other threads might
                // be reading it at the same time
                // So we have to create a new Vec and then set self.pages
                let mut new_pages = Vec::with_capacity(new_size);
                let mut to_drop = Vec::with_capacity(to_drop_len);

                let pages_ptr = pages.as_ptr();
                let to_drop_ptr = to_drop.as_mut_ptr();
                let new_pages_ptr = new_pages.as_mut_ptr();

                unsafe {
                    // Copy the content to drop in to_drop
                    std::ptr::copy_nonoverlapping(pages_ptr.add(new_size), to_drop_ptr, to_drop_len);
                    to_drop.set_len(to_drop_len);

                    // Copy the content to keep in new_pages
                    std::ptr::copy_nonoverlapping(pages_ptr, new_pages_ptr, new_size);
                    new_pages.set_len(new_size);
                }

                if self.pages.compare_and_set(
                    shared_pages, Owned::new(new_pages), AcqRel, &guard
                ).is_err() {
                    // self.pages have been modified by clear, resize of find_place
                    // in another thread
                    // Retry the operation because there might be new pages to drop/copy
                    continue;
                }

                unsafe {
                    guard.defer_unchecked(move || {
                        // Drop the vec and its elements
                        for elem in &mut to_drop {
                            std::ptr::drop_in_place(elem.as_mut());
                        }
                        let owned = shared_pages.into_owned();
                        std::mem::drop(to_drop);
                        std::mem::drop(owned);
                    })
                }

            }

            return;
        }
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
        let guard = epoch::pin();

        loop {
            let shared_pages = self.pages.load(Acquire, &guard);
            let pages = unsafe { shared_pages.as_ref() }.unwrap();

            #[allow(clippy::type_complexity)]
            let (used, mut unused): (Vec<NonNull<Page<T>>>, Vec<NonNull<Page<T>>>) = pages
                .iter()
                .partition(|p| {
                    unsafe { p.as_ref() }.bitfield.load(Relaxed) == !0
                });

            if self.pages.compare_and_set(shared_pages, Owned::new(used), AcqRel, &guard).is_err() {
                // self.pages has been updated
                // Restart the operation because there might be new/removed block in it
                continue;
            }

            unsafe {
                guard.defer_unchecked(move || {
                    // Drop the old vec
                    let old = shared_pages.into_owned();
                    std::mem::drop(old);
                    // Drop the unused pages
                    for elem in unused.iter_mut() {
                        std::ptr::drop_in_place(elem.as_mut());
                    }
                    std::mem::drop(unused);
                })
            }

            return;
        }
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
        let guard = epoch::pin();

        let pages = self.pages.load(Acquire, &guard);
        let pages = unsafe { pages.as_ref().unwrap() };
        let pages_len = pages.len();

        let used = pages.iter()
                        .map(|p| unsafe { p.as_ref() }.bitfield.load(Relaxed).count_zeros() as usize)
                        .sum::<usize>();

        let free = (pages_len * BLOCK_PER_PAGE) - used;

        (used, free)
    }
}

unsafe impl<T: Sized> Send for SharedArena<T> {}
unsafe impl<T: Sized> Sync for SharedArena<T> {}

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

        let guard = epoch::pin();

        let pages = self.pages.load(Acquire, &guard);
        let pages = unsafe { pages.as_ref().unwrap() };

        let mut vec = Vec::with_capacity(pages.len());

        for page in pages.iter() {
            let used = unsafe { page.as_ref() }.bitfield.load(Relaxed).count_zeros() as usize;
            vec.push(Page {
                used,
                free: super::page::BLOCK_PER_PAGE - used,
            });
        }

        let blocks_used: usize = vec.iter().map(|p| p.used).sum();
        let blocks_free: usize = vec.iter().map(|p| p.free).sum();

        f.debug_struct("MemPool")
         .field("blocks_free", &blocks_free)
         .field("blocks_used", &blocks_used)
         .field("npages", &pages.len())
         .field("pages", &vec)
         .finish()
    }
}
