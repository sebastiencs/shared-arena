use std::cell::UnsafeCell;
use std::ptr::NonNull;
use std::sync::atomic::AtomicUsize;

use crate::page::{arena::PageArena, pool::PagePool, shared_arena::PageSharedArena};

// // https://stackoverflow.com/a/53646925
// const fn max(a: usize, b: usize) -> usize {
//     [a, b][(a < b) as usize]
// }

// const ALIGN_BLOCK: usize = max(128, 64);

// We make the struct repr(C) to ensure that the pointer to the inner
// value remains at offset 0. This is to avoid any pointer arithmetic
// when dereferencing it
#[repr(C)]
pub struct Block<T> {
    /// Inner value
    pub value: UnsafeCell<T>,
    /// Number of references to this block
    pub counter: AtomicUsize,
    /// Information about its page.
    /// It's a tagged pointer on 64 bits architectures.
    /// Contains:
    ///   - Pointer to page
    ///   - Index of the block in page
    ///   - PageKind
    /// Read only and initialized on Page creation.
    /// Doesn't need to be atomic.
    pub(crate) page: PageTaggedPtr,
}

impl<T> Block<T> {
    pub(crate) fn drop_block(block: NonNull<Block<T>>) {
        let block_ref = unsafe { block.as_ref() };

        match block_ref.page.page_kind() {
            PageKind::SharedArena => {
                let page_ptr = block_ref.page.page_ptr::<PageSharedArena<T>>();
                PageSharedArena::<T>::drop_block(page_ptr, block);
            }
            PageKind::Arena => {
                let page_ptr = block_ref.page.page_ptr::<PageArena<T>>();
                PageArena::<T>::drop_block(page_ptr, block);
            }
            PageKind::Pool => {
                let page_ptr = block_ref.page.page_ptr::<PagePool<T>>();
                PagePool::<T>::drop_block(page_ptr, block);
            }
        }
    }
}

#[cfg(target_pointer_width = "64")]
#[derive(Copy, Clone)]
pub(crate) struct PageTaggedPtr {
    pub(crate) data: usize,
}

#[cfg(not(target_pointer_width = "64"))]
#[derive(Copy, Clone)]
pub(crate) struct PageTaggedPtr {
    pub(crate) ptr: usize,
    pub(crate) data: usize,
}

impl std::fmt::Debug for PageTaggedPtr {
    #[cfg(target_pointer_width = "64")]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageTaggedPtr")
            .field("data    ", &format!("{:064b}", self.data))
            .field(
                "page_ptr",
                &format!("{:064b}", self.page_ptr::<usize>().as_ptr() as usize),
            )
            .field("page_kind", &self.page_kind())
            .field(
                "page_index_block",
                &format!("{:08b} ({})", self.index_block(), self.index_block()),
            )
            .finish()
    }

    #[cfg(not(target_pointer_width = "64"))]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageTaggedPtr")
            .field(
                "page_ptr",
                &format!("{:032b}", self.page_ptr::<usize>().as_ptr() as usize),
            )
            .field("data    ", &format!("{:032b}", self.data))
            .field("page_kind", &self.page_kind())
            .field(
                "page_index_block",
                &format!("{:08b} ({})", self.index_block(), self.index_block()),
            )
            .finish()
    }
}

impl PageTaggedPtr {
    #[cfg(target_pointer_width = "64")]
    pub(crate) fn new(page_ptr: usize, index: usize, kind: PageKind) -> PageTaggedPtr {
        let tag = Self::make_tag(index, kind);

        PageTaggedPtr {
            data: (page_ptr & !(0b11111111 << 56)) | (tag << 56),
        }
    }

    #[cfg(not(target_pointer_width = "64"))]
    pub(crate) fn new(page_ptr: usize, index: usize, kind: PageKind) -> PageTaggedPtr {
        let tag = Self::make_tag(index, kind);

        PageTaggedPtr {
            ptr: page_ptr,
            data: tag,
        }
    }

    pub(crate) fn make_tag(index: usize, kind: PageKind) -> usize {
        let kind: usize = kind.into();
        // Index is 6 bits at most
        // Kind is 2 bit
        let kind = kind << 6;

        // Tag is 8 bits
        kind | index
    }

    #[cfg(target_pointer_width = "64")]
    pub(crate) fn page_ptr<T>(self) -> NonNull<T> {
        let ptr = ((self.data << 8) as isize >> 8) as *mut T;

        NonNull::new(ptr).expect("Invalid pointer")
    }

    #[cfg(not(target_pointer_width = "64"))]
    pub(crate) fn page_ptr<T>(self) -> NonNull<T> {
        NonNull::new(self.ptr as *mut T).unwrap()
    }

    pub(crate) fn page_kind(self) -> PageKind {
        PageKind::from(self)
    }

