
#[cfg_attr(
    any(
        target_arch = "mips",
        target_arch = "arm",
        target_arch = "aarch64",
        target_arch = "mips64",
        target_arch = "mips64el"
    ),
    repr(align(32))
)]
#[cfg_attr(
    any(
        target_arch = "x86",
        target_arch = "powerpc",
// https://community.arm.com/developer/ip-products/processors/f/cortex-a-forum/13570/cortex-a7-cache-line-size
        target_arch = "armv7",
        target_arch = "armv7r",
    ),
    repr(align(64))
)]
#[cfg_attr(
    any(
        target_arch = "x86_64",
        target_arch = "powerpc64",
    ),
    repr(align(128))
)]
#[cfg_attr(any(target_arch = "s390x"), repr(align(256)))]
#[cfg_attr(any(target_arch = "wasm32"), repr(align(0)))]
#[derive(Debug)]
pub struct CacheAligned<T: Sized>(T);

impl<T> CacheAligned<T> {
    pub fn new(v: T) -> CacheAligned<T> {
        CacheAligned(v)
    }
}

impl<T: Copy + Clone> Copy for CacheAligned<T> {}

impl<T: Clone> Clone for CacheAligned<T> {
    fn clone(&self) -> CacheAligned<T> {
        CacheAligned(self.0.clone())
    }
}

impl<T: Sized> std::ops::Deref for CacheAligned<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: Sized> std::ops::DerefMut for CacheAligned<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
