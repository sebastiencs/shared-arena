
use async_std::sync::RwLock;

use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::page::{Page, IndexInPage};
use super::arena_arc::ArenaArc;

use std::cell::{RefCell, Ref, RefMut};

pub struct SharedArena<T: Sized> {
    last_found: AtomicUsize,
    first_pages: AtomicBool,
    pages1: RefCell<Vec<Arc<Page<T>>>>,
    pages2: RefCell<Vec<Arc<Page<T>>>>,
    pages: RwLock<Vec<Arc<Page<T>>>>,
    pushing: AtomicBool,
}

impl<T: Sized> SharedArena<T> {
    fn borrow_pages(&self, first_pages: bool) -> Ref<Vec<Arc<Page<T>>>> {
        match first_pages {
            true => self.pages1.borrow(),
            _ => self.pages2.borrow()
        }
    }

    fn borrow_pages_mut(&self, first_pages: bool) -> RefMut<Vec<Arc<Page<T>>>> {
        match first_pages {
            true => self.pages1.borrow_mut(),
            _ => self.pages2.borrow_mut()
        }
    }

    async fn find_place(&self) -> (Arc<Page<T>>, IndexInPage) {
        let first_pages = self.first_pages.load(Ordering::Acquire);

        loop {
            let pages = self.borrow_pages(first_pages);

            let pages_len = {
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

                pages_len
            };

            if self.pushing.compare_exchange_weak(
                false, true, Ordering::SeqCst, Ordering::Relaxed
            ).is_err() {
                continue;
            };

            {
                let mut other_pages = self.borrow_pages_mut(!first_pages);

                let ptr = pages.as_ptr();
                let other_ptr_mut = other_pages.as_mut_ptr();

                if other_pages.capacity() <= pages_len {
                    // Avoid dropping old values
                    unsafe { other_pages.set_len(0) };
                    *other_pages = Vec::with_capacity(pages_len * 2);
                }

                unsafe {
                    std::ptr::copy_nonoverlapping(ptr, other_ptr_mut, pages_len);
                    other_pages.set_len(pages_len);
                };
                other_pages.push(Arc::new(Page::new()));

            }

            self.first_pages.store(!first_pages, Ordering::Release);

            self.pushing.store(false, Ordering::Release);
        }

        // We didn't find empty space in our pages
        // We lock the arena and allocate a new page

        let mut pages = self.pages.write().await;

        if pages.len() > 10 {
        //if pages.len() > pages_len {
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
        let mut pages1 = Vec::with_capacity(1);
        pages1.push(Arc::new(Page::new()));
        SharedArena {
            pages: RwLock::new(pages),
            last_found: AtomicUsize::new(0),
            first_pages: AtomicBool::new(true),
            pages1: RefCell::new(pages1),
            pages2: RefCell::new(Vec::new()),
            pushing: AtomicBool::new(false),
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
            let pages = self.pages.read().await;

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
