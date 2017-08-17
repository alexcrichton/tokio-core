use std::cell::UnsafeCell;
use std::marker;
use std::mem;
use std::ptr;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;

const N: usize = 1024;
const M: usize = 3;
const SLOT_IDX_MASK: usize = N - 1;

pub struct AtomicSlab<T> {
    next_free: AtomicUsize,
    ever_allocated: AtomicUsize,
    slots: Box<[AtomicUsize; N]>,
    _data: marker::PhantomData<T>,
}

struct Slot<T> {
    state: AtomicUsize,
    data: UnsafeCell<T>,
}

impl<T> AtomicSlab<T> {
    pub fn new() -> AtomicSlab<T> {
        AtomicSlab {
            next_free: AtomicUsize::new(usize::max_value()),
            ever_allocated: AtomicUsize::new(0),
            slots: slots(),
            _data: marker::PhantomData,
        }
    }

    pub fn insert(&self, t: T) -> usize {
        let mut idx = self.next_free.load(SeqCst);
        let slot = loop {
            if idx == usize::max_value() {
                idx = self.ever_allocated.fetch_add(1, SeqCst);
                break self.allocate(&self.slots, idx, 0);
            } else {
                // Given our index, drill down to the actual `Slot<T>` so we can
                // take a look at it
                let slot = self.locate(idx).expect("next free points to empty");

                // If this slot is allocate (state == MAX-1) then we skip it.
                // Note that if we see this value of `state` then we should be
                // guaranteed that `next_free` has changed due to where we store
                // `MAX - 1` below. This ni turn means that we should be able to
                // continue making progress.
                //
                // TODO: is this check necessary? I think we cen rely on the
                //       compare_exchange below to make progress and
                //       exclusivity.
                let state = slot.state.load(SeqCst);
                if state == usize::max_value() - 1 {
                    let next = self.next_free.load(SeqCst);
                    assert!(next != idx);
                    idx = next;
                    continue
                }

                // Woohoo we've got a free slot! Try to move our `next_free`
                // poitner to whatever our slot said its next free pointer was.
                // If we succeed at this then we've acquired the slot. If we
                // fail then we try it all again and `next_free` will have
                // changed.
                let res = self.next_free.compare_exchange(idx, state, SeqCst, SeqCst);
                if let Err(next) = res {
                    idx = next;
                    continue
                }
                break slot
            }
        };

        // Once we've got a slot the first thing we do is store our data into
        // it. Only after this do we flag the slot as allocated to ensure that
        // concurrent calls to `get` don't expose uninitialized data.
        //
        // Once the value is stored we flag the slot as allocated and in use
        // (MAX-1) and then we're able to hand it out via `get` and such.
        unsafe {
            ptr::write(slot.data.get(), t);
        }
        let prev = slot.state.swap(usize::max_value() - 1, SeqCst);
        assert!(prev != usize::max_value() - 1);
        return idx
    }

    /// Fetches the value for a corresponding slab index
    ///
    /// This function will look up `idx` internally, returning the `T`
    /// corresponding to it. If the `idx` is not allocate within this slab then
    /// `None` is returned.
    pub fn get(&self, idx: usize) -> Option<&T> {
        let slot = match self.locate(idx) {
            Some(slot) => slot,
            None => return None,
        };
        let state = slot.state.load(SeqCst);
        if state == usize::max_value() - 1 {
            Some(unsafe { &*slot.data.get() })
        } else {
            None
        }
    }

    /// Removes an entry from this slab.
    ///
    /// This is unsafe because you must externally ensure that there's no
    /// references handed out from `get` that are active. This is only safe if
    /// no `get` references are alive.
    pub unsafe fn remove(&self, idx: usize) -> T {
        let slot = self.locate(idx).expect("index not present");
        let state = slot.state.swap(usize::max_value(), SeqCst);
        assert_eq!(state, usize::max_value() - 1);
        let ret = ptr::read(slot.data.get());

        let mut next = self.next_free.load(SeqCst);
        loop {
            slot.state.store(next, SeqCst);
            match self.next_free.compare_exchange(next, idx, SeqCst, SeqCst) {
                Ok(_) => return ret,
                Err(n) => next = n,
            }
        }
    }

