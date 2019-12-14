
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
        page.nfree.fetch_sub(1, Ordering::AcqRel);

        let node = &page.nodes[index_in_page.0];

        if node.counter.load(Ordering::Relaxed) != 0 {
            panic!("PoolArc: Counter not zero");
        }

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
