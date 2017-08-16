use std::marker::PhantomData;
use std::mem;
use std::ptr;
use std::sync::{Arc, Weak};

use futures::executor::{Notify, NotifyHandle, UnsafeNotify};

struct WeakWrapped<T>(PhantomData<T>);

pub struct WeakHandle<'a, T: 'a>(pub &'a Arc<T>);

impl<'a, T> From<WeakHandle<'a, T>> for NotifyHandle
    where T: Notify + 'static,
{
    fn from(handle: WeakHandle<'a, T>) -> NotifyHandle {
        weak2handle(Arc::downgrade(handle.0))
    }
}

impl<'a, T> Clone for WeakHandle<'a, T> {
    fn clone(&self) -> WeakHandle<'a, T> {
        WeakHandle(self.0)
    }
}

fn weak2handle<T: Notify + 'static>(rc: Weak<T>) -> NotifyHandle {
    unsafe {
        let ptr = mem::transmute::<Weak<T>, *mut WeakWrapped<T>>(rc);
        NotifyHandle::new(ptr)
    }
}

impl<T: Notify + 'static> Notify for WeakWrapped<T> {
    fn notify(&self, id: usize) {
        unsafe {
            let me: *const WeakWrapped<T> = self;
            let me: &Weak<T> = &*(&me as *const *const WeakWrapped<T> as *const Weak<T>);
            if let Some(ptr) = me.upgrade() {
                ptr.notify(id);
            }
        }
    }
}

unsafe impl<T: Notify + 'static> UnsafeNotify for WeakWrapped<T> {
    unsafe fn clone_raw(&self) -> NotifyHandle {
        let me: *const WeakWrapped<T> = self;
        let arc = (*(&me as *const *const WeakWrapped<T> as *const Weak<T>)).clone();
        weak2handle(arc)
    }

    unsafe fn drop_raw(&self) {
        let mut me: *const WeakWrapped<T> = self;
        let me = &mut me as *mut *const WeakWrapped<T> as *mut Weak<T>;
        ptr::drop_in_place(me);
    }
}

