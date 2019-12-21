
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, AtomicPtr, Ordering::*};
use std::ptr::NonNull;

use crate::cache_line::CacheAligned;

use super::page::{Page, IndexInPage, BLOCK_PER_PAGE, Block};
use super::arena_arc::ArenaArc;
use super::arena_box::ArenaBox;

use crossbeam_epoch::{self as epoch, Owned, Shared, Guard};

struct VecShared<T> {
    vec: Vec<NonNull<Page<T>>>,
    counter: AtomicUsize,
}

pub struct NewSharedArena<T: Sized> {
    last_found: CacheAligned<AtomicUsize>,
    pages: CacheAligned<AtomicPtr<VecShared<T>>>,
    writer: CacheAligned<AtomicBool>,
}

struct WriterGuard<'a> {
    writer: &'a AtomicBool
}

impl WriterGuard<'_> {
    fn new(writer: &AtomicBool) -> Option<WriterGuard> {
        if writer.compare_exchange(false, true, SeqCst, Relaxed).is_err() {
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

impl<T: Sized> NewSharedArena<T> {
    /// Ensure that a single thread is running `fun` at a time
    fn with_single_writer(&self, pages_ptr: *mut VecShared<T>, fun: impl Fn()) {
        let _writer_guard = match WriterGuard::new(&self.writer) {
            Some(guard) => guard,
            _ => return
        };

        {
            let current_pages = self.pages.load(Acquire);
            if pages_ptr != current_pages {
                // pages has already been updated
                return;
            }
        }

        fun();
    }

    fn find_place(&self) -> (NonNull<Page<T>>, NonNull<Block<T>>) {

        // let guard = epoch::pin();

        loop {

            let mut last_found = self.last_found.load(Relaxed);

            let (pages_ptr, pages_len) = 'valid_ptr: loop {

                let ptr_loaded = coarsetime::Instant::now();

                let pages_ptr = &self.pages.load(Acquire);

                let pages = &pages_ptr.vec;
                let pages_len = pages.len();

                last_found = last_found % pages_len;

                let (before, after) = pages.split_at(last_found);

                if ptr_loaded.elapsed().as_secs() > 1 {
                    continue 'valid_ptr;
                }

                for (index, page) in after.iter().chain(before).enumerate() {
                    if let Some(block) = unsafe { page.as_ref() }.acquire_free_block() {
                        self.last_found.store(last_found + index, Relaxed);
                        //self.last_found.store((last_found + index + 1) % pages_len, Release);
                        return (*page, block);
                    };

                    if ptr_loaded.elapsed().as_secs() > 1 {
                        last_found += index;
                        continue 'valid_ptr;
                    }
                }

                (pages_ptr, pages_len)
            };

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

            self.with_single_writer(pages_ptr, || {
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

        // Defer the drop because other thread might still read that Vec
        self.drop_deferred(shared_pages, Some(0), &guard);

        true
    }

    fn drop_deferred(&self, obj: Shared<'_, Vec<NonNull<Page<T>>>>, set_len: Option<usize>, guard: &Guard) {
        unsafe {
            guard.defer_unchecked(move || {
                let mut owned = obj.into_owned();
                if let Some(set_len) = set_len {
                    // We drop the Vec but not the pages because we just
                    // memcpy them.
                    // set_len is None when we want to drop its content
                    owned.set_len(set_len);
                };
            });
        }
    }

    /// Clears the arena and delete its pages.
    ///
    /// After calling this function, the arena will have a capacity
    /// of 32 elements (1 page).
    ///
    /// Note that all [`ArenaArc`] and [`ArenaBox`] created before this
    /// function will still be valid.
    ///
    /// ## Example
    ///
    /// ```
    /// use shared_arena::NewSharedArena;
    ///
    /// let arena = NewSharedArena::with_capacity(100);
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

        self.drop_deferred(old, None, &guard);
    }

    /// Resizes the arena to hold at least `new_cap` elements.
    ///
    /// Because the arena allocates by page of 32 elements, it might
    /// hold more elements than `new_cap`.
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
    /// use shared_arena::NewSharedArena;
    ///
    /// let arena = NewSharedArena::with_capacity(100);
    /// arena.resize(200);
    /// ```
    ///
    /// [`ArenaBox`]: ./struct.ArenaBox.html
    /// [`ArenaArc`]: ./struct.ArenaArc.html
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
                        std::mem::drop(to_drop);
                        let mut owned = shared_pages.into_owned();
                        // Drop the vec only, its elements have been memcpy
                        owned.set_len(0);
                    })
                }

            }

            return;
        }
    }

    /// Constructs a new NewSharedArena capable of holding exactly 32 elements
    ///
    /// The Arena will reallocate itself if there is not enough space
    /// when allocating (with alloc* functions)
    ///
    /// ## Example
    ///
    /// ```
    /// use shared_arena::NewSharedArena;
    ///
    /// let arena = NewSharedArena::new();
    /// ```
    ///
    /// [`alloc`]: struct.NewSharedArena.html#method.alloc
    /// [`alloc_with`]: struct.NewSharedArena.html#method.alloc_with
    /// [`alloc_maybeuninit`]: struct.NewSharedArena.html#method.alloc_maybeuninit
    pub fn new() -> NewSharedArena<T> {
        Self::with_capacity(1)
    }

    /// Constructs a new NewSharedArena capable of holding at least `cap` elements
    ///
    /// Because the arena allocate by page of 32 elements, it might
    /// hold more elements than `cap`.
    ///
    /// The Arena will reallocate itself if there is not enough space
    /// when allocating (with alloc* functions)
    ///
    /// ## Example
    ///
    /// ```
    /// use shared_arena::NewSharedArena;
    ///
    /// let arena = NewSharedArena::with_capacity(2048);
    /// ```
    ///
    /// [`alloc`]: struct.NewSharedArena.html#method.alloc
    /// [`alloc_with`]: struct.NewSharedArena.html#method.alloc_with
    /// [`alloc_maybeuninit`]: struct.NewSharedArena.html#method.alloc_maybeuninit
    pub fn with_capacity(cap: usize) -> NewSharedArena<T> {
        let npages = ((cap.max(1) - 1) / BLOCK_PER_PAGE) + 1;

        let mut pages = Vec::with_capacity(npages);
        pages.resize_with(npages, Page::<T>::new);

        NewSharedArena {
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
    /// use shared_arena::NewSharedArena;
    ///
    /// let arena = NewSharedArena::new();
    /// let my_num: ArenaBox<u8> = arena.alloc(0xFF);
    /// ```
    ///
    /// [`ArenaBox`]: ./struct.ArenaBox.html
    pub fn alloc(&self, value: T) -> ArenaBox<T> {
        let (page, block) = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            ptr.write(value);
            ArenaBox::new(page, block)
        }
        // let ptr = page.nodes[node].value.get();
        // unsafe { std::ptr::write(ptr, value); }

        // unreachable!();
        // ArenaBox::new(unsafe { std::mem::uninitialized() }, node)
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
    /// use shared_arena::NewSharedArena;
    ///
    /// #[derive(Copy, Clone)]
    /// struct MyStruct { .. }
    ///
    /// let ref_struct: &MyStruct = ..;
    ///
    /// let arena = NewSharedArena::new();
    /// let my_struct: ArenaBox<MyStruct> = arena.alloc_in_place(|place| {
    ///     unsafe {
    ///         std::ptr::copy(ref_struct, place.as_mut_ptr(), 1);
    ///     }
    /// });
    /// ```
    ///
    /// [`ArenaBox`]: ./struct.ArenaBox.html
    /// [`alloc`]: struct.NewSharedArena.html#method.alloc
    /// [`MaybeUninit`]: https://doc.rust-lang.org/std/mem/union.MaybeUninit.html
    pub fn alloc_in_place<F>(&self, initializer: F) -> ArenaBox<T>
    where
        F: Fn(&mut MaybeUninit<T>)
    {
        let (page, block) = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            initializer(&mut *(ptr as *mut std::mem::MaybeUninit<T>));
            ArenaBox::new(page, block)
        }

        // let v = page.nodes[node].value.get();
        // initializer(unsafe { &mut *(v as *mut std::mem::MaybeUninit<T>) });

        // unreachable!();
        // ArenaBox::new(unsafe { std::mem::uninitialized() }, node)
    }

    /// Writes a value in the arena, and returns an [`ArenaArc`]
    /// pointing to that value.
    ///
    /// ## Example
    ///
    /// ```
    /// use shared_arena::NewSharedArena;
    ///
    /// let arena = NewSharedArena::new();
    /// let my_num: ArenaArc<u8> = arena.alloc_arc(0xFF);
    /// ```
    ///
    /// [`ArenaArc`]: ./struct.ArenaArc.html
    pub fn alloc_arc(&self, value: T) -> ArenaArc<T> {
        let (page, block) = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            ptr.write(value);
            ArenaArc::new(page, block)
        }
        // let ptr = page.nodes[node].value.get();
        // unsafe { std::ptr::write(ptr, value); }

        // ArenaArc::new(page, node)
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
    /// use shared_arena::NewSharedArena;
    ///
    /// #[derive(Copy, Clone)]
    /// struct MyStruct { .. }
    ///
    /// let ref_struct: &MyStruct = ..;
    ///
    /// let arena = NewSharedArena::new();
    /// let my_struct: ArenaArc<MyStruct> = arena.alloc_in_place_arc(|place| {
    ///     unsafe {
    ///         std::ptr::copy(ref_struct, place.as_mut_ptr(), 1);
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
            ArenaArc::new(page, block)
        }
        // let v = page.nodes[node].value.get();
        // initializer(unsafe { &mut *(v as *mut std::mem::MaybeUninit<T>) });

        // ArenaArc::new(page, node)
    }

    pub fn stats(&self) -> usize {
        let guard = epoch::pin();

        let pages = self.pages.load(Acquire, &guard);
        let pages = unsafe { pages.as_ref().unwrap() };

        let mut used = 0;

        for page in pages.iter() {
            used += unsafe { page.as_ref() }.bitfield.load(Relaxed).count_zeros() as usize;
        //     used += page.bitfield
        //                 .iter()
        //                 .map(|b| b.load(Relaxed).count_zeros() as usize)
        //                 .sum::<usize>();
            // }
        }

        used
    }

    // pub unsafe fn alloc_with<Fun>(&self, fun: Fun) -> ArenaArc<T>
    // where
    //     Fun: Fn(&mut T)
    // {
    //     let (page, node) = self.find_place();

    //     let v = page.nodes[node].value.get();
    //     fun(&mut *v);

    //     ArenaArc::new(page, node)
    // }
}

