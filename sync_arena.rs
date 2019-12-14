
use async_std::sync::RwLock;

use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{Page, PoolArc, IndexInPage};

pub struct SharedArena<T: Sized> {
    last_found: AtomicUsize,
    pages: RwLock<Vec<Arc<Page<T>>>>,
}

impl<T: Sized> SharedArena<T> {
    async fn find_place(&self) -> (Arc<Page<T>>, IndexInPage) {
        let pages_len = {
            let pages = self.pages.read().await;
            let pages_len = pages.len();
            let last_found = self.last_found.load(Ordering::Acquire);

            let (before, after) = pages.split_at(last_found);

            for (index, page) in after.iter().chain(before).enumerate() {
                if let Some(node) = page.acquire_free_node() {
                    self.last_found.store((last_found + index) % pages_len, Ordering::Release);
                    return (page.clone(), node);
                };
            }

            pages_len
        };

        // We didn't find empty space in our pages
        // We lock the arena and allocate a new page

        let mut pages = self.pages.write().await;

        if pages.len() > pages_len {
            // Another thread has already pushed a new page
            let last = pages.last().unwrap();
            if let Some(node) = last.acquire_free_node() {
                return (last.clone(), node);
            };
        }

        let new_page = Arc::new(Page::new());
        pages.push(new_page.clone());
        let node = new_page.acquire_free_node().unwrap();

        self.last_found.store(pages.len() - 1, Ordering::Release);

        (new_page, node)
    }

    pub fn new() -> SharedArena<T> {
        let mut pages = Vec::with_capacity(32);
        pages.push(Arc::new(Page::new()));
        SharedArena {
            pages: RwLock::new(pages),
            last_found: AtomicUsize::new(0)
        }
    }

    pub async fn check_empty(&self) {
        for (index, page) in self.pages.read().await.iter().enumerate() {
            println!("PAGE {} FREE {}", index, page.nfree.load(Ordering::Relaxed));
        }
    }

    pub async fn alloc(&mut self, value: T) -> PoolArc<T> {
        let (page, node) = self.find_place().await;

        let ptr = page.nodes[node.0].value.get();
        unsafe {
            std::ptr::write(ptr, value);
        }

        PoolArc::new(page, node)
    }

    pub async unsafe fn alloc_with<Fun>(&mut self, fun: Fun) -> PoolArc<T>
    where
        Fun: Fn(&mut T)
    {
        let (page, node) = self.find_place().await;

        let v = page.nodes[node.0].value.get();
        fun(&mut *v);

        PoolArc::new(page, node)
    }

    pub async fn alloc_maybeuninit<Fun>(&mut self, fun: Fun) -> PoolArc<T>
    where
        Fun: Fn(&mut MaybeUninit<T>)
    {
        let (page, node) = self.find_place().await;

        let v = page.nodes[node.0].value.get();
        fun(unsafe { &mut *(v as *mut std::mem::MaybeUninit<T>) });

        PoolArc::new(page, node)
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
        let len = async_std::task::block_on(async {
           self.pages.read().await.len()
        });
        f.debug_struct("MemPool")
         .field("npages", &len)
         .finish()
    }
}
