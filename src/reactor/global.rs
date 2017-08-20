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

pub(super) fn inner() -> Option<Arc<Inner>> {
    DEFAULT.with(|h| h.core.clone()).and_then(|x| x)
}

#[allow(dead_code)]
pub fn shutdown_global() {
    DEFAULT.drop();
}

impl HelperThread {
    fn new() -> HelperThread {
        let mut core = match Core::new_id(0) {
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
