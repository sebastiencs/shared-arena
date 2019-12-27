
use std::cell::{Cell, UnsafeCell};
use std::ptr::NonNull;
use std::alloc::{alloc, dealloc, Layout};
use std::marker::PhantomData;
use std::rc::{Rc, Weak};

// use super::circular_iter::CircularIterator;

type Pointer<T> = Cell<*mut T>;

struct Block<T> {
    value: UnsafeCell<T>,
    counter: usize,
    index_in_page: usize,
}

use super::page::{BLOCK_PER_PAGE, MASK_ARENA_BIT};

struct Page<T> {
    bitfield: usize,
    blocks: [Block<T>; BLOCK_PER_PAGE],
    pub arena_free_list: Weak<Pointer<Page<T>>>,
    pub next_free: Pointer<Page<T>>,
    pub next: Pointer<Page<T>>,
    pub in_free_list: bool,
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
            drop_block_in_pool(page, block);
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
        // See PoolBox<T>::new for why we touch the counter
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

fn drop_block_in_pool<T>(page: &mut Page<T>, block: &Block<T>) {
    unsafe {
        // Drop the inner value
        std::ptr::drop_in_place(block.value.get());
    }
    let index_in_page = block.index_in_page;
    page.bitfield |= 1 << index_in_page;
    // The bit dedicated to the Page is inversed (1 for used, 0 for free)
    if !page.bitfield == 1 << 63 {
        // We were the last block/arena referencing this page
        // Deallocate it
        page.deallocate();
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
        // See PoolBox<T>::new for why we touch the counter
        assert!(block.counter == 1, "PoolBox: Counter != 1 on drop {}", block.counter);
        block.counter = 0;
        drop_block_in_pool(page, block);
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

        let mut page = unsafe { page_ptr.as_mut() };

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
    #[inline]
    pub fn acquire_free_block(&mut self) -> Option<NonNull<Block<T>>> {
        let index_free = self.bitfield.trailing_zeros() as usize;

        if index_free == BLOCK_PER_PAGE {
            return None;
        }

        // We clear the bit of the free block to mark it as non free
        self.bitfield &= !(1 << index_free);

        Some(NonNull::from(&self.blocks[index_free]))
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

/// The difference with SharedArena is that the pool
/// is not sendable to other threads, neither its PoolBox and
/// PoolRc
pub struct Pool<T: Sized> {
    free: Rc<Pointer<Page<T>>>,
    page_list: Pointer<Page<T>>,
    npages: usize,
    // pages: Vec<NonNull<Page<T>>>,
    _marker: PhantomData<*mut ()>
}

impl<T: Sized> Pool<T> {
    pub fn new() -> Pool<T> {
        Self::with_capacity(1)
    }

    pub fn with_capacity(cap: usize) -> Pool<T> {
        let npages = ((cap.max(1) - 1) / BLOCK_PER_PAGE) + 1;
        let mut free = Rc::new(Cell::new(std::ptr::null_mut()));

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

    // pub fn with_capacity(cap: usize) -> Pool<T> {
    //     let npages = ((cap.max(1) - 1) / BLOCK_PER_PAGE) + 1;

    //     let mut pages = Vec::with_capacity(npages);
    //     pages.resize_with(npages, Page::<T>::new);

    //     Pool { last_found: 0, pages, _marker: PhantomData }
    // }

    fn alloc_new_page(&mut self) -> NonNull<Page<T>> {
        let len = self.npages;

        let to_allocate = len.min(900_000);

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

            println!("POOL ALLOCATE MORE", );
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
}

impl<T> Drop for Pool<T> {
    fn drop(&mut self) {
        let mut next = self.page_list.get();

        while let Some(next_ref) = unsafe { next.as_mut() } {
            unsafe {
                std::ptr::drop_in_place(next);
            }
            next = next_ref.next.get();
        }
    }
}
