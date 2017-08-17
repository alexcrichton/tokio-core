extern crate env_logger;
extern crate futures;
extern crate tokio_core;

use std::time::{Instant, Duration};

use futures::Future;
use tokio_core::reactor::Timeout;

macro_rules! t {
    ($e:expr) => (match $e {
        Ok(e) => e,
        Err(e) => panic!("{} failed with {:?}", stringify!($e), e),
    })
}

#[test]
fn smoke() {
    drop(env_logger::init());
    let dur = Duration::from_millis(10);
    let start = Instant::now();
    let timeout = Timeout::new(dur);
    t!(timeout.wait());
    assert!(start.elapsed() >= (dur / 2));
}

#[test]
fn two() {
    drop(env_logger::init());

    let dur = Duration::from_millis(10);
    let timeout = Timeout::new(dur);
    t!(timeout.wait());
    let timeout = Timeout::new(dur);
    t!(timeout.wait());
}
