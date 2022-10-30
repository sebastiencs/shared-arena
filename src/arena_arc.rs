use std::ptr::NonNull;
use std::sync::atomic::Ordering::*;

use crate::block::Block;

/// A reference-counting pointer to `T` in the arena
///
/// The type `ArenaArc<T>` provides shared ownership of a value of
/// type `T` in the arena.  
/// Invoking [`Clone`] on `ArenaArc` produces a new `ArenaArc`
/// instance, which points to the same value, while increasing a
/// reference count.
///
/// When the last `ArenaArc` pointer to a given value is dropped,
/// the pointed-to value is also dropped and its dedicated memory
/// in the arena is marked as available for future allocation.
///
/// Shared mutable references in Rust is not allowed, if you need to
/// mutate through an `ArenaArc`, use a Mutex, RwLock or one of
/// the atomic types.
///
/// If you don't need to share the value, you should use [`ArenaBox`].
///
/// ## Cloning references
///
/// Creating a new reference from an existing reference counted pointer
/// is done using the `Clone` trait implemented for `ArenaArc<T>`
///
/// ## `Deref` behavior
///
/// `ArenaArc<T>` automatically dereferences to `T`, so you can call
/// `T`'s methods on a value of type `ArenaArc<T>`.
///
/// ```
/// # use shared_arena::{ArenaArc, SharedArena};
/// let arena = SharedArena::new();
/// let my_num: ArenaArc<i64> = arena.alloc_arc(-100i64);
///
/// assert!(my_num.is_negative());
/// assert_eq!(*my_num.clone(), -100);
/// ```
///
/// [`Arc`]: https://doc.rust-lang.org/std/sync/struct.Arc.html
/// [`Send`]: https://doc.rust-lang.org/std/marker/trait.Send.html
/// [`Sync`]: https://doc.rust-lang.org/std/marker/trait.Sync.html
/// [`deref`]: https://doc.rust-lang.org/std/ops/trait.Deref.html
/// [`Arena`]: ./struct.Arena.html
/// [`SharedArena`]: ./struct.SharedArena.html
/// [`ArenaBox`]: ./struct.ArenaBox.html
/// [`Clone`]: https://doc.rust-lang.org/std/clone/trait.Clone.html#tymethod.clone
///
pub struct ArenaArc<T> {
    block: NonNull<Block<T>>,
}

unsafe impl<T: Send> Send for ArenaArc<T> {}
unsafe impl<T: Send + Sync> Sync for ArenaArc<T> {}

impl<T: std::fmt::Display> std::fmt::Display for ArenaArc<T> {
    /// ```
    /// # use shared_arena::{ArenaArc, SharedArena};
    /// let arena = SharedArena::new();
    /// let my_num = arena.alloc(10);
    ///
    /// println!("{}", my_num);
    /// ```
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&**self, f)
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for ArenaArc<T> {
    /// ```
    /// # use shared_arena::{ArenaArc, SharedArena};
    /// let arena = SharedArena::new();
    /// let my_opt: ArenaArc<Option<i32>> = arena.alloc_arc(Some(10));
    ///
    /// println!("{:?}", my_opt);
    /// ```
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

impl<T> std::fmt::Pointer for ArenaArc<T> {
    /// ```
    /// # use shared_arena::{ArenaArc, SharedArena};
    /// let arena = SharedArena::new();
    /// let my_num = arena.alloc_arc(10);
    ///
    /// println!("{:p}", my_num);
    /// ```
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ptr: *const T = &**self;
        std::fmt::Pointer::fmt(&ptr, f)
    }
}

impl<T> ArenaArc<T> {
    pub(crate) fn new(block: NonNull<Block<T>>) -> ArenaArc<T> {
        let counter_ref = &unsafe { block.as_ref() }.counter;

        // Don't use compare_exchange here, it's too expensive
        // Relaxed load/store are just movs
        // We read the counter just for sanity check.
        // The bitfield indicated the block as free so it's guarantee to be zero,
        // but we check, just in case something went wrong

        let counter = counter_ref.load(Relaxed);
        assert!(counter == 0, "PoolArc: Counter not zero {}", counter);

        counter_ref.store(1, Relaxed);

        ArenaArc { block }
    }
}

impl<T> Clone for ArenaArc<T> {
    /// Make a clone of the ArenaArc pointer.
    ///
    /// This increase the reference counter.
    /// ```
    /// # use shared_arena::{ArenaArc, SharedArena};
    /// let arena = SharedArena::new();
    /// let my_num = arena.alloc_arc(10);
    ///
    /// assert_eq!(*my_num, *my_num.clone());
    /// ```
    #[inline]
    fn clone(&self) -> ArenaArc<T> {
        let counter_ref = &unsafe { self.block.as_ref() }.counter;

        let old = counter_ref.fetch_add(1, Relaxed);

        assert!(old < isize::max_value() as usize);

        ArenaArc { block: self.block }
    }
}

impl<T> std::ops::Deref for ArenaArc<T> {
    type Target = T;

    /// ```
    /// # use shared_arena::{ArenaArc, SharedArena};
    /// let arena = SharedArena::new();
    /// let my_opt: ArenaArc<Option<i32>> = arena.alloc_arc(Some(10));
    ///
    /// assert!(my_opt.is_some());
    /// ```
    fn deref(&self) -> &T {
        unsafe { &*self.block.as_ref().value.get() }
    }
}

/// Drop the ArenaArc<T> and decrement its reference counter
///
/// If it is the last reference to that value, the value is
/// also dropped
impl<T> Drop for ArenaArc<T> {
    /// ```
    /// # use shared_arena::{ArenaBox, Arena};
    /// let arena = Arena::new();
    /// let my_num = arena.alloc_arc(10);
    ///
    /// assert_eq!(arena.stats(), (1, 62));
    /// std::mem::drop(my_num);
    /// assert_eq!(arena.stats(), (0, 63));
    /// ```
    fn drop(&mut self) {
        let block = unsafe { self.block.as_ref() };

        // We decrement the reference counter
        let count = block.counter.fetch_sub(1, AcqRel);

        // We were the last reference
        if count == 1 {
            Block::drop_block(self.block)
        };
    }
}
