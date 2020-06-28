
use std::cell::{Cell, UnsafeCell};
use std::ptr::NonNull;
use std::alloc::{alloc, dealloc, Layout};
use std::marker::PhantomData;
use std::rc::{Rc, Weak};
use std::mem::MaybeUninit;

use super::page::{PageTaggedPtr, PageKind};

// This module is a transpose of other modules without atomics

type Pointer<T> = Cell<*mut T>;

#[repr(C)]
struct Block<T> {
    value: UnsafeCell<T>,
    counter: usize,
    page: PageTaggedPtr,
}

impl<T> Block<T> {
    pub(crate) fn drop_block(block: NonNull<Block<T>>) {
        let block_ref = unsafe { block.as_ref() };

        match block_ref.page.page_kind() {
            PageKind::Pool => {
                let page_ptr = block_ref.page.page_ptr::<Page<T>>();
                Page::<T>::drop_block(page_ptr, block);
            }
            x => panic!("Wrong PageTaggedPtr {:?}", x)
        }
    }
}

use super::page::{BLOCK_PER_PAGE, MASK_ARENA_BIT};

pub struct Page<T> {
    bitfield: usize,
    blocks: [Block<T>; BLOCK_PER_PAGE],
    arena_free_list: Weak<Pointer<Page<T>>>,
    next_free: Pointer<Page<T>>,
    next: Pointer<Page<T>>,
    in_free_list: bool,
}

pub struct PoolRc<T> {
    block: NonNull<Block<T>>,
    // page: NonNull<Page<T>>,
    _marker: PhantomData<*mut ()>
}

impl<T> PoolRc<T> {
    fn new(mut block: NonNull<Block<T>>) -> PoolRc<T> {
        let counter = &mut unsafe { block.as_mut() }.counter;
        assert!(*counter == 0, "PoolRc: Counter not zero {}", counter);
        *counter = 1;
        PoolRc { block, _marker: PhantomData }
    }
}

impl<T> std::ops::Deref for PoolRc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.block.as_ref().value.get() }
    }
}

/// Drop the PoolBox<T>
///
/// The value pointed by this PoolBox is also dropped
impl<T> Drop for PoolRc<T> {
    fn drop(&mut self) {
        let block = unsafe { self.block.as_mut() };

        // We decrement the reference counter
        block.counter -= 1;

        // We were the last reference
        if block.counter == 0 {
            Block::drop_block(self.block)
        };
    }
}

pub struct PoolBox<T> {
    block: NonNull<Block<T>>,
    _marker: PhantomData<*mut ()>
}

impl<T> PoolBox<T> {
    fn new(mut block: NonNull<Block<T>>) -> PoolBox<T> {
        let counter = &mut unsafe { block.as_mut() }.counter;
        // See ArenaBox<T>::new for why we touch the counter
        assert!(*counter == 0, "PoolBox: Counter not zero {}", counter);
        *counter = 1;
        PoolBox { block, _marker: PhantomData }
    }
}

impl<T> std::ops::Deref for PoolBox<T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.block.as_ref().value.get() }
    }
}

impl<T> std::ops::DerefMut for PoolBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.block.as_ref().value.get() }
    }
}

/// Drop the PoolBox<T>
///
/// The value pointed by this PoolBox is also dropped
impl<T> Drop for PoolBox<T> {
    fn drop(&mut self) {
        let block = unsafe { self.block.as_mut() };

        // See ArenaBox<T>::new for why we touch the counter
        assert!(block.counter == 1, "PoolBox: Counter != 1 on drop {}", block.counter);
        block.counter = 0;

        // We were the last reference
        if block.counter == 0 {
            Block::drop_block(self.block)
        };
    }
}

impl<T> Page<T> {
    fn allocate() -> NonNull<Page<T>> {
        let layout = Layout::new::<Page<T>>();
        unsafe {
            let page = alloc(layout) as *const Page<T>;
            NonNull::from(&*page)
        }
    }

