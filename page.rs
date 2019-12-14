
use std::sync::atomic::{AtomicUsize, Ordering};
use std::ptr::NonNull;
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::Arc;

pub const NODE_PER_PAGE: usize = 32;
pub const WRONG_NODE_INDEX: usize = NODE_PER_PAGE + 1;

pub struct Node<T: Sized> {
    /// Read only and initialized on Node creation
    /// Doesn't need to be atomic
    pub index_in_page: usize,
    /// Number of references to this node
    pub counter: AtomicUsize,
    /// Index to the next free node in our page
    pub next_free: AtomicUsize,
    pub value: UnsafeCell<T>,
}

pub struct IndexInPage(pub usize);

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

pub struct Page<T: Sized> {
    /// Number of free nodes in this page
    pub nfree: AtomicUsize,
    /// Index to a free node in this page
    pub free_node: AtomicUsize,
    /// Array of nodes
    pub nodes: [Node<T>; NODE_PER_PAGE],
}

impl<T: Sized> Page<T> {
    pub fn new() -> Page<T> {

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

    pub fn acquire_free_node(&self) -> Option<IndexInPage> {
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
