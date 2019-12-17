
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::*};

use super::page::{Page, IndexInPage};
use super::arena_arc::ArenaArc;

use crossbeam_epoch::{self as epoch, Owned, Shared, Guard};

pub struct SharedArena<T: Sized> {
    last_found: AtomicUsize,
    pages: epoch::Atomic<Vec<Arc<Page<T>>>>,
    writer: AtomicBool,
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

            let last_found = self.last_found.load(Acquire);

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
            // We should reach this point very occasionally.

            self.with_single_writer(pages, &guard, || {

                // Double the number of pages
                let new_len = 1.max(pages_len * 2);

                println!("WRITER {}", new_len);

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

                if self.pages.compare_and_set(
                    shared_pages, Owned::new(new_pages), AcqRel, &guard
                ).is_err() {
                    // Pages have been updated by Self::clear()
                    return;
                }

                self.free_deferred(shared_pages, Some(0), &guard);
            });
        }
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

        self.last_found.store(0, Release);

        let old = self.pages.swap(Owned::new(new_pages), AcqRel, &guard);

        self.free_deferred(old, None, &guard);
    }

    pub fn new() -> SharedArena<T> {
        let mut pages = Vec::with_capacity(1);
        pages.push(Default::default());

        SharedArena {
            last_found: AtomicUsize::new(0),
            writer: AtomicBool::new(false),
            pages: epoch::Atomic::new(pages),
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