    fn deallocate_page(page: *mut Page<T>) {
        let layout = Layout::new::<Page<T>>();
        unsafe {
            std::ptr::drop_in_place(&mut (*page).arena_free_list as *mut _);
            dealloc(page as *mut Page<T> as *mut u8, layout);
        }
    }

    fn new(
        arena_free_list: Weak<Pointer<Page<T>>>,
        next: *mut Page<T>
    ) -> NonNull<Page<T>>
    {
        let mut page_ptr = Self::allocate();
        let page_copy = page_ptr;

        let page = unsafe { page_ptr.as_mut() };

        // Initialize the page
        // Don't invoke any Drop here, the allocated page is uninitialized

        // We fill the bitfield with ones
        page.bitfield = !0;
        // page.next_free.set(next);
        // page.next.set(next);
        page.in_free_list = true;

        let free_ptr = &mut page.arena_free_list as *mut Weak<Pointer<Page<T>>>;
        unsafe {
            free_ptr.write(arena_free_list);
            // TODO: forget the old weak

            let next_free_ptr = &mut page.next_free as *mut Pointer<_>;
            let next_ptr = &mut page.next as *mut Pointer<_>;
            next_free_ptr.write(Cell::new(next));
            next_ptr.write(Cell::new(next));
        }

        // initialize the blocks
        for (index, block) in page.blocks.iter_mut().enumerate() {
            block.page = PageTaggedPtr::new(page_copy.as_ptr() as usize, index, PageKind::Pool);
            block.counter = 0;
        }

        page_ptr
    }

    /// Make a new list of Page
    ///
    /// Returns the first and last Page in the list
    pub fn make_list(
        npages: usize,
        arena_free_list: &Rc<Pointer<Page<T>>>
    ) -> (NonNull<Page<T>>, NonNull<Page<T>>)
    {
        let arena_free_list = Rc::downgrade(arena_free_list);

        let last = Page::<T>::new(arena_free_list.clone(), std::ptr::null_mut());
        let mut previous = last;

        for _ in 0..npages - 1 {
            let previous_ptr = unsafe { previous.as_mut() };
            let page = Page::<T>::new(arena_free_list.clone(), previous_ptr);
            previous = page;
        }

        (previous, last)
    }

    /// Search for a free [`Block`] in the [`Page`] and mark it as non-free
    ///
    /// If there is no free block, it returns None
    fn acquire_free_block(&mut self) -> Option<NonNull<Block<T>>> {
        let index_free = self.bitfield.trailing_zeros() as usize;

        if index_free == BLOCK_PER_PAGE {
            return None;
        }

        // We clear the bit of the free block to mark it as non free
        self.bitfield &= !(1 << index_free);

        Some(NonNull::from(&self.blocks[index_free]))
    }

    fn drop_block(mut page: NonNull<Page<T>>, block: NonNull<Block<T>>) {
        let page_ptr = page.as_ptr();
        let page = unsafe { page.as_mut() };
        let block = unsafe { block.as_ref() };

        unsafe {
            // Drop the inner value
            std::ptr::drop_in_place(block.value.get());
        }

        let index_in_page = block.page.index_block();
        page.bitfield |= 1 << index_in_page;

        // The bit dedicated to the Page is inversed (1 for used, 0 for free)
        if !page.bitfield == MASK_ARENA_BIT {
            // We were the last block/arena referencing this page
            // Deallocate it
            Page::<T>::deallocate_page(page_ptr);
            return;
        }

        if !page.in_free_list {
            page.in_free_list = true;

            if let Some(arena_free_list) = page.arena_free_list.upgrade() {
                let current = arena_free_list.get();
                page.next_free.set(current);
                arena_free_list.set(page_ptr);
            };
        }
    }
}

pub(super) fn drop_page<T>(page: *mut Page<T>) {
    // We clear the bit dedicated to the arena
    let new_bitfield = {
        let page = unsafe { page.as_mut().unwrap() };
        page.bitfield &= !MASK_ARENA_BIT;
        page.bitfield
    };

    if !new_bitfield == MASK_ARENA_BIT {
        // No one is referencing this page anymore (neither Arena, ArenaBox or ArenaArc)
        Page::<T>::deallocate_page(page);
    }
}

