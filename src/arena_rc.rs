
use std::ptr::NonNull;

use crate::block::Block;

#[repr(C)]
pub struct ArenaRc<T> {
    block: NonNull<Block<T>>,
}

impl<T: std::fmt::Debug> std::fmt::Debug for ArenaRc<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

impl<T> ArenaRc<T> {
    pub fn new(mut block: NonNull<Block<T>>) -> ArenaRc<T> {
        // ArenaRc is not Send, so we can make the counter non-atomic
        let counter_mut = unsafe { block.as_mut() }.counter.get_mut();

        // The bitfield indicated the block as free so it's guarantee to be zero,
        // but we check, just in case something went wrong

        assert!(*counter_mut == 0, "ArenaRc: Counter not zero {}", counter_mut);
        *counter_mut = 1;

        ArenaRc { block }
    }
}

impl<T> Clone for ArenaRc<T> {
    /// Make a clone of the ArenaRc pointer.
    ///
    /// This increase the reference counter.
    #[inline]
    fn clone(&self) -> ArenaRc<T> {
        // ArenaRc is not Send, so we can make the counter non-atomic
        let counter_mut = unsafe { &mut *self.block.as_ptr() }.counter.get_mut();

        assert!(*counter_mut < isize::max_value() as usize);
        *counter_mut += 1;

        ArenaRc {
            block: self.block
        }
    }
}

impl<T> std::ops::Deref for ArenaRc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.block.as_ref().value.get() }
    }
}

/// Drop the ArenaRc<T> and decrement its reference counter
///
/// If it is the last reference to that value, the value is
/// also dropped
impl<T> Drop for ArenaRc<T> {
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
        let rc2 = rc.clone();

        assert_eq!(*rc2, 10);
    }
}
