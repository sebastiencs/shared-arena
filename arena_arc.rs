
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::ptr::NonNull;

use super::page::{IndexInPage, Page, Node, WRONG_NODE_INDEX};

pub struct ArenaArc<T: Sized> {
    page: Arc<Page<T>>,
    node: NonNull<Node<T>>,
}

unsafe impl<T: Sized> Send for ArenaArc<T> {}
unsafe impl<T: Sync + Sized> Sync for ArenaArc<T> {}

impl<T: std::fmt::Debug + Sized> std::fmt::Debug for ArenaArc<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("PoolArc")
         .field("inner", &self)
         .finish()
    }
}

impl<T> ArenaArc<T> {
    pub fn new(page: Arc<Page<T>>, index_in_page: IndexInPage) -> ArenaArc<T> {
        let node = &page.nodes[index_in_page.0];

        let counter = node.counter.load(Ordering::Relaxed);
        assert!(counter == 0, "PoolArc: Counter not zero");

        node.counter.fetch_add(1, Ordering::Relaxed);
        let node = NonNull::from(node);

        ArenaArc { node, page }
    }
}

impl<T: Sized> std::ops::Deref for ArenaArc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.node.as_ref().value.get() }
    }
}

impl<T: Sized> std::ops::DerefMut for ArenaArc<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.node.as_ref().value.get() }
    }
}

impl<T> Drop for ArenaArc<T> {
    fn drop(&mut self) {
        let (page, node) = unsafe {
            (self.page.as_ref(), self.node.as_ref())
        };

        // We decrement the reference counter
        let count = node.counter.fetch_sub(1, Ordering::AcqRel);

        // We were the last reference
        if count == 1 {
            let index_in_page = node.index_in_page;

            let mut bitfield = page.bitfield.load(Ordering::Relaxed);

            // We set our bit to mark the node as free
            let mut new_bitfield = bitfield | (1 << index_in_page);

            while let Err(x) = page.bitfield.compare_exchange_weak(
                bitfield, new_bitfield, Ordering::SeqCst, Ordering::Relaxed
            ) {
                bitfield = x;
                new_bitfield = bitfield | (1 << index_in_page);
            }
        };
    }
}
