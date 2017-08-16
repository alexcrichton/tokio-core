use std::cell::UnsafeCell;

pub struct CoreCell<T> {
    inner: UnsafeCell<T>,
}

pub struct CoreProof(());

unsafe impl<T: Send> Send for CoreCell<T> {}
unsafe impl<T: Send> Sync for CoreCell<T> {} // TODO: note difference here

impl<T> CoreCell<T> {
    pub fn new(t: T) -> CoreCell<T> {
        CoreCell { inner: UnsafeCell::new(t) }
    }

    // pub unsafe fn get<'a>(&'a self, _core: &'a CoreProof) -> &'a T {
    //     &*self.inner.get()
    // }

    // TODO: is this actually safe?
    pub fn get_mut<'a>(&'a self, _core: &'a mut CoreProof) -> &'a mut T {
        unsafe { &mut *self.inner.get() }
    }

    pub unsafe fn into_inner(self) -> T {
        self.inner.into_inner()
    }
}

impl CoreProof {
    pub unsafe fn new() -> CoreProof {
        CoreProof(())
    }
}
