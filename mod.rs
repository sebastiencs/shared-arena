
use std::sync::atomic::{AtomicUsize, Ordering};
use std::ptr::NonNull;
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;

const NODE_PER_PAGE: usize = 32;
const WRONG_NODE_INDEX: usize = NODE_PER_PAGE + 1;

struct Node<T: Sized> {
    /// Read only and initialized on Node creation
    /// Doesn't need to be atomic
    index_in_page: usize,
    /// Number of references to this node
    counter: AtomicUsize,
    /// Index to the next free node in our page
    next_free: AtomicUsize,
    value: UnsafeCell<T>,
}

struct IndexInPage(usize);

impl From<usize> for IndexInPage {
    fn from(n: usize) -> IndexInPage {
        IndexInPage(n)
    }
}

impl From<IndexInPage> for usize {
    fn from(n: IndexInPage) -> usize {
        n.0
    }
}

struct Page<T: Sized> {
    /// Number of free nodes in this page
    nfree: AtomicUsize,
    /// Index to a free node in this page
    free_node: AtomicUsize,
    /// Array of nodes
    nodes: [Node<T>; NODE_PER_PAGE],
}

impl<T: Sized> Page<T> {
    fn new() -> Page<T> {

        // MaybeUninit doesn't allow field initialization :(
        // https://doc.rust-lang.org/std/mem/union.MaybeUninit.html#initializing-a-struct-field-by-field
        #[allow(clippy::uninit_assumed_init)]
        let mut nodes: [Node<T>; NODE_PER_PAGE] = unsafe { MaybeUninit::uninit().assume_init() };

        for (index, node) in nodes.iter_mut().enumerate() {
            node.index_in_page = index;
            node.counter = AtomicUsize::new(0);
            node.next_free = AtomicUsize::new(index + 1);
        }

        Page {
            nodes,
            nfree: AtomicUsize::new(NODE_PER_PAGE),
            free_node: AtomicUsize::new(0),
        }
    }

    fn next(&self, index: usize) -> usize {
        self.nodes
            .get(index)
            .map(|node| node.next_free.load(Ordering::Relaxed))
            .unwrap_or(WRONG_NODE_INDEX)
    }

    fn acquire_free_node(&self) -> Option<IndexInPage> {
        let freed = self.nfree.load(Ordering::Acquire);

        if freed == 0 {
            return None;
        }

        let mut next = self.free_node.load(Ordering::Relaxed);
        let mut next_next = self.next(next);

        while let Err(x) = self.free_node.compare_exchange_weak(
            next, next_next, Ordering::SeqCst, Ordering::Relaxed
        ) {
            next = x;
            next_next = self.next(next);
        }

        self.nodes.get(next).map(|_| next.into())
    }
}

use std::sync::Arc;

pub struct MemPool<T: Sized> {
    pages: Vec<Arc<Page<T>>>,
}

impl<T: Sized> MemPool<T> {
    fn alloc_new_page(&mut self) -> Arc<Page<T>> {
        let new_page = Arc::new(Page::new());
        self.pages.push(new_page.clone());
        new_page
    }

    fn find_place(&mut self) -> (Arc<Page<T>>, IndexInPage) {
        for page in self.pages.iter() {
            if let Some(node) = page.acquire_free_node() {
                return (page.clone(), node);
            };
        }

        let new_page = self.alloc_new_page();
        let node = new_page.acquire_free_node().unwrap();

        (new_page, node)
    }

    pub fn new() -> MemPool<T> {
        let mut pages = Vec::with_capacity(32);
        pages.push(Arc::new(Page::new()));
        MemPool { pages }
    }

    pub fn check_empty(&self) {
        for (index, page) in self.pages.iter().enumerate() {
            println!("PAGE {} FREE {}", index, page.nfree.load(Ordering::Relaxed));
        }
    }

    pub fn alloc(&mut self, value: T) -> PoolArc<T> {
        let (page, node) = self.find_place();

        let ptr = page.nodes[node.0].value.get();
        unsafe {
            std::ptr::write(ptr, value);
        }

        PoolArc::new(page, node)
    }