    pub(crate) fn index_block(self) -> usize {
        #[cfg(target_pointer_width = "64")]
        let rotate = 56;
        #[cfg(not(target_pointer_width = "64"))]
        let rotate = 0;

        (self.data >> rotate) & 0b111111
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum PageKind {
    SharedArena = 0,
    Arena = 1,
    Pool = 2,
}

impl From<PageTaggedPtr> for PageKind {
    fn from(source: PageTaggedPtr) -> Self {
        #[cfg(target_pointer_width = "64")]
        let rotate = 62;
        #[cfg(not(target_pointer_width = "64"))]
        let rotate = 6;

        let kind = source.data >> rotate;

        match kind {
            0 => PageKind::SharedArena,
            1 => PageKind::Arena,
            2 => PageKind::Pool,
            _ => panic!("Invalid page kind"),
        }
    }
}

impl Into<usize> for PageKind {
    fn into(self) -> usize {
        match self {
            PageKind::SharedArena => 0,
            PageKind::Arena => 1,
            PageKind::Pool => 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PageKind, PageTaggedPtr};

    #[test]
    fn pagekind_into() {
        let num: usize = PageKind::SharedArena.into();
        assert_eq!(num, 0);

        let num: usize = PageKind::Arena.into();
        assert_eq!(num, 1);

        let num: usize = PageKind::Pool.into();
        assert_eq!(num, 2);
    }

    #[test]
    fn page_tagged_ptr() {
        let real_ptr = Box::into_raw(Box::new(1));

        for index_block in 0..64 {
            let tagged_ptr =
                PageTaggedPtr::new(real_ptr as usize, index_block, PageKind::SharedArena);
            let ptr = tagged_ptr.page_ptr::<usize>().as_ptr();
            assert_eq!(ptr, real_ptr as *mut _, "{:064b}", ptr as usize);
            assert_eq!(tagged_ptr.page_kind(), PageKind::SharedArena);
            assert_eq!(tagged_ptr.index_block(), index_block);

            let tagged_ptr = PageTaggedPtr::new(real_ptr as usize, index_block, PageKind::Arena);
            let ptr = tagged_ptr.page_ptr::<usize>().as_ptr();
            assert_eq!(ptr, real_ptr as *mut _, "{:064b}", ptr as usize);
            assert_eq!(tagged_ptr.page_kind(), PageKind::Arena);
            assert_eq!(tagged_ptr.index_block(), index_block);

            let tagged_ptr = PageTaggedPtr::new(real_ptr as usize, index_block, PageKind::Pool);
            let ptr = tagged_ptr.page_ptr::<usize>().as_ptr();
            assert_eq!(ptr, real_ptr as *mut _, "{:064b}", ptr as usize);
            assert_eq!(tagged_ptr.page_kind(), PageKind::Pool);
            assert_eq!(tagged_ptr.index_block(), index_block);

            let tagged_ptr = PageTaggedPtr::new(16, index_block, PageKind::SharedArena);
            let ptr = tagged_ptr.page_ptr::<usize>().as_ptr();
            assert_eq!(ptr, 16 as *mut _, "{:064b}", ptr as usize);
            assert_eq!(tagged_ptr.page_kind(), PageKind::SharedArena);
            assert_eq!(tagged_ptr.index_block(), index_block);

            let tagged_ptr = PageTaggedPtr::new(16, index_block, PageKind::Arena);
            let ptr = tagged_ptr.page_ptr::<usize>().as_ptr();
            assert_eq!(ptr, 16 as *mut _, "{:064b}", ptr as usize);
            assert_eq!(tagged_ptr.page_kind(), PageKind::Arena);
            assert_eq!(tagged_ptr.index_block(), index_block);

            let tagged_ptr = PageTaggedPtr::new(16, index_block, PageKind::Pool);
            let ptr = tagged_ptr.page_ptr::<usize>().as_ptr();
            assert_eq!(ptr, 16 as *mut _, "{:064b}", ptr as usize);
            assert_eq!(tagged_ptr.page_kind(), PageKind::Pool);
            assert_eq!(tagged_ptr.index_block(), index_block);
        }

        unsafe { Box::from_raw(real_ptr) };
    }

    #[test]
    fn page_tagged_ptr_debug() {
        let real_ptr = Box::into_raw(Box::new(1));

        let tagged_ptr = PageTaggedPtr::new(real_ptr as usize, 63, PageKind::Arena);
        println!("{:?} {:?}", tagged_ptr.clone(), PageKind::Arena);

        let tagged_ptr_2 = tagged_ptr;
        let tagged_ptr_3 = tagged_ptr_2.clone();

        assert!(tagged_ptr.data == tagged_ptr_2.data);
        assert!(tagged_ptr.data == tagged_ptr_3.data);

        unsafe { Box::from_raw(real_ptr) };
    }

    #[test]
    #[should_panic]
    fn invalid_block() {
        use std::cell::UnsafeCell;
        use std::ptr::NonNull;
        use std::sync::atomic::AtomicUsize;

        let mut block = super::Block {
            value: UnsafeCell::new(1),
            counter: AtomicUsize::new(1),
            page: super::PageTaggedPtr {
                data: !0,
                #[cfg(not(target_pointer_width = "64"))]
                ptr: !0,
            },
        };

        super::Block::drop_block(NonNull::from(&mut block));
    } // grcov_ignore

    #[test]
    #[should_panic]
    fn invalid_tagged_ptr() {
        super::PageKind::from(super::PageTaggedPtr {
            data: !0,
            #[cfg(not(target_pointer_width = "64"))]
            ptr: !0,
        });
    } // grcov_ignore
}