unsafe impl<T: Sized> Send for NewSharedArena<T> {}
unsafe impl<T: Sized> Sync for NewSharedArena<T> {}

impl<T: Sized> Default for NewSharedArena<T> {
    fn default() -> NewSharedArena<T> {
        NewSharedArena::new()
    }
}

impl<T> std::fmt::Debug for NewSharedArena<T> {
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

        let pages = async_std::task::block_on(async {
            let guard = epoch::pin();

            let pages = self.pages.load(Acquire, &guard);
            let pages = unsafe { pages.as_ref().unwrap() };

            let mut vec = Vec::with_capacity(pages.len());

            for page in pages.iter() {
                // let used = page.bitfield
                //                .iter()
                //                .map(|b| b.load(Relaxed).count_zeros() as usize)
                //                .sum::<usize>();
                let used = unsafe { page.as_ref() }.bitfield.load(Relaxed).count_zeros() as usize;
                vec.push(Page {
                    used,
                    free: super::page::BLOCK_PER_PAGE - used,
                });
            }

            vec
        });

        let nodes_used: usize = pages.iter().map(|p| p.used).sum();
        let nodes_free: usize = pages.iter().map(|p| p.free).sum();

        f.debug_struct("MemPool")
         .field("nodes_free", &nodes_free)
         .field("nodes_used", &nodes_used)
         .field("npages", &pages.len())
         .field("pages", &pages)
         .finish()
    }
}
