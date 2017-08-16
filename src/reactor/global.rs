use std::io;
use std::sync::{Once, ONCE_INIT};
use std::thread;

use reactor::Core;
use futures::future;

pub fn default_core() -> io::Result<&'static Core> {
    static INIT: Once = ONCE_INIT;
    static mut CORE: *const Core = 0 as *const Core;

    unsafe {
        let mut err = None;
        INIT.call_once(|| {
            let core = match Core::new() {
                Ok(c) => c,
                Err(e) => {
                    err = Some(e);
                    return
                }
            };
            CORE = Box::into_raw(Box::new(core));
            thread::spawn(|| helper(&*CORE));
        });
        if CORE.is_null() {
            match err {
                Some(e) => Err(e),
                None => Err(io::Error::new(io::ErrorKind::Other,
                                           "creation of core failed"))
            }
        } else {
            Ok(&*CORE)
        }
    }
}

fn helper(core: &'static Core) {
    core.run(future::empty::<(), ()>()).unwrap();
}
