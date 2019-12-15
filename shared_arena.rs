
use async_std::sync::RwLock;

use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::page::{Page, IndexInPage};
use super::arena_arc::ArenaArc;

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
                    self.last_found.store((last_found + index + 1) % pages_len, Ordering::Release);
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
            // acquiring: bool
        }

        impl std::fmt::Debug for Page {
            fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "Page {{ free: {} used: {} }}", self.free, self.used)
            }
        }

        let pages = async_std::task::block_on(async {
            let pages = self.pages.read().await;

            let mut vec = Vec::with_capacity(pages.len());
            for page in pages.iter() {
                let used = page.bitfield.load(Ordering::Relaxed).count_zeros() as usize;
                // let free = page.nfree.load(Ordering::Relaxed);
                // let acquiring = page.acquiring.load(Ordering::Relaxed);
                vec.push(Page {
                    // free,
                    // acquiring,
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

    // pub async fn check_empty(&self) {
    //     for (index, page) in self.pages.read().await.iter().enumerate() {
    //         println!("PAGE {} FREE {}", index, page.nfree.load(Ordering::Relaxed));
    //     }
    // }
