
use std::cell::UnsafeCell;

struct Block<T> {
    value: UnsafeCell<T>
}

//use super::page::BITFIELD_WIDTH as BLOCK_PER_PAGE;

struct FixedPoolBox {

}

struct FixedPool<T, const SIZE: usize> {
    bitfields: [usize; SIZE],
    blocks: [Block<T>; SIZE]
}

impl<T, const SIZE: usize> FixedPool<T, SIZE> {

    // fn acquire_free_block(&mut self)

    fn find_place(&mut self) -> Option<usize> {
        for (index, bitfield) in self.bitfields.iter_mut().enumerate() {
            let index_free = bitfield.trailing_zeros();

            if index_free == 64 {
                continue;
            }

            *bitfield &= !(1 << index_free);

            return Some(index + index_free as usize);
        }

        None
    }

    fn alloc(&mut self, value: T) -> Option<()> {

        None
    }
}
