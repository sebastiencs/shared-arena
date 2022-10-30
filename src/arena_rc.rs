use std::ptr::NonNull;

use crate::block::Block;

/// A single threaded reference-counting pointer to `T` in the arena.  
///
/// It cannot be sent between threads.
///
/// When the last `ArenaRc` pointer to a given value is dropped,
/// the pointed-to value is also dropped and its dedicated memory
/// in the arena is marked as available for future allocation.
///
/// Shared mutable references in Rust is not allowed, if you need to
/// mutate through an `ArenaRc`, use a Mutex, RwLock or one of
/// the atomic types.
///
/// If you don't need to share the value, you should use [`ArenaBox`].
///
/// ## Cloning references
///
/// Creating a new reference from an existing reference counted pointer
/// is done using the `Clone` trait implemented for `ArenaRc<T>`
///
/// ## `Deref` behavior
///
/// `ArenaRc<T>` automatically dereferences to `T`, so you can call
/// `T`'s methods on a value of type `ArenaRc<T>`.
///
/// ```
/// # use shared_arena::{ArenaRc, Arena};
/// let arena = Arena::new();
/// let my_num: ArenaRc<i32> = arena.alloc_rc(100i32);
///
/// assert!(my_num.is_positive());
///
/// let value = 1 + *my_num;
/// assert_eq!(value, 101);
///
/// assert_eq!(*my_num.clone(), 100);
/// ```
///
/// [`ArenaRc`]: ./struct.ArenaRc.html
/// [`Arena`]: ./struct.Arena.html
/// [`DerefMut`]: https://doc.rust-lang.org/std/ops/trait.DerefMut.html
/// [`ArenaBox`]: ./struct.ArenaBox.html
/// [`Clone`]: https://doc.rust-lang.org/std/clone/trait.Clone.html#tymethod.clone
///
pub struct ArenaRc<T> {
    block: NonNull<Block<T>>,
}

impl<T: std::fmt::Display> std::fmt::Display for ArenaRc<T> {
    /// ```
    /// # use shared_arena::{ArenaRc, SharedArena};
    /// let arena = SharedArena::new();
    /// let mut my_num = arena.alloc_rc(10);
    ///
    /// println!("{}", my_num);
    /// ```
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&**self, f)
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for ArenaRc<T> {
    /// ```
    /// # use shared_arena::{ArenaRc, SharedArena};
    /// let arena = SharedArena::new();
    /// let my_opt = arena.alloc_rc(Some(10));
    ///
    /// println!("{:?}", my_opt);
    /// ```
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

impl<T> std::fmt::Pointer for ArenaRc<T> {
    /// ```
    /// # use shared_arena::{ArenaRc, SharedArena};
    /// let arena = SharedArena::new();
    /// let my_num = arena.alloc_rc(10);
    ///
    /// println!("{:p}", my_num);
    /// ```
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ptr: *const T = &**self;
        std::fmt::Pointer::fmt(&ptr, f)
    }
}

impl<T> ArenaRc<T> {
    pub(crate) fn new(mut block: NonNull<Block<T>>) -> ArenaRc<T> {
        // ArenaRc is not Send, so we can make the counter non-atomic
        let counter_mut = unsafe { block.as_mut() }.counter.get_mut();

        // The bitfield indicated the block as free so it's guarantee to be zero,
        // but we check, just in case something went wrong

        assert!(
            *counter_mut == 0,
            "ArenaRc: Counter not zero {}",
            counter_mut
        );
        *counter_mut = 1;

        ArenaRc { block }
    }
}

impl<T> Clone for ArenaRc<T> {
    /// Make a clone of the ArenaRc pointer.
    ///
    /// This increase the reference counter.
    /// ```
    /// # use shared_arena::{ArenaRc, SharedArena};
    /// let arena = SharedArena::new();
    /// let my_num = arena.alloc_rc(10);
    ///
    /// assert_eq!(*my_num, *my_num.clone());
    /// ```
    #[inline]
    fn clone(&self) -> ArenaRc<T> {
        // ArenaRc is not Send, so we can make the counter non-atomic
        let counter_mut = unsafe { &mut *self.block.as_ptr() }.counter.get_mut();

        assert!(*counter_mut < isize::max_value() as usize);
        *counter_mut += 1;

        ArenaRc { block: self.block }
    }
}

impl<T> std::ops::Deref for ArenaRc<T> {
    type Target = T;

    /// ```
    /// # use shared_arena::{ArenaRc, SharedArena};
    /// let arena = SharedArena::new();
    /// let my_opt = arena.alloc_rc(Some(10));
    ///
    /// assert!(my_opt.is_some());
    /// ```
    fn deref(&self) -> &T {
        unsafe { &*self.block.as_ref().value.get() }
    }
}

/// Drop the ArenaRc<T> and decrement its reference counter
///
/// If it is the last reference to that value, the value is
/// also dropped
impl<T> Drop for ArenaRc<T> {
    /// ```
    /// # use shared_arena::{ArenaRc, Arena};
    /// let arena = Arena::new();
    /// let my_num = arena.alloc_rc(10);
    ///
    /// assert_eq!(arena.stats(), (1, 62));
    /// std::mem::drop(my_num);
    /// assert_eq!(arena.stats(), (0, 63));
    /// ```
    fn drop(&mut self) {
        // ArenaRc is not Send, so we can make the counter non-atomic
        let counter_mut = unsafe { self.block.as_mut() }.counter.get_mut();

        // // We decrement the reference counter
        *counter_mut -= 1;

        // We were the last reference
        if *counter_mut == 0 {
            Block::drop_block(self.block)
        };
    }
}

#[cfg(test)]
mod tests {
    use crate::Pool;

    #[test]
    fn arena_rc() {
        let pool = Pool::new();
        let rc = pool.alloc_rc(10);

        assert_eq!(*rc, 10);
        let rc2 = rc;

        assert_eq!(*rc2, 10);
    }
}
