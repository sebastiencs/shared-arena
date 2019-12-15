
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::ptr::NonNull;
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::Arc;

use crate::cache_line::CacheAligned;

pub const NODE_PER_PAGE: usize = 32;
pub const WRONG_NODE_INDEX: usize = NODE_PER_PAGE + 1;

pub struct Node<T: Sized> {
    /// Read only and initialized on Node creation
    /// Doesn't need to be atomic
    pub index_in_page: usize,
    /// Number of references to this node
    pub counter: AtomicUsize,
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
    // 1 => Free
    // 0 => Occupied
    pub bitfield: CacheAligned<AtomicU32>,

    /// Array of nodes
    pub nodes: CacheAligned<[Node<T>; NODE_PER_PAGE]>,
}

unsafe impl<T> Sync for Page<T> {}

impl<T: Sized> Page<T> {
    pub fn new() -> Page<T> {

        // MaybeUninit doesn't allow field initialization :(
        // https://doc.rust-lang.org/std/mem/union.MaybeUninit.html#initializing-a-struct-field-by-field
        #[allow(clippy::uninit_assumed_init)]
        let mut nodes: [Node<T>; NODE_PER_PAGE] = unsafe { MaybeUninit::uninit().assume_init() };

        for (index, node) in nodes.iter_mut().enumerate() {
            node.index_in_page = index;
            node.counter = AtomicUsize::new(0);
        }

        Page {
            nodes: CacheAligned::new(nodes),
            bitfield: CacheAligned::new(AtomicU32::new(!0))
        }
    }

    pub fn acquire_free_node(&self) -> Option<IndexInPage> {
        let mut bitfield = self.bitfield.load(Ordering::Relaxed);

        let mut index_free = bitfield.trailing_zeros();

        if index_free == 32 {
            return None;
        }

        // Bitfield where we clear the bit of the free node to mark
        // it as non-free
        let mut new_bitfield = bitfield & !(1 << index_free);

        while let Err(x) = self.bitfield.compare_exchange_weak(
            bitfield, new_bitfield, Ordering::SeqCst, Ordering::Relaxed
        ) {
            bitfield = x;
            index_free = bitfield.trailing_zeros();

            if index_free == 32 {
                return None;
            }

            new_bitfield = bitfield & !(1 << index_free);
        }

        Some((index_free as usize).into())
    }
}
