use std::io;
use std::sync::Arc;
use std::thread;

use futures::sync::oneshot;
use reactor::{Core, Inner};

struct HelperThread {
    thread: Option<thread::JoinHandle<()>>,
    tx: Option<oneshot::Sender<()>>,
    core: Option<Arc<Inner>>,
}

statik!(static DEFAULT: HelperThread = HelperThread::new());

pub fn default_core() -> io::Result<Core> {
    match DEFAULT.with(|h| h.core.clone()) {
        Some(Some(inner)) => Ok(Core { inner }),
        Some(None) => Err(io::Error::new(io::ErrorKind::Other, "global core failed to be created")),
        None => Err(io::Error::new(io::ErrorKind::Other, "global core has been shut down")),
    }
}

#[allow(dead_code)]
pub fn shutdown_global() {
    DEFAULT.drop();
}

impl HelperThread {
    fn new() -> HelperThread {
        let core = match Core::new() {
            Ok(core) => core,
            Err(_) => return HelperThread {
                thread: None,
                tx: None,
                core: None,
            },
        };
        let (tx, rx) = oneshot::channel();
        let inner = core.inner.clone();
        let thread = thread::spawn(move || drop(core.run(rx)));
        HelperThread {
            thread: Some(thread),
            tx: Some(tx),
            core: Some(inner),
        }
    }
}

impl Drop for HelperThread {
    fn drop(&mut self) {
        drop(self.tx.take());
        drop(self.thread.take().unwrap().join());
    }
}