impl<T> Drop for Page<T> {
    fn drop(&mut self) {
        panic!("PAGE");
    }
}

/// The difference with SharedArena is that Pool
/// cannot be shared/sent to other threads, neither PoolBox or
/// PoolRc
pub struct Pool<T: Sized> {
    free: Rc<Pointer<Page<T>>>,
    page_list: Pointer<Page<T>>,
    npages: Cell<usize>,
    _marker: PhantomData<*mut ()>
}

impl<T: Sized> Pool<T> {
    pub fn new() -> Pool<T> {
        Self::with_capacity(1)
    }

    pub fn with_capacity(cap: usize) -> Pool<T> {
        let npages = ((cap.max(1) - 1) / BLOCK_PER_PAGE) + 1;
        let free = Rc::new(Cell::new(std::ptr::null_mut()));

        let (mut first, _) = Page::make_list(npages, &free);
        let first_ref = unsafe { first.as_mut() };

        free.set(first_ref);

        Pool {
            npages: Cell::new(npages),
            free,
            page_list: Cell::new(first_ref),
            _marker: PhantomData
        }
    }

    fn alloc_new_page(&self) -> NonNull<Page<T>> {
        let len = self.npages.get();

        let to_allocate = len.max(1).min(900_000);

        let (first, mut last) = Page::make_list(to_allocate, &self.free);

        let last_ref = unsafe { last.as_mut() };
        last_ref.next_free.set(self.free.get());
        last_ref.next.set(self.page_list.get());

        let first_ptr = first.as_ptr();
        self.free.set(first_ptr);
        self.page_list.set(first_ptr);

        self.npages.set(len + to_allocate);

        first
    }

    fn find_place(&self) -> NonNull<Block<T>> {
        loop {
            while let Some(page) = unsafe { self.free.get().as_mut() } {
                if let Some(block) = page.acquire_free_block() {
                    return block;
                }

                let next = page.next_free.get();

                self.free.set(next);
                page.in_free_list = false;
            }
            self.alloc_new_page();
        }
    }

    pub fn alloc(&self, value: T) -> PoolBox<T> {
        let block = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            ptr.write(value);
        }