    pub unsafe fn alloc_with<Fun>(&mut self, fun: Fun) -> PoolArc<T>
    where
        Fun: Fn(&mut T)
    {
        let (page, node) = self.find_place();

        let v = page.nodes[node.0].value.get();
        fun(&mut *v);

        PoolArc::new(page, node)
    }

    pub fn alloc_maybeuninit<Fun>(&mut self, fun: Fun) -> PoolArc<T>
    where
        Fun: Fn(&mut MaybeUninit<T>)
    {
        let (page, node) = self.find_place();

        let v = page.nodes[node.0].value.get();
        fun(unsafe { &mut *(v as *mut std::mem::MaybeUninit<T>) });

        PoolArc::new(page, node)
    }
}

impl<T: Sized> Default for MemPool<T> {
    fn default() -> MemPool<T> {
        MemPool::new()
    }
}

impl<T> std::fmt::Debug for MemPool<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let len = self.pages.len();
        f.debug_struct("MemPool")
         .field("npages", &len)
         .finish()
    }
}

pub struct PoolArc<T: Sized> {
    page: Arc<Page<T>>,
    node: NonNull<Node<T>>,
}

unsafe impl<T: Sized> Send for MemPool<T> {}
unsafe impl<T: Sized> Send for PoolArc<T> {}
unsafe impl<T: Sync + Sized> Sync for PoolArc<T> {}

impl<T: std::fmt::Debug + Sized> std::fmt::Debug for PoolArc<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("PoolArc")
         .field("inner", &self)
         .finish()
    }
}

impl<T> PoolArc<T> {
    fn new(page: Arc<Page<T>>, index_in_page: IndexInPage) -> PoolArc<T> {
        page.nfree.fetch_sub(1, Ordering::AcqRel);

        let node = &page.nodes[index_in_page.0];

        if node.counter.load(Ordering::Relaxed) != 0 {
            panic!("PoolArc: Counter not zero");
        }

        node.counter.fetch_add(1, Ordering::Relaxed);
        let node = NonNull::from(node);

        PoolArc { node, page }
    }
}

impl<T: Sized> std::ops::Deref for PoolArc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.node.as_ref().value.get() }
    }
}

impl<T: Sized> std::ops::DerefMut for PoolArc<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.node.as_ref().value.get() }
    }
}

impl<T> Drop for PoolArc<T> {
    fn drop(&mut self) {
        let (page, node) = unsafe {
            (self.page.as_ref(), self.node.as_ref())
        };

        // We decrement the reference counter
        let count = node.counter.fetch_sub(1, Ordering::AcqRel);

        // We were the last reference
        if count == 1 {

            // We load the previous 'free_node' index of our page
            let mut page_free_node = page.free_node.load(Ordering::Relaxed);

            // We store a wrong index in our next_free
            // See below for explanation
            node.next_free.store(WRONG_NODE_INDEX, Ordering::Relaxed);

            // We want our page's 'free_node' to point to us
            // Since other threads can modify 'free_node' at the same time,
            // we loop on the exchange to ensure that:
            // 1 - our page's 'free_node' point to us
            // 2 - we get the previous value of the page 'free_node' to use
            //     it for our own 'next_free' (see below)
            while let Err(x) = page.free_node.compare_exchange_weak(
                page_free_node, node.index_in_page, Ordering::SeqCst, Ordering::Relaxed
            ) {
                println!("RETRY2", );
                page_free_node = x;
            }

            // At this point, another thread can already read the page's
            // 'free_node', which now point to us.
            // However our 'next_free' could point to another node that
            // might not be free.
            // We previously stored a wrong index in our 'next_free' to
            // avoid that another thread take that index as a free one.
            // So Page::acquire_free_node() will fail and return None, even
            // though there might be free nodes in our page.
            // But that's fine:
            // MemPool::find_place() will skip our page and continue searching
            // in other pages.
            // In the worst case, a new page will be allocated.
            // That's fine too, I would say

            // We put the previous 'free_node' of our page in our
            // 'next_free', now the linked list is valid
            node.next_free.store(page_free_node, Ordering::Release);

            // Increment the page's 'nfree' counter
            page.nfree.fetch_add(1, Ordering::Relaxed);
        };
    }
}
