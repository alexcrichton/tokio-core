use std::cell::UnsafeCell;

/// An unsafe abstraction to be used with `CoreProof` below to prove that a span
/// of code has exclusive access.
///
/// There's a number of variables can in theory be accessed concurrently in a
/// `Core`, but they're practically only ever accessed by the "one polling
/// thread" which provides the ability for exclusive access to the contents.
/// This cell is an attempt to make these access patterns safe.
pub struct CoreCell<T> {
    inner: UnsafeCell<T>,
}

pub struct CoreProof(());

// Like `Mutex` we promote `Send` to `Sync` as the inner values themselves don't
// need to be `Sync` as they're only accessed in a synchronized manner.
unsafe impl<T: Send> Send for CoreCell<T> {}
unsafe impl<T: Send> Sync for CoreCell<T> {}

impl<T> CoreCell<T> {
    pub fn new(t: T) -> CoreCell<T> {
        CoreCell { inner: UnsafeCell::new(t) }
    }

    /// Acquires safe accesss to the internals of this cell.
    ///
    /// This function will take a mutable loan on an instance of `CoreProof`.
    /// For each core there's only one instance of `CoreProof`, so if we can
    /// prove we have exclusive access to `CoreProof` then we can prove that we
    /// *should* have exclusive access to the contents of this cell.
    ///
    /// Note that the lifetimes are all the same here to tie everything
    /// together, importantly so!
    pub fn get_mut<'a>(&'a self, _core: &'a mut CoreProof) -> &'a mut T {
        unsafe { &mut *self.inner.get() }
    }

    pub unsafe fn into_inner(self) -> T {
        self.inner.into_inner()
    }
}

impl CoreProof {
    /// This function is unsafe because multiple instances of `CoreProof` can
    /// make the `get_mut` function above unsafe. It's intended that there's
    /// only one instance of this type per core.
    pub unsafe fn new() -> CoreProof {
        CoreProof(())
    }
}
