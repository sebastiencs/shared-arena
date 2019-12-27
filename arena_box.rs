

use std::sync::atomic::Ordering::*;
use std::ptr::NonNull;



use super::page::{Page, Block};

/// A pointer to `T` in the arena
///
/// The only difference with [`ArenaArc`] is that `ArenaBox` is not
/// clonable, it implements [`DerefMut`] so it is directly mutable
/// (without mutex or other synchronization methods).
///
/// There is no other difference, see the documentation of [`ArenaArc`]
/// for more information.
///
/// Note that even if it is not clonable, it's still can be sent to others
/// threads.
///
/// ```
/// use shared_arena::{ArenaBox, Arena};
///
/// let arena = Arena::new();
/// let mut my_vec: ArenaBox<_> = arena::alloc(Vec::new());
///
/// my_vec.push(1);
/// ```
///
/// [`ArenaArc`]: ./struct.ArenaArc.html
/// [`Arena`]: ./struct.Arena.html
/// [`SharedArena`]: ./struct.SharedArena.html
/// [`DerefMut`]: https://doc.rust-lang.org/std/ops/trait.DerefMut.html
///
// Implementation details:
//
// We make the struct repr(C) to ensure that the pointer to Block remains
// at offset 0. This is to avoid any pointer arithmetic when dereferencing the
// inner value
//
// TODO: Should we use a tagged pointer here ?
// The pointer to Page is used to have access to the bitfield and deallocate
// the Page when necessary.
// However we could tag the block pointer to retrieve the Page and so ArenaBox
// would have the size of 1 pointer only, instead of 2 now.
// This has 2 inconvenients:
// - Block would be aligned on 64 bytes to allow a big enough tag
//   This could make the Page way bigger than necessary
//   Or we could use the unused msb in the pointer (16 with 64 bits ptrs) but
//   it would not work with 32 bits ptrs
// - Dereferencing would involved removing the tag
//   Though the compiler could cache the pointer somehow on its first used
//
// The inconvenients with 2 pointers:
// - Its size, when moving the struct around
// - Do not allow non-null pointer optimization (e.g with Option<ArenaBox<T>>)
//
// Benchmarks have to be made.
#[repr(C)]
pub struct ArenaBox<T> {
    block: NonNull<Block<T>>,
    page: NonNull<Page<T>>,
}

unsafe impl<T: Send> Send for ArenaBox<T> {}
unsafe impl<T: Send + Sync> Sync for ArenaBox<T> {}

impl<T: std::fmt::Debug> std::fmt::Debug for ArenaBox<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

impl<T> ArenaBox<T> {
    pub fn new(page: NonNull<Page<T>>, block: NonNull<Block<T>>) -> ArenaBox<T> {
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

        ArenaBox { block, page }
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
        let (page, block) = unsafe {
            (self.page.as_mut(), self.block.as_ref())
        };

        // See ArenaBox<T>::new for why we touch the counter

        let counter_ref = &block.counter;

        let counter = counter_ref.load(Relaxed);
        assert!(counter == 1, "PoolBox: Counter != 1 on drop {}", counter);

        counter_ref.store(0, Relaxed);

        page.drop_block(block);
    }
}
