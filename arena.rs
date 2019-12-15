
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::mem::MaybeUninit;

use super::page::{Page, IndexInPage};
use super::arena_arc::ArenaArc;

pub struct Arena<T: Sized> {
    last_found: usize,
    pages: Vec<Arc<Page<T>>>,
}

unsafe impl<T: Sized> Send for Arena<T> {}

impl<T: Sized> Arena<T> {
    fn alloc_new_page(&mut self) -> Arc<Page<T>> {
        let new_page = Arc::new(Page::new());
        self.pages.push(new_page.clone());
        new_page
    }

    fn find_place(&mut self) -> (Arc<Page<T>>, IndexInPage) {
        let (before, after) = self.pages.split_at(self.last_found);

        for (index, page) in after.iter().chain(before).enumerate() {
            if let Some(node) = page.acquire_free_node() {
                self.last_found = (self.last_found + index) % self.pages.len();
                return (page.clone(), node);
            };
        }

        let new_page = self.alloc_new_page();
        let node = new_page.acquire_free_node().unwrap();

        self.last_found = self.pages.len() - 1;

        (new_page, node)
    }

    pub fn new() -> Arena<T> {
        let mut pages = Vec::with_capacity(32);
        pages.push(Arc::new(Page::new()));
        Arena { pages, last_found: 0 }
    }

    pub fn alloc(&mut self, value: T) -> ArenaArc<T> {
        let (page, node) = self.find_place();

        let ptr = page.nodes[node.0].value.get();
        unsafe {
            std::ptr::write(ptr, value);
        }

        ArenaArc::new(page, node)
    }

    pub unsafe fn alloc_with<Fun>(&mut self, fun: Fun) -> ArenaArc<T>
    where
        Fun: Fn(&mut T)
    {
        let (page, node) = self.find_place();

        let v = page.nodes[node.0].value.get();
        fun(&mut *v);

        ArenaArc::new(page, node)
    }

    pub fn alloc_maybeuninit<Fun>(&mut self, fun: Fun) -> ArenaArc<T>
    where
        Fun: Fn(&mut MaybeUninit<T>)
    {
        let (page, node) = self.find_place();

        let v = page.nodes[node.0].value.get();
        fun(unsafe { &mut *(v as *mut std::mem::MaybeUninit<T>) });

        ArenaArc::new(page, node)
    }
}

impl<T: Sized> Default for Arena<T> {
    fn default() -> Arena<T> {
        Arena::new()
    }
}

impl<T> std::fmt::Debug for Arena<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let len = self.pages.len();
        f.debug_struct("MemPool")
         .field("npages", &len)
         .finish()
    }
}