    fn allocate(&self,
                arr: &[AtomicUsize; N],
                idx: usize,
                level: usize) -> &Slot<T> {
        if level == M - 1 {
            let slot = Box::new(Slot {
                state: AtomicUsize::new(usize::max_value()),
                data: UnsafeCell::new(unsafe { mem::uninitialized() }),
            });
            let slot = Box::into_raw(slot);
            match arr[idx].compare_exchange(0, slot as usize, SeqCst, SeqCst) {
                Ok(_) => return unsafe { &*(slot as *const Slot<T>) },
                Err(_) => panic!("allocated slot previously taken"),
            }
        } else {
            let slot = &arr[idx & SLOT_IDX_MASK];
            let mut next = slot.load(SeqCst);
            if next == 0 {
                let new = slots();
                let new = Box::into_raw(new);
                match slot.compare_exchange(0, new as usize, SeqCst, SeqCst) {
                    Ok(_) => next = new as usize,
                    Err(other) => {
                        next = other;
                        unsafe {
                            drop(Box::from_raw(new));
                        }
                    }
                }
            }
            unsafe {
                self.allocate(&*(next as *const [AtomicUsize; N]),
                              idx >> shift(),
                              level + 1)
            }
        }
    }

    fn locate(&self, mut idx: usize) -> Option<&Slot<T>> {
        let mut arr = &*self.slots;
        let mut level = 0;
        while level < M - 1 {
            let slot = &arr[idx & SLOT_IDX_MASK];
            let next = slot.load(SeqCst);
            if next == 0 {
                return None
            }
            idx >>= shift();
            level += 1;
            unsafe {
                arr = &*(next as *const [AtomicUsize; N]);
            }
        }
        match arr[idx & SLOT_IDX_MASK].load(SeqCst) {
            0 => None,
            n => Some(unsafe { &*(n as *const Slot<T>) })
        }
    }
}

impl<T> Drop for AtomicSlab<T> {
    fn drop(&mut self) {
        free::<T>(&mut self.slots, 0);

        fn free<T>(arr: &mut [AtomicUsize; N], level: usize) {
            for slot in arr.iter_mut() {
                let value = *slot.get_mut();
                if value == 0 {
                    continue
                }

                unsafe {
                    if level == M - 1 {
                        free_slot(value as *mut Slot<T>);
                    } else {
                        let value = value as *mut [AtomicUsize; N];
                        free::<T>(&mut *value, level + 1);
                        drop(Box::from_raw(value));
                    }
                }
            }
        }

        unsafe fn free_slot<T>(slot: *mut Slot<T>) {
            // If this is an allocate slot, drop it directly as we want to drop
            // the data inside.
            if *(*slot).state.get_mut() == usize::max_value() - 1 {
                return drop(Box::from_raw(slot))
            }

            // If there's no data in here, we need to go through some trickery
            // to avoid dropping uninitialized or duplicate data.
            drop(Vec::from_raw_parts(slot, 0, 1));
        }
    }
}

fn shift() -> u32 {
    N.trailing_zeros()
}

fn slots() -> Box<[AtomicUsize; N]> {
    let v = (0..N).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>().into_boxed_slice();
    assert_eq!(v.len(), N);
    let ptr: *const AtomicUsize = v.as_ptr();
    unsafe {
        mem::forget(v);
        Box::from_raw(ptr as *mut [AtomicUsize; N])
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::thread;

    use super::*;

    #[test]
    fn smoke() {
        let a = AtomicSlab::new();
        assert_eq!(a.insert(3), 0);
        unsafe { assert_eq!(a.remove(0), 3); }
    }

    #[test]
    fn smoke2() {
        let a = AtomicSlab::new();
        for i in 0..N * 2 {
            assert_eq!(a.insert(4), i);
        }
        for i in 0..N * 2 {
            unsafe { assert_eq!(a.remove(i), 4); }
        }
    }

    #[test]
    fn smoke3() {
        let a = AtomicSlab::new();
        for _ in 0..N * 2 {
            assert_eq!(a.insert(5), 0);
            unsafe { assert_eq!(a.remove(0), 5); }
        }
    }

    #[test]
    fn threads() {
        const N: usize = 4;
        const M: usize = 4000;

        let go = Arc::new(AtomicBool::new(false));
        let a = Arc::new(AtomicSlab::new());
        let threads = (0..N).map(|i| {
            let go = go.clone();
            let a = a.clone();
            thread::spawn(move || {
                while !go.load(SeqCst) {}

                for _ in 0..M {
                    a.insert(i);
                }
            })
        }).collect::<Vec<_>>();
        go.store(true, SeqCst);
        for thread in threads {
            thread.join().unwrap();
        }
        let mut cnts = vec![0; N];
        for i in 0..N * M {
            unsafe { cnts[a.remove(i)] += 1; }
        }
        for cnt in cnts {
            assert_eq!(cnt, M);
        }
    }
}
