use std::sync::atomic::AtomicIsize;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;

pub struct Statik<T> {
    pub __val: AtomicUsize,
    pub __lookers: AtomicIsize,
    pub __mk: fn() -> T,
}

macro_rules! statik {
    (static $name:ident: $t:ty = $e:expr) => (
        static $name: $crate::statik::Statik<$t> = {
            fn __mk() -> $t { $e }

            $crate::statik::Statik {
                __val: ::std::sync::atomic::ATOMIC_USIZE_INIT,
                __lookers: ::std::sync::atomic::ATOMIC_ISIZE_INIT,
                __mk: __mk,
            }
        };
    )
}

unsafe impl<T: Send + Sync> Send for Statik<T> {}
unsafe impl<T: Send + Sync> Sync for Statik<T> {}

impl<T> Statik<T> {
    pub fn with<F, R>(&'static self, f: F) -> Option<R>
        where F: FnOnce(&T) -> R
    {
        let mut lookers = self.__lookers.load(SeqCst);
        loop {
            if lookers < 0 {
                return None
            }
            match self.__lookers.compare_exchange(lookers, lookers + 1, SeqCst, SeqCst) {
                Ok(_) => break,
                Err(n) => lookers = n,
            }
        }

        unsafe {
            let mut val = self.__val.load(SeqCst);
            let ret = loop {
                match val {
                    0 => {}
                    n => break f(&*(n as *mut T)),
                }
                let ptr = Box::into_raw(Box::new((self.__mk)()));
                match self.__val.compare_exchange(0, ptr as usize, SeqCst, SeqCst) {
                    Ok(_) => val = ptr as usize,
                    Err(e) => {
                        drop(Box::from_raw(ptr));
                        val = e;
                    }
                }
            };
            self.drop();
            return Some(ret)
        }
    }

    pub fn drop(&'static self) -> bool {
        match self.__lookers.fetch_sub(1, SeqCst) {
            0 => {}
            _ => return false,
        }

        unsafe {
            let val = self.__val.load(SeqCst);
            if val != 0 {
                drop(Box::from_raw(val as *mut T));
            }
            return true
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::*;
    use std::thread;

    #[test]
    fn smoke() {
        statik!(static A: usize = 3);

        assert_eq!(A.with(|x| *x), Some(3));
        assert!(A.drop());
        assert_eq!(A.with(|x| *x), None);
    }

    #[test]
    fn smoke2() {
        statik!(static A: usize = panic!());

        assert!(A.drop());
        assert_eq!(A.with(|x| *x), None);
    }

    #[test]
    fn drop_drops() {
        static mut HITS: usize = 0;

        struct B;

        impl Drop for B {
            fn drop(&mut self) {
                unsafe { HITS += 1; }
            }
        }

        statik!(static A: B = B);

        unsafe {
            assert_eq!(HITS, 0);
            A.with(|_| ());
            assert_eq!(HITS, 0);
            assert!(A.drop());
            assert_eq!(HITS, 1);
            assert!(A.with(|_| ()).is_none());
            assert_eq!(HITS, 1);
        }
    }

    #[test]
    fn many_threads() {
        static HIT: AtomicBool = ATOMIC_BOOL_INIT;

        const N: usize = 4;
        const M: usize = 1_000_000;

        struct B;

        impl Drop for B {
            fn drop(&mut self) {
                assert!(!HIT.swap(true, Ordering::SeqCst));
            }
        }

        statik!(static A: B = B);

        let threads = (0..N).map(|_| {
            thread::spawn(|| {
                for _ in 0..M {
                    A.with(|_| ());
                }
                A.drop();
                for _ in 0..M/2 {
                    assert!(A.with(|_| ()).is_none());
                }
            })
        });
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(A.with(|_| ()).is_none());
        assert!(!A.drop());
        assert!(HIT.load(Ordering::SeqCst));
    }
}
