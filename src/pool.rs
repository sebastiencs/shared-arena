
use std::cell::{Cell, UnsafeCell};
use std::ptr::NonNull;
use std::alloc::{alloc, dealloc, Layout};
use std::marker::PhantomData;
use std::rc::{Rc, Weak};

// This module is a transpose of other modules without atomics

type Pointer<T> = Cell<*mut T>;

#[repr(C)]
struct Block<T> {
    value: UnsafeCell<T>,
    counter: usize,
    index_in_page: usize,
}

use super::page::{BLOCK_PER_PAGE, MASK_ARENA_BIT};

struct Page<T> {
    bitfield: usize,
    blocks: [Block<T>; BLOCK_PER_PAGE],
    arena_free_list: Weak<Pointer<Page<T>>>,
    next_free: Pointer<Page<T>>,
    next: Pointer<Page<T>>,
    in_free_list: bool,
}

pub struct PoolRc<T> {
    block: NonNull<Block<T>>,
    page: NonNull<Page<T>>,
    _marker: PhantomData<*mut ()>
}

impl<T> PoolRc<T> {
    fn new(page: NonNull<Page<T>>, mut block: NonNull<Block<T>>) -> PoolRc<T> {
        let counter = &mut unsafe { block.as_mut() }.counter;
        assert!(*counter == 0, "PoolRc: Counter not zero {}", counter);
        *counter = 1;
        PoolRc { block, page, _marker: PhantomData }
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
        let (page, block) = unsafe {
            (self.page.as_mut(), self.block.as_mut())
        };
        block.counter -= 1;
        if block.counter == 0 {
            page.drop_block(block);
        }
    }
}

pub struct PoolBox<T> {
    block: NonNull<Block<T>>,
    page: NonNull<Page<T>>,
    _marker: PhantomData<*mut ()>
}

impl<T> PoolBox<T> {
    fn new(page: NonNull<Page<T>>, mut block: NonNull<Block<T>>) -> PoolBox<T> {
        let counter = &mut unsafe { block.as_mut() }.counter;
        // See ArenaBox<T>::new for why we touch the counter
        assert!(*counter == 0, "PoolBox: Counter not zero {}", counter);
        *counter = 1;
        PoolBox { block, page, _marker: PhantomData }
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
        let (page, block) = unsafe {
            (self.page.as_mut(), self.block.as_mut())
        };
        // See ArenaBox<T>::new for why we touch the counter
        assert!(block.counter == 1, "PoolBox: Counter != 1 on drop {}", block.counter);
        block.counter = 0;
        page.drop_block(block);
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

    pub fn deallocate(&mut self) {
        let layout = Layout::new::<Page<T>>();
        unsafe {
            dealloc(self as *mut Page<T> as *mut u8, layout);
        }
    }

    fn new(
        arena_free_list: Weak<Pointer<Page<T>>>,
        next: *mut Page<T>
    ) -> NonNull<Page<T>>
    {
        let mut page_ptr = Self::allocate();

        let page = unsafe { page_ptr.as_mut() };

        // Initialize the page
        // Don't invoke any Drop here, the allocated page is uninitialized

        // We fill the bitfield with ones
        page.bitfield = !0;
        page.next_free.set(next);
        page.next.set(next);
        page.in_free_list = true;

        let free_ptr = &mut page.arena_free_list as *mut Weak<Pointer<Page<T>>>;
        unsafe {
            free_ptr.write(arena_free_list);
            // TODO: forget the old weak
        }

        // initialize the blocks
        for (index, block) in page.blocks.iter_mut().enumerate() {
            block.index_in_page = index;
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
    pub fn acquire_free_block(&mut self) -> Option<NonNull<Block<T>>> {
        let index_free = self.bitfield.trailing_zeros() as usize;

        if index_free == BLOCK_PER_PAGE {
            return None;
        }

        // We clear the bit of the free block to mark it as non free
        self.bitfield &= !(1 << index_free);

        Some(NonNull::from(&self.blocks[index_free]))
    }

    fn drop_block(&mut self, block: &Block<T>) {
        unsafe {
            // Drop the inner value
            std::ptr::drop_in_place(block.value.get());
        }
        let index_in_page = block.index_in_page;
        self.bitfield |= 1 << index_in_page;

        // The bit dedicated to the Page is inversed (1 for used, 0 for free)
        if !self.bitfield == MASK_ARENA_BIT {
            // We were the last block/arena referencing this page
            // Deallocate it
            self.deallocate();
            return;
        }

        if !self.in_free_list {
            self.in_free_list = true;

            let arena_free_list = match self.arena_free_list.upgrade() {
                Some(ptr) => ptr,
                _ => return // The arena has been dropped
            };

            let current = arena_free_list.get();
            self.next_free.set(current);
            arena_free_list.set(self);
        }
    }
}

impl<T> Drop for Page<T> {
    fn drop(&mut self) {
        self.bitfield &= !MASK_ARENA_BIT;

        // The bit dedicated to the arena is inversed (1 for used, 0 for free)
        if !self.bitfield == MASK_ARENA_BIT {
            // No one is referencing this page anymore (neither Arena, ArenaBox or ArenaArc)
            self.deallocate();
        }
    }
}

/// The difference with SharedArena is that Pool
/// cannot be shared/sent to other threads, neither PoolBox or
/// PoolRc
pub struct Pool<T: Sized> {
    free: Rc<Pointer<Page<T>>>,
    page_list: Pointer<Page<T>>,
    npages: usize,
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
            npages,
            free,
            page_list: Cell::new(first_ref),
            _marker: PhantomData
        }
    }

    fn alloc_new_page(&mut self) -> NonNull<Page<T>> {
        let len = self.npages;

        let to_allocate = len.max(1).min(900_000);

        let (mut first, mut last) = Page::make_list(to_allocate, &self.free);

        let (first_ref, last_ref) = unsafe {
            (first.as_mut(), last.as_mut())
        };

        last_ref.next_free.set(self.free.get());
        last_ref.next.set(self.page_list.get());

        self.free.set(first_ref);
        self.page_list.set(first_ref);

        self.npages += to_allocate;

        first
    }

    fn find_place(&mut self) -> (NonNull<Page<T>>, NonNull<Block<T>>) {
        loop {
            while let Some(page) = unsafe { self.free.get().as_mut() } {
                if let Some(block) = page.acquire_free_block() {
                    return (NonNull::from(page), block);
                }

                let next = page.next_free.get();

                self.free.set(next);
                page.in_free_list = false;
            }
            self.alloc_new_page();
        }
    }

    pub fn alloc(&mut self, value: T) -> PoolBox<T> {
        let (page, block) = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            ptr.write(value);
        }

        PoolBox::new(page, block)
    }

    pub fn alloc_rc(&mut self, value: T) -> PoolRc<T> {
        let (page, block) = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            ptr.write(value);
        }

        PoolRc::new(page, block)
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

        self.npages -= to_drop.len();

        for page in to_drop.iter().rev() {
            // Invoke Page::drop
            unsafe { std::ptr::drop_in_place(*page) }
        }
    }
}

impl<T> Drop for Pool<T> {
    fn drop(&mut self) {
        let mut next = self.page_list.get();

        while let Some(next_ref) = unsafe { next.as_mut() } {
            let next_next = next_ref.next.get();
            unsafe {
                std::ptr::drop_in_place(next);
            }
            next = next_next;
        }
    }
}
