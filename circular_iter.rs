use std::marker::PhantomData;

// A slightly faster iterator than using
// slice::split_at -> Vec::iter -> Iterator::chain -> Iterator::enumerate

pub struct CircularIter<'a, T> {
    start: *const T,
    end: *const T,
    next_start: *const T,
    next_end: *const T,
    index: usize,
    _marker: PhantomData<&'a T>
}

pub struct CircularIterMut<'a, T> {
    start: *mut T,
    end: *mut T,
    next_start: *mut T,
    next_end: *mut T,
    index: usize,
    _marker: PhantomData<&'a T>
}

impl<'a, T> Iterator for CircularIter<'a, T> {
    type Item = (usize, &'a T);

    fn next(&mut self) -> Option<(usize, &'a T)> {
        if self.start != self.end {
            let current = self.start;
            let index = self.index;
            self.index += 1;
            unsafe {
                self.start = current.add(1);
                Some((index, &*current))
            }
        } else if self.next_start != self.next_end {
            let next = unsafe { &*self.next_start };
            let index = self.index;

            self.start = unsafe { self.next_start.add(1) };
            self.end = self.next_end;
            self.next_start = std::ptr::null();
            self.next_end = std::ptr::null();
            self.index += 1;
            Some((index, next))
        } else {
            None
        }
    }
}

impl<'a, T> Iterator for CircularIterMut<'a, T> {
    type Item = (usize, &'a mut T);

    #[inline]
    fn next(&mut self) -> Option<(usize, &'a mut T)> {
        if self.start != self.end {
            let current = self.start;
            let index = self.index;
            self.index += 1;
            unsafe {
                self.start = current.add(1);
                Some((index, &mut *current))
            }
        } else if self.next_start != self.next_end {
            let next = unsafe { &mut *self.next_start };
            let index = self.index;

            self.start = unsafe { self.next_start.add(1) };
            self.end = self.next_end;
            self.next_start = std::ptr::null_mut();
            self.next_end = std::ptr::null_mut();
            self.index += 1;
            Some((index, next))
        } else {
            None
        }
    }
}

pub trait CircularIterator<T> {
    fn circular_iter(&self, split_at: usize) -> CircularIter<T>;
    fn circular_iter_mut(&mut self, split_at: usize) -> CircularIterMut<T>;
}

impl<T> CircularIterator<T> for Vec<T> {
    #[inline]
    fn circular_iter(&self, split_at: usize) -> CircularIter<T> {
        // Keep this branchless

        let len = self.len();
        let ptr = self.as_ptr();
        let split_at = split_at % len;

        unsafe {
            let start = ptr.add(split_at);
            CircularIter {
                start,
                end: ptr.add(len),
                next_start: ptr,
                next_end: start,
                index: split_at,
                _marker: PhantomData
            }
        }
    }

    #[inline]
    fn circular_iter_mut(&mut self, split_at: usize) -> CircularIterMut<T> {
        // Keep this branchless

        let len = self.len();
        let ptr = self.as_mut_ptr();
        let split_at = split_at % len;

        unsafe {
            let start = ptr.add(split_at);
            CircularIterMut {
                start,
                end: ptr.add(len),
                next_start: ptr,
                next_end: start,
                index: split_at,
                _marker: PhantomData
            }
        }
    }
}
