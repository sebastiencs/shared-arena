
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::*};

use crate::cache_line::CacheAligned;

use super::page::{Page, IndexInPage, NODE_PER_PAGE};
use super::arena_arc::ArenaArc;

use crossbeam_epoch::{self as epoch, Owned, Shared, Guard};

pub struct SharedArena<T: Sized> {
    last_found: CacheAligned<AtomicUsize>,
    pages: CacheAligned<epoch::Atomic<Vec<Arc<Page<T>>>>>,
    writer: CacheAligned<AtomicBool>,
}

struct WriterGuard<'a> {
    writer: &'a AtomicBool
}

impl WriterGuard<'_> {
    fn new(writer: &AtomicBool) -> Option<WriterGuard> {
        if writer.compare_exchange_weak(false, true, SeqCst, Relaxed).is_err() {
            return None;
        };

        Some(WriterGuard { writer })
    }
}

impl Drop for WriterGuard<'_> {
    fn drop(&mut self) {
        self.writer.store(false, SeqCst);
    }
}

impl<T: Sized> SharedArena<T> {
    /// Ensure that a single thread is running `fun` at a time
    fn with_single_writer(&self, pages: &[Arc<Page<T>>], guard: &epoch::Guard, fun: impl Fn()) {
        let _writer_guard = match WriterGuard::new(&self.writer) {
            Some(guard) => guard,
            _ => return
        };

        {
            let current_pages = self.pages.load(Acquire, guard);
            let current_pages = unsafe { current_pages.as_ref().unwrap() };
            if pages.as_ptr() != current_pages.as_ptr() {
                // pages has already been updated
                return;
            }
        }

        fun();
    }

    fn find_place(&self) -> (Arc<Page<T>>, IndexInPage) {

        let guard = epoch::pin();

        loop {
            let shared_pages = self.pages.load(Acquire, &guard);

            let pages = unsafe { shared_pages.as_ref().unwrap() };
            let pages_len = pages.len();

            let last_found = self.last_found.load(Acquire).min(pages_len.max(1) - 1);

            let (before, after) = pages.split_at(last_found);

            for (index, page) in after.iter().chain(before).enumerate() {
                if let Some(node) = page.acquire_free_node() {
                    self.last_found.store((last_found + index + 1) % pages_len, Release);
                    return (page.clone(), node);
                };
            }

            // At this point, we didn't find empty space in our pages
            // We have to allocate new pages and we allow only 1 thread
            // to do it.
            // We should reach this point very occasionally, never if the
            // arena has been created the right capacity (Self::with_capacity)
            // If other threads reach this point, they will continue the loop
            // and search for empty spaces, there might be some available
            // since the last check
            // Another way to deal with this it to make other threads spin loop
            // until self.pages has changed

            self.with_single_writer(pages, &guard, || {
                // Double the number of pages
                let new_len = pages_len * 2;

                self.alloc_new_pages(shared_pages, pages, new_len, &guard);
            });
        }
    }

    fn alloc_new_pages(
        &self,
        shared_pages: Shared<'_, Vec<Arc<Page<T>>>>,
        pages: &[Arc<Page<T>>],
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
        new_pages.resize_with(new_len, Default::default);

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

        // Deferre the drop because other thread might still read that Vec
        self.free_deferred(shared_pages, Some(0), &guard);

        true
    }

    fn free_deferred(&self, obj: Shared<'_, Vec<Arc<Page<T>>>>, set_len: Option<usize>, guard: &Guard) {
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

    pub fn clear(&self) {
        let guard = epoch::pin();

        let mut new_pages = Vec::with_capacity(1);
        new_pages.resize_with(1, Default::default);

        let old = self.pages.swap(Owned::new(new_pages), AcqRel, &guard);

        self.free_deferred(old, None, &guard);
    }

    pub fn resize(&self, new_size: usize) {
        let guard = epoch::pin();
        let new_size = 1.max(new_size / NODE_PER_PAGE);

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

                // Replace self.pages by new_pages
                if self.pages.compare_and_set(
                    shared_pages, Owned::new(new_pages), AcqRel, &guard
                ).is_err() {
                    // self.pages have been modified by clear, resize of find_place
                    // in another thread
                    // Retry the operation because there might be new pages to drop/copy
                    // Note: new_pages is dropped in the error of compare_and_set
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

    pub fn new() -> SharedArena<T> {
        let mut pages = Vec::with_capacity(1);
        pages.push(Default::default());

        SharedArena {
            last_found: CacheAligned::new(AtomicUsize::new(0)),
            writer: CacheAligned::new(AtomicBool::new(false)),
            pages: CacheAligned::new(epoch::Atomic::new(pages)),
        }
    }

    pub fn alloc(&self, value: T) -> ArenaArc<T> {
        let (page, node) = self.find_place();

        let ptr = page.nodes[node.0].value.get();
        unsafe { std::ptr::write(ptr, value); }

        ArenaArc::new(page, node)
    }

    pub unsafe fn alloc_with<Fun>(&self, fun: Fun) -> ArenaArc<T>
    where
        Fun: Fn(&mut T)
    {
        let (page, node) = self.find_place();

        let v = page.nodes[node.0].value.get();
        fun(&mut *v);

        ArenaArc::new(page, node)
    }

    pub fn alloc_maybeuninit<Fun>(&self, fun: Fun) -> ArenaArc<T>
    where
        Fun: Fn(&mut MaybeUninit<T>)
    {
        let (page, node) = self.find_place();

        let v = page.nodes[node.0].value.get();
        fun(unsafe { &mut *(v as *mut std::mem::MaybeUninit<T>) });

        ArenaArc::new(page, node)
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

        let pages = async_std::task::block_on(async {
            let guard = epoch::pin();

            let pages = self.pages.load(Acquire, &guard);
            let pages = unsafe { pages.as_ref().unwrap() };

            let mut vec = Vec::with_capacity(pages.len());

            for page in pages.iter() {
                let used = page.bitfield
                               .iter()
                               .map(|b| b.load(Relaxed).count_zeros() as usize)
                               .sum::<usize>();
                vec.push(Page {
                    used,
                    free: super::page::NODE_PER_PAGE - used,
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
