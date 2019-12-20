
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::ptr::NonNull;

use static_assertions::const_assert;

use super::page::{IndexInPage, Page, Block};

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
/// use shared_arena::{ArenaBox, SharedArena};
///
/// let arena = shared_arena::new();
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
    #[inline(never)]
    pub fn new(page: NonNull<Page<T>>, block: NonNull<Block<T>>) -> ArenaBox<T> {
        // let block = &page.nodes[index_in_page];

        // let counter = unsafe { block.as_ref() }.counter.load(Ordering::Relaxed);

        // if counter != 0 {
        //     let index = unsafe { block.as_ref() }.index_in_page;
        //     panic!("PoolBox Counter not zero at index {} {}", index, counter);
        // }

        // assert!(counter == 0, "PoolBox: Counter not zero {}", counter);

        // We still store 1 in the counter to make the asserts works
        // unsafe { block.as_ref() }.counter.store(1, Ordering::Release);
        // let block = NonNull::from(block);

        //let block = unsafe { std::mem::uninitialized() };

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

        // let counter = block.counter.fetch_sub(1, Ordering::Acquire);
        // assert!(counter == 1, "ArenaBox has a counter != 1 on drop {}", counter);

        super::arena_arc::drop_block_in_arena(page, block);
    }
}
