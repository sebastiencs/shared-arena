
use std::sync::atomic::{AtomicUsize, Ordering, AtomicPtr};
use std::ptr::NonNull;
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;

const NODE_PER_PAGE: usize = 32;

struct Node<T: Sized> {
    /// Read only
    index_page: usize,
    counter: AtomicUsize,
    next_free: AtomicUsize,
    value: UnsafeCell<T>,
}

struct Page<T: Sized> {
    nodes: Vec<Node<T>>,
    free: AtomicUsize,
    next_free: AtomicUsize,
}

impl<T: Sized> Page<T> {
    fn new() -> Page<T> {
        let mut nodes = Vec::<Node<T>>::with_capacity(NODE_PER_PAGE);

        unsafe { nodes.set_len(NODE_PER_PAGE) };

        // TODO: Should use MaybeUninit here

        for (index, node) in nodes.iter_mut().enumerate() {
            node.index_page = index;
            node.counter = AtomicUsize::new(0);
            node.next_free = AtomicUsize::new(index + 1);
        }

        let len = nodes.len();

        Page {
            nodes,
            free: AtomicUsize::new(len),
            next_free: AtomicUsize::new(0),
        }
    }

    fn next(&self, index: usize) -> usize {
        match self.nodes.get(index) {
            Some(node) => {
                node.next_free.load(Ordering::Relaxed)
            }
            _ => self.nodes.len() + 1
        }
    }

    fn next_free(&self) -> Option<&Node<T>> {
        let freed = self.free.load(Ordering::Acquire);

        if freed == 0 {
            return None;
        }

        let mut next = self.next_free.load(Ordering::Relaxed);
        let mut next_next = self.next(next);

        while let Err(x) = self.next_free.compare_exchange_weak(
            next, next_next, Ordering::SeqCst, Ordering::Relaxed
        ) {
            next = x;
            next_next = self.next(next);
        }

        self.nodes.get(next)
    }
}

struct MemPool<T: Sized> {
    pages: Vec<Page<T>>,
}

impl<T: Sized> MemPool<T> {
    fn alloc_new_page(&mut self) -> &Page<T> {
        self.pages.push(Page::new());
        self.pages.last().unwrap()
    }

    fn alloc_place(&mut self) -> (&Page<T>, &Node<T>) {
        let new_page = self.alloc_new_page();
        let node = new_page.next_free().unwrap();

        (new_page, node)
    }

    fn find_place(&mut self) -> Option<(&Page<T>, &Node<T>)> {
        for page in self.pages.iter() {
            if let Some(node) = page.next_free() {
                return Some((page, node));
            };
        }
        None
    }

    fn alloc(&mut self, value: T) -> Guard<T> {
        let (page, node) = match self.find_place() {
            Some(x) => x,
            _ => self.alloc_place()
        };

        let v = node.value.get();
        unsafe { std::ptr::write(v, value); }

        Guard::new(page, node)
    }

    fn alloc_with<Fun>(&mut self, fun: Fun) -> Guard<T>
    where
        Fun: Fn(&mut MaybeUninit<T>)
    {
        let (page, node) = match self.find_place() {
            Some(x) => x,
            _ => self.alloc_place()
        };

        let v = node.value.get();
        fun(unsafe { std::mem::transmute(v) });

        Guard::new(page, node)
    }
}

struct Guard<T: Sized> {
    page: NonNull<Page<T>>,
    node: NonNull<Node<T>>,
}

impl<T> Guard<T> {
    fn new(page: &Page<T>, node: &Node<T>) -> Guard<T> {
        page.free.fetch_add(1, Ordering::AcqRel);
        node.counter.fetch_add(1, Ordering::AcqRel);
        Guard {
            page: NonNull::from(page),
            node: NonNull::from(node)
        }
    }
}

impl<T> Drop for Guard<T> {
    fn drop(&mut self) {
        let (page, node) = unsafe {
            (self.page.as_ref(), self.node.as_ref())
        };

        // We decrement the reference counter
        let count = node.counter.fetch_sub(1, Ordering::AcqRel);

        // We were the last reference
        if count == 1 {

            // We load the previous 'next_free' ptr/index of our page
            let mut next = page.next_free.load(Ordering::Relaxed);

            // We want our page's 'next_free' to point to us
            // Since other threads can modify 'next_free' at the same time,
            // we loop on the exchange to ensure that:
            // 1 - our page's 'next_free' point to us
            // 2 - we get the previous value of the page 'next_free' to use
            //     it for our own 'next_free' (see bellow)
            while let Err(x) = page.next_free.compare_exchange_weak(
                next, node.index_page, Ordering::SeqCst, Ordering::Relaxed
            ) {
                next = x;
            }

            // We put the previous 'next_free' of our page in our own
            // 'next_free'
            node.next_free.store(next, Ordering::Release);

            // Increment the page's 'free' counter
            page.free.fetch_add(1, Ordering::Relaxed);
        };
    }
}
