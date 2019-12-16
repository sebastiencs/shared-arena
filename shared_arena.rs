
use async_std::sync::RwLock;

use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{compiler_fence, fence, AtomicBool, AtomicUsize, Ordering};

use super::page::{Page, IndexInPage};
use super::arena_arc::ArenaArc;

use std::cell::{RefCell, Ref, RefMut};
use std::cell::UnsafeCell;

pub struct SharedArena<T: Sized> {
    last_found: AtomicUsize,
    use_copy: AtomicBool,
    pages: UnsafeCell<Vec<Arc<Page<T>>>>,
    pages_copy: UnsafeCell<Vec<Arc<Page<T>>>>,
    // pages: RwLock<Vec<Arc<Page<T>>>>,
    writer: AtomicBool,
}

struct WriterGuard<'a> {
    writer: &'a AtomicBool
}

impl WriterGuard<'_> {
    fn new(writer: &AtomicBool) -> Option<WriterGuard> {
        if writer.compare_exchange_weak(
            false, true, Ordering::SeqCst, Ordering::Relaxed
        ).is_err() {
            return None;
        };

        Some(WriterGuard { writer })
    }
}

impl Drop for WriterGuard<'_> {
    fn drop(&mut self) {
        self.writer.store(false, Ordering::SeqCst);
    }
}

impl<T: Sized> SharedArena<T> {
    fn borrow_pages(&self, use_copy: bool) -> &[Arc<Page<T>>] {
        let pages = if use_copy {
            &self.pages
        } else {
            &self.pages_copy
        };
        unsafe { &*pages.get() }
    }

    #[allow(clippy::mut_from_ref)]
    fn borrow_pages_mut(&self, use_copy: bool) -> &mut Vec<Arc<Page<T>>> {
        let pages = if use_copy {
            &self.pages
        } else {
            &self.pages_copy
        };
        unsafe { &mut *pages.get() }
    }

    fn with_single_writer<F>(&self, use_copy: bool, fun: F)
    where
        F: Fn(&mut Vec<Arc<Page<T>>>)
    {
        let _guard = match WriterGuard::new(&self.writer) {
            Some(guard) => guard,
            _ => return
        };

        if self.use_copy.load(Ordering::Acquire) != use_copy {
            return;
        }

        let other_pages = self.borrow_pages_mut(!use_copy);

        fun(other_pages);

        self.use_copy.store(!use_copy, Ordering::Release);
    }

    async fn find_place(&self) -> (Arc<Page<T>>, IndexInPage) {

        loop {
            let use_copy = self.use_copy.load(Ordering::Acquire);

            let (pages_ptr, pages_len) = {
                let pages = self.borrow_pages(use_copy);

                // let pages = self.pages.read().await;
                let pages_len = pages.len();

                let last_found = self.last_found.load(Ordering::Acquire);

                let (before, after) = pages.split_at(last_found);

                for (index, page) in after.iter().chain(before).enumerate() {
                    if let Some(node) = page.acquire_free_node() {
                        self.last_found.store((last_found + index + 1) % pages_len, Ordering::Release);
                        return (page.clone(), node);
                    };
                }

                (pages.as_ptr(), pages_len)
            };

            self.with_single_writer(use_copy, |other_pages| {
                unsafe {
                    if other_pages.capacity() <= pages_len {
                        // Avoid dropping old values
                        other_pages.set_len(0);
                        *other_pages = Vec::with_capacity(pages_len * 2);
                    }

                    let other_ptr_mut = other_pages.as_mut_ptr();

                    std::ptr::copy_nonoverlapping(pages_ptr, other_ptr_mut, pages_len);
                    other_pages.set_len(pages_len);
                };

                other_pages.push(Default::default());
            });
        }
    }

    pub fn clear(&self) {
        let use_copy = self.use_copy.load(Ordering::Acquire);

        self.with_single_writer(use_copy, |other_pages| {
            // let other_ptr_mut = other_pages.as_mut_ptr();
            // std::ptr::copy_nonoverlapping(pages_ptr, other_ptr_mut, pages_len);

            other_pages.clear();
        });
    }

    pub fn new() -> SharedArena<T> {
        // let mut pages = Vec::with_capacity(1);
        // pages.push(Default::default());
        let mut pages = Vec::with_capacity(1);
        pages.push(Default::default());
        SharedArena {
            // pages: RwLock::new(pages),
            last_found: AtomicUsize::new(0),
            use_copy: AtomicBool::new(true),
            pages: UnsafeCell::new(pages),
            pages_copy: UnsafeCell::new(Vec::new()),
            writer: AtomicBool::new(false),
        }
    }

    pub async fn check_empty(&self) {
        // for (index, page) in self.pages.read().await.iter().enumerate() {
        //     println!("PAGE {} FREE {}", index, page.nfree.load(Ordering::Relaxed));
        // }
    }

    pub async fn alloc(&self, value: T) -> ArenaArc<T> {
        let (page, node) = self.find_place().await;

        let ptr = page.nodes[node.0].value.get();
        unsafe {
            std::ptr::write(ptr, value);
        }

        ArenaArc::new(page, node)
    }

    pub async unsafe fn alloc_with<Fun>(&self, fun: Fun) -> ArenaArc<T>
    where
        Fun: Fn(&mut T)
    {
        let (page, node) = self.find_place().await;

        let v = page.nodes[node.0].value.get();
        fun(&mut *v);

        ArenaArc::new(page, node)
    }

    pub async fn alloc_maybeuninit<Fun>(&self, fun: Fun) -> ArenaArc<T>
    where
        Fun: Fn(&mut MaybeUninit<T>)
    {
        let (page, node) = self.find_place().await;

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
            let use_copy = self.use_copy.load(Ordering::Acquire);

            let pages = self.borrow_pages(use_copy);

            // let pages = self.pages.read().await;

            let mut vec = Vec::with_capacity(pages.len());
            for page in pages.iter() {
                let used = page.bitfield
                               .iter()
                               .map(|b| b.load(Ordering::Relaxed).count_zeros() as usize)
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
