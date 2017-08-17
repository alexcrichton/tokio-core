extern crate env_logger;
extern crate futures;
extern crate tokio_core;

use std::time::{Instant, Duration};

use futures::{Future, Stream};
use tokio_core::reactor::Interval;

macro_rules! t {
    ($e:expr) => (match $e {
        Ok(e) => e,
        Err(e) => panic!("{} failed with {:?}", stringify!($e), e),
    })
}

#[test]
fn single() {
    drop(env_logger::init());
    let dur = Duration::from_millis(10);
    let start = Instant::now();
    let interval = Interval::new(dur);
    t!(interval.take(1).collect().wait());
    assert!(start.elapsed() >= dur);
}

#[test]
fn two_times() {
    drop(env_logger::init());
    let dur = Duration::from_millis(10);
    let start = Instant::now();
    let interval = Interval::new(dur);
    let result = t!(interval.take(2).collect().wait());
    assert_eq!(result, vec![(), ()]);
    let time = start.elapsed();
    assert!(time >= dur * 2,
            "\ntime:  {:?}\ndur*2: {:?}",
            time,
            dur * 2);
}
