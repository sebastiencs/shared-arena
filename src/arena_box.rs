

use std::sync::atomic::Ordering::*;
use std::ptr::NonNull;

use crate::block::Block;

/// A pointer to `T` in the arena
///
/// `ArenaBox` implements [`DerefMut`] so it is directly mutable
/// (without mutex or other synchronization methods).
///
/// It is not clonable and can be sent to others threads.
///
/// ## `Deref` & `DerefMut` behavior
///
/// `ArenaBox<T>` automatically dereferences to `T`, so you can call
/// `T`'s methods on a value of type `ArenaBox<T>`.
///
/// ```
/// # use shared_arena::{ArenaBox, SharedArena};
/// let arena = SharedArena::new();
/// let mut my_opt: ArenaBox<Option<i32>> = arena.alloc(Some(10));
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
pub struct ArenaBox<T> {
    block: NonNull<Block<T>>,
}

unsafe impl<T: Send> Send for ArenaBox<T> {}
unsafe impl<T: Send + Sync> Sync for ArenaBox<T> {}

impl<T: std::fmt::Debug> std::fmt::Debug for ArenaBox<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

impl<T> ArenaBox<T> {
    pub fn new(block: NonNull<Block<T>>) -> ArenaBox<T> {
        let counter_ref = &unsafe { block.as_ref() }.counter;

        // See ArenaArc<T>::new for more info.
        // We should avoid touching the counter with ArenaBox, but still do it
        // for sanity and be 100% sure to avoid any memory corruption.
        // The slowest operation here is dereferencing the block, though.
        // So ArenaArc and ArenaBox have the same cost on creation.
        // However dropping an ArenaBox is cheaper.

        let counter = counter_ref.load(Relaxed);
        assert!(counter == 0, "PoolBox: Counter not zero {}", counter);

        counter_ref.store(1, Relaxed);

        ArenaBox { block }
    }
}

impl<T> std::ops::Deref for ArenaBox<T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.block.as_ref().value.get() }
    }
}

impl<T> std::ops::DerefMut for ArenaBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.block.as_ref().value.get() }
    }
}

/// Drop the ArenaBox<T>
///
/// The value pointed by this ArenaBox is also dropped
impl<T> Drop for ArenaBox<T> {
    fn drop(&mut self) {
        let block = unsafe { self.block.as_ref() };

        // See ArenaBox<T>::new for why we touch the counter

        let counter_ref = &block.counter;

        let counter = counter_ref.load(Relaxed);
        assert!(counter == 1, "PoolBox: Counter != 1 on drop {}", counter);

        counter_ref.store(0, Relaxed);

        Block::drop_block(self.block)
    }
}