        PoolBox::new(block)
    }

    pub fn alloc_in_place<F>(&self, initializer: F) -> PoolBox<T>
    where
        F: Fn(&mut MaybeUninit<T>)
    {
        let block = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            initializer(&mut *(ptr as *mut std::mem::MaybeUninit<T>));
        }

        PoolBox::new(block)
    }

    pub fn alloc_rc(&self, value: T) -> PoolRc<T> {
        let block = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            ptr.write(value);
        }

        PoolRc::new(block)
    }

    pub fn alloc_in_place_rc<F>(&self, initializer: F) -> PoolRc<T>
    where
        F: Fn(&mut MaybeUninit<T>)
    {
        let block = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            initializer(&mut *(ptr as *mut std::mem::MaybeUninit<T>));
        }

        PoolRc::new(block)
    }

    pub fn stats(&self) -> (usize, usize) {
        let mut next = self.page_list.get();
        let mut used = 0;
        let mut npages = 0;

        while let Some(next_ref) = unsafe { next.as_mut() } {
            let next_next = next_ref.next.get();

            let bitfield = next_ref.bitfield;
            let zeros = bitfield.count_zeros() as usize;
            used += zeros;
            next = next_next;

            npages += 1;
        }

        assert!(npages == self.npages.get());

        let free = (npages * BLOCK_PER_PAGE) - used;

        (used, free)
    }

    #[cfg(target_pointer_width = "64") ]
    #[cfg(test)]
    pub(crate) fn size_lists(&self) -> (usize, usize) {
        let mut next = self.page_list.get();
        let mut size = 0;
        while let Some(next_ref) = unsafe { next.as_mut() } {
            next = next_ref.next.get();
            size += 1;
        }

        let mut next = self.free.get();
        let mut free = 0;
        while let Some(next_ref) = unsafe { next.as_mut() } {
            next = next_ref.next_free.get();
            free += 1;
        }

        (size, free)
    }

    pub fn shrink_to_fit(&mut self) {

        let mut current: &Pointer<Page<T>> = &self.free;

        let mut to_drop = vec![];

        while let Some(current_value) = unsafe { current.get().as_mut() } {
            let next = &current_value.next_free;
            let next_value = next.get();

            if current_value.bitfield == !0 {
                current.set(next_value);
                to_drop.push(current_value as *const _ as *mut Page<T>);
            } else {
                current = next;
            }
        }

        let mut current: &Pointer<Page<T>> = &self.page_list;

        // Loop on the full list
        // We remove the pages from it
        while let Some(current_value) = unsafe { current.get().as_mut() } {
            let next = &current_value.next;
            let next_value = next.get();

            if to_drop.contains(&(current_value as *const _ as *mut Page<T>)) {
                current.set(next_value);
            } else {
                current = next;
            }
        }

        self.npages.set(self.npages.get() - to_drop.len());

        for page in to_drop.iter().rev() {
            drop_page(*page)
        }
    }

    #[allow(dead_code)]
    #[cfg(test)]
    pub(crate) fn display_list(&self) {
        let mut full = vec![];

        let mut next = self.page_list.get();
        while let Some(next_ref) = unsafe { next.as_mut() } {
            full.push(next);
            next = next_ref.next.get();
        }

        let mut list_free = vec![];

        let mut next = self.page_list.get();
        while let Some(next_ref) = unsafe { next.as_mut() } {
            list_free.push(next);
            next = next_ref.next_free.get();
        }

        println!("FULL {} {:#?}", full.len(), full);
        println!("FREE {} {:#?}", list_free.len(), list_free);
    }
}

impl<T> Default for Pool<T> {
    fn default() -> Self {
        Pool::new()
    }
}

impl<T> Drop for Pool<T> {
    fn drop(&mut self) {
        let mut next = self.page_list.get();

        while let Some(next_ref) = unsafe { next.as_mut() } {
            let next_next = next_ref.next.get();
            drop_page(next);
            next = next_next;
        }
    }
}

impl<T> std::fmt::Debug for Pool<T> {
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

        let npages = self.npages.get();

        let mut vec = Vec::with_capacity(npages);

        let mut next = self.page_list.get();

        while let Some(next_ref) = unsafe { next.as_mut() } {
            let used = next_ref.bitfield.count_zeros() as usize;
            vec.push(Page {
                used,
                free: BLOCK_PER_PAGE - used
            });

            next = next_ref.next.get();
        }

        let blocks_used: usize = vec.iter().map(|p| p.used).sum();
        let blocks_free: usize = vec.iter().map(|p| p.free).sum();

        f.debug_struct("Arena")
         .field("blocks_free", &blocks_free)
         .field("blocks_used", &blocks_used)
         .field("npages", &npages)
         .field("pages", &vec)
         .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::Pool;

    #[cfg(target_pointer_width = "64") ]
    #[test]
    fn arena_shrink() {
        let mut arena = Pool::<usize>::with_capacity(1000);
        assert_eq!(arena.stats(), (0, 1008));
        arena.shrink_to_fit();
        assert_eq!(arena.stats(), (0, 0));
    }

    #[cfg(target_pointer_width = "64") ]
    #[test]
    fn arena_shrink2() {
        let mut arena = Pool::<usize>::with_capacity(1000);

        println!("A");
        let _a = arena.alloc(1);
        arena.shrink_to_fit();
        assert_eq!(arena.stats(), (1, 62));

        println!("A1");
        let _a = arena.alloc(1);
        arena.shrink_to_fit();
        assert_eq!(arena.stats(), (2, 61));

        println!("A2");
        let mut values = Vec::with_capacity(64);
        for _ in 0..64 {
            values.push(arena.alloc(1));
        }

        println!("A3");
        assert_eq!(arena.stats(), (66, 60));
        println!("A32");
        arena.shrink_to_fit();
        println!("A33");
        assert_eq!(arena.stats(), (66, 60));

        println!("A4");
        std::mem::drop(values);

        println!("A5");
        assert_eq!(arena.stats(), (2, 124));
        println!("A6");
        arena.shrink_to_fit();
        println!("A7");
        assert_eq!(arena.stats(), (2, 61));
    }

