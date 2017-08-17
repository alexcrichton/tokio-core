extern crate tokio_core;
extern crate env_logger;
extern crate futures;

use std::sync::mpsc;
use std::time::Duration;

use futures::future;
use futures::sync::oneshot;
use futures::unsync::CurrentThread;
use futures::Future;
use tokio_core::reactor::Core;

#[test]
fn simple() {
    drop(env_logger::init());

    let (tx1, rx1) = oneshot::channel();
    let (tx2, rx2) = oneshot::channel();
    CurrentThread.spawn(future::lazy(|| {
        tx1.send(1).unwrap();
        Ok(())
    }));
    CurrentThread.spawn(future::lazy(|| {
        tx2.send(2).unwrap();
        Ok(())
    }));

    assert_eq!(rx1.join(rx2).wait().unwrap(), (1, 2));
}

#[test]
fn simple_core_poll() {
    drop(env_logger::init());

    let (tx, rx) = mpsc::channel();
    let (tx1, tx2) = (tx.clone(), tx.clone());
    let core = Core::new().unwrap();

    core.turn(Some(Duration::new(0, 0)));
    CurrentThread.spawn(future::lazy(move || {
        tx1.send(1).unwrap();
        Ok(())
    }));
    core.turn(Some(Duration::new(0, 0)));
    CurrentThread.spawn(future::lazy(move || {
        tx2.send(2).unwrap();
        Ok(())
    }));
    assert_eq!(rx.try_recv().unwrap(), 1);
    assert!(rx.try_recv().is_err());
    core.turn(Some(Duration::new(0, 0)));
    assert_eq!(rx.try_recv().unwrap(), 2);
}

#[test]
fn spawn_in_poll() {
    drop(env_logger::init());

    let (tx1, rx1) = oneshot::channel();
    let (tx2, rx2) = oneshot::channel();
    CurrentThread.spawn(future::lazy(move || {
        tx1.send(1).unwrap();
        CurrentThread.spawn(future::lazy(|| {
            tx2.send(2).unwrap();
            Ok(())
        }));
        Ok(())
    }));

    assert_eq!(rx1.join(rx2).wait().unwrap(), (1, 2));
}
