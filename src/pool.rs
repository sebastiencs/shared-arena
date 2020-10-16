use std::cell::Cell;
use std::ptr::NonNull;
use std::marker::PhantomData;
use std::rc::Rc;
use std::mem::MaybeUninit;

use crate::block::Block;
use crate::common::{BLOCK_PER_PAGE, Pointer};
use crate::page::pool::{PagePool, drop_page};
use crate::ArenaRc;

/// A pointer to `T` in `Pool`
///
/// `PoolBox` implements [`DerefMut`] so it is directly mutable
/// (without mutex or other synchronization methods).
///
/// It is not clonable and cannot be sent to others threads.
///
/// ```
/// # use shared_arena::{PoolBox, Pool};
/// let pool = Pool::new();
/// let mut my_opt: PoolBox<Option<i32>> = pool.alloc(Some(10));
///
/// assert!(my_opt.is_some());
/// assert_eq!(my_opt.take(), Some(10));
/// assert!(my_opt.is_none());
/// ```
///
/// [`ArenaArc`]: ./struct.ArenaArc.html
/// [`Arena`]: ./struct.Arena.html
/// [`SharedArena`]: ./struct.SharedArena.html
/// [`DerefMut`]: https://doc.rust-lang.org/std/ops/trait.DerefMut.html
///
pub struct PoolBox<T> {
    block: NonNull<Block<T>>,
    _marker: PhantomData<*mut ()>
}

impl<T> PoolBox<T> {
    fn new(mut block: NonNull<Block<T>>) -> PoolBox<T> {
        // PoolBox is not Send, so we can make the counter non-atomic
        let counter_mut = unsafe { block.as_mut() }.counter.get_mut();

        // let counter = &mut unsafe { block.as_mut() }.counter;
        // See ArenaBox<T>::new for why we touch the counter
        assert!(*counter_mut == 0, "PoolBox: Counter not zero {}", counter_mut);
        *counter_mut = 1;
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
        // PoolBox is not Send, so we can make the counter non-atomic
        let counter_mut = unsafe { self.block.as_mut() }.counter.get_mut();
        // let block = unsafe { self.block.as_mut() };

        // See ArenaBox<T>::new for why we touch the counter
        assert!(*counter_mut == 1, "PoolBox: Counter != 1 on drop {}", counter_mut);
        *counter_mut = 0;

        Block::drop_block(self.block)
    }
}

/// A single threaded arena
///
/// It produces only `PoolBox` and `ArenaRc` which cannot be sent
/// to other threads.
///
/// [`ArenaRc`]: ./struct.ArenaRc.html
/// [`PoolBox`]: ./struct.PoolBox.html
///
pub struct Pool<T: Sized> {
    free: Rc<Pointer<PagePool<T>>>,
    page_list: Pointer<PagePool<T>>,
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

        let (mut first, _) = PagePool::make_list(npages, &free);
        let first_ref = unsafe { first.as_mut() };

        free.set(first_ref);

        Pool {
            npages: Cell::new(npages),
            free,
            page_list: Cell::new(first_ref),
            _marker: PhantomData
        }
    }

    fn alloc_new_page(&self) -> NonNull<PagePool<T>> {
        let len = self.npages.get();

        let to_allocate = len.max(1).min(900_000);

        let (first, mut last) = PagePool::make_list(to_allocate, &self.free);

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

    pub fn alloc_with<F>(&self, initializer: F) -> PoolBox<T>
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

    pub fn alloc_rc(&self, value: T) -> ArenaRc<T> {
        let block = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            ptr.write(value);
        }

        ArenaRc::new(block)
    }

    pub fn alloc_rc_with<F>(&self, initializer: F) -> ArenaRc<T>
    where
        F: Fn(&mut MaybeUninit<T>)
    {
        let block = self.find_place();

        unsafe {
            let ptr = block.as_ref().value.get();
            initializer(&mut *(ptr as *mut std::mem::MaybeUninit<T>));
        }

        ArenaRc::new(block)
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

    /// Shrinks the capacity of the arena as much as possible.
    ///
    /// It will drop all pages that are unused (no ArenaRc or PoolBox
    /// points to it).  
    /// If there is still one or more references to a page, the page
    /// won't be dropped.
    ///
    /// The dedicated memory will be deallocated during this call.
    ///
    /// ## Example
    ///
    /// ```
    /// # use shared_arena::Pool;
    /// let mut arena = Pool::with_capacity(2048);
    /// let mut values = Vec::new();
    ///
    /// assert_eq!(arena.stats(), (0, 2079));
    ///
    /// for _ in 0..80 {
    ///     values.push(arena.alloc(0xFF));
    /// }
    ///
    /// arena.shrink_to_fit();
    ///
    /// let (used, free) = arena.stats();
    /// assert!(used == 80, free == 46);
    ///
    /// ```
    pub fn shrink_to_fit(&mut self) {

        let mut current: &Pointer<PagePool<T>> = &self.free;

        let mut to_drop = vec![];

        while let Some(current_value) = unsafe { current.get().as_mut() } {
            let next = &current_value.next_free;
            let next_value = next.get();

            if current_value.bitfield == !0 {
                current.set(next_value);
                to_drop.push(current_value as *const _ as *mut PagePool<T>);
            } else {
                current = next;
            }
        }

        let mut current: &Pointer<PagePool<T>> = &self.page_list;

        // Loop on the full list
        // We remove the pages from it
        while let Some(current_value) = unsafe { current.get().as_mut() } {
            let next = &current_value.next;
            let next_value = next.get();

            if to_drop.contains(&(current_value as *const _ as *mut PagePool<T>)) {
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

/// Code that should fail to compile.
/// compile_fail is supported on doc only
///
/// Fails because Pool doesn't implement Sync, which Arc requires
/// ```compile_fail
/// use shared_arena::Pool;
/// use std::sync::Arc;
///
/// let arena: Arc<Pool<i32>> = Arc::new(Pool::new());
///
/// std::thread::spawn(move || {
///     std::mem::drop(arena)
/// });
/// ```
///
/// ```compile_fail
/// use shared_arena::Pool;
/// use std::sync::Arc;
///
/// let arena: Arc<Pool<i32>> = Arc::new(Pool::new());
///
/// std::thread::spawn(move || {
///     arena.alloc(1);
/// });
/// arena.alloc(2);
/// ```
#[allow(dead_code)]
fn arena_fail() {} // grcov_ignore

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

        let a = arena.alloc_with(|place| unsafe {
            ptr::copy(&101, place.as_mut_ptr(), 1);
        });
        assert!(*a == 101);

        let a = arena.alloc_rc_with(|place| unsafe {
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

            let a = arena.alloc_with(|place| unsafe {
                ptr::copy(&101, place.as_mut_ptr(), 1);
            });
            let b = arena.alloc_rc_with(|place| unsafe {
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
        use std::sync::atomic::AtomicUsize;

        let mut block = super::Block {
            value: UnsafeCell::new(1),
            counter: AtomicUsize::new(1),
            page: crate::block::PageTaggedPtr {
                data: !0,
            },
        };

        super::Block::drop_block(NonNull::from(&mut block));
    } // grcov_ignore
}