    #[cfg(target_pointer_width = "64") ]
    #[test]
    fn arena_size() {
        let mut arena = Pool::<usize>::with_capacity(1000);

        assert_eq!(arena.size_lists(), (16, 16));
        let a = arena.alloc(1);
        assert_eq!(arena.size_lists(), (16, 16));

        let mut values = Vec::with_capacity(539);
        for _ in 0..539 {
            values.push(arena.alloc(1));
        }
        assert_eq!(arena.size_lists(), (16, 8));

        arena.shrink_to_fit();

        assert_eq!(arena.size_lists(), (9, 1));

        values.truncate(503);
        arena.shrink_to_fit();

        assert_eq!(arena.size_lists(), (8, 0));

        std::mem::drop(a);
        for _ in 0..62 {
            values.remove(0);
        }

        assert_eq!(arena.size_lists(), (8, 1));

        arena.shrink_to_fit();

        assert_eq!(arena.size_lists(), (7, 0));

        values.clear();

        assert_eq!(arena.size_lists(), (7, 7));

        arena.shrink_to_fit();

        assert_eq!(arena.size_lists(), (0, 0));

        {
            let _a = arena.alloc(1);
            println!("LA3", );
            assert_eq!(arena.size_lists(), (1, 1));

            println!("{:?}", arena);
            arena.display_list();
        }

        assert_eq!(arena.size_lists(), (1, 1));
        arena.shrink_to_fit();
        assert_eq!(arena.size_lists(), (0, 0));

        let mut values = Vec::with_capacity(126);
        for _ in 0..126 {
            values.push(arena.alloc(1));
        }
        assert_eq!(arena.size_lists(), (2, 1));

        values.remove(0);
        assert_eq!(arena.size_lists(), (2, 2));

        values.push(arena.alloc(1));
        assert_eq!(arena.size_lists(), (2, 2));
    }

    #[test]
    fn alloc_fns() {
        let arena = Pool::<usize>::new();

        use std::ptr;

        let a = arena.alloc_in_place(|place| unsafe {
            ptr::copy(&101, place.as_mut_ptr(), 1);
        });
        assert!(*a == 101);

        let a = arena.alloc_in_place_rc(|place| unsafe {
            ptr::copy(&102, place.as_mut_ptr(), 1);
        });
        assert!(*a == 102);

        let a = arena.alloc(103);
        assert!(*a == 103);

        let a = arena.alloc_rc(104);
        assert!(*a == 104);
    }

    #[test]
    fn drop_arena_with_valid_allocated() {
        let (a, b, c, d) = {
            let arena = Pool::<usize>::new();

            use std::ptr;

            let a = arena.alloc_in_place(|place| unsafe {
                ptr::copy(&101, place.as_mut_ptr(), 1);
            });
            let b = arena.alloc_in_place_rc(|place| unsafe {
                ptr::copy(&102, place.as_mut_ptr(), 1);
            });
            let c = arena.alloc(103);
            let d = arena.alloc_rc(104);

            (a, b, c, d)
        };

        assert_eq!((*a, *b, *c, *d), (101, 102, 103, 104))
    }

    #[test]
    #[should_panic]
    #[cfg(target_pointer_width = "64") ]
    fn invalid_block() {
        use std::cell::UnsafeCell;
        use std::ptr::NonNull;

        let mut block = super::Block {
            value: UnsafeCell::new(1),
            counter: 1,
            page: super::PageTaggedPtr {
                data: !0,
                #[cfg(test)]
                real_ptr: !0
            },
        };

        super::Block::drop_block(NonNull::from(&mut block));
    }
}
