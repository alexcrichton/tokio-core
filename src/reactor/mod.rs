//! The core reactor driving all I/O
//!
//! This module contains the `Core` type which is the reactor for all I/O
//! happening in `tokio-core`. This reactor (or event loop) is used to run
//! futures, schedule tasks, issue I/O requests, etc.

use std::fmt;
use std::cmp;
use std::io;
use std::sync::atomic::{AtomicUsize, ATOMIC_USIZE_INIT, Ordering};
use std::time::{Duration, Instant};

use futures::current_thread::{Spawner, TaskRunner};
use futures::executor::{self, Spawn};
use futures::future::{self, Executor, ExecuteError};
use futures::prelude::*;
use futures::sync::mpsc;
use futures_timer::{Timer, TimerHandle};
use tokio::reactor::{Reactor, Handle as TokioHandle};

static NEXT_LOOP_ID: AtomicUsize = ATOMIC_USIZE_INIT;

mod interval;
mod poll_evented;
mod timeout;
pub use self::interval::Interval;
pub use self::poll_evented::PollEvented;
pub use self::timeout::Timeout;

/// An event loop.
///
/// The event loop is the main source of blocking in an application which drives
/// all other I/O events and notifications happening. Each event loop can have
/// multiple handles pointing to it, each of which can then be used to create
/// various I/O objects to interact with the event loop in interesting ways.
pub struct Core {
    reactor: Reactor,
    tasks: Spawn<TaskRunner>,
    timer: Spawn<Timer>,
    id: usize,
    tx: mpsc::UnboundedSender<Box<FnBox>>,
}

/// An unique ID for a Core
///
/// An ID by which different cores may be distinguished. Can be compared and used as an index in
/// a `HashMap`.
///
/// The ID is globally unique and never reused.
#[derive(Clone,Copy,Eq,PartialEq,Hash,Debug)]
pub struct CoreId(usize);

/// Handle to an event loop, used to construct I/O objects, send messages, and
/// otherwise interact indirectly with the event loop itself.
///
/// Handles can be cloned, and when cloned they will still refer to the
/// same underlying event loop.
#[derive(Clone)]
pub struct Remote {
    handle: TokioHandle,
    tx: mpsc::UnboundedSender<Box<FnBox>>,
    id: usize,
}

/// A non-sendable handle to an event loop, useful for manufacturing instances
/// of `LoopData`.
#[derive(Clone)]
pub struct Handle {
    remote: Remote,
    tasks: Spawner,
    timer: TimerHandle,
}

impl Core {
    /// Creates a new event loop, returning any error that happened during the
    /// creation.
    pub fn new() -> io::Result<Core> {
        let (tx, rx) = mpsc::unbounded();
        let core = Core {
            reactor: Reactor::new()?,
            tasks: executor::spawn(TaskRunner::new()),
            timer: executor::spawn(Timer::new()),
            id: NEXT_LOOP_ID.fetch_add(1, Ordering::Relaxed),
            tx: tx,
        };
        let handle = core.handle();
        let spawner = core.tasks.get_ref().spawner();
        core.tasks.get_ref().spawner().spawn(rx.for_each(move |f| {
            spawner.spawn(f.call_box(&handle));
            Ok(())
        }));

        Ok(core)
    }

    /// Returns a handle to this event loop which cannot be sent across threads
    /// but can be used as a proxy to the event loop itself.
    ///
    /// Handles are cloneable and clones always refer to the same event loop.
    /// This handle is typically passed into functions that create I/O objects
    /// to bind them to this event loop.
    pub fn handle(&self) -> Handle {
        Handle {
            remote: self.remote(),
            tasks: self.tasks.get_ref().spawner(),
            timer: self.timer.get_ref().handle(),
        }
    }

    /// Generates a remote handle to this event loop which can be used to spawn
    /// tasks from other threads into this event loop.
    pub fn remote(&self) -> Remote {
        Remote {
            handle: self.reactor.handle().clone(),
            tx: self.tx.clone(),
            id: self.id,
        }
    }

    /// Runs a future until completion, driving the event loop while we're
    /// otherwise waiting for the future to complete.
    ///
    /// This function will begin executing the event loop and will finish once
    /// the provided future is resolved. Note that the future argument here
    /// crucially does not require the `'static` nor `Send` bounds. As a result
    /// the future will be "pinned" to not only this thread but also this stack
    /// frame.
    ///
    /// This function will return the value that the future resolves to once
    /// the future has finished. If the future never resolves then this function
    /// will never return.
    ///
    /// # Panics
    ///
    /// This method will **not** catch panics from polling the future `f`. If
    /// the future panics then it's the responsibility of the caller to catch
    /// that panic and handle it as appropriate.
    pub fn run<F>(&mut self, f: F) -> Result<F::Item, F::Error>
        where F: Future,
    {
        let mut task = executor::spawn(f);
        let mut future_fired = true;

        loop {
            if future_fired || true {
                let res = task.poll_future_notify(&&self.reactor, 0)?;
                if let Async::Ready(e) = res {
                    return Ok(e)
                }
            }
            future_fired = self.poll(None);
        }
    }

    /// Performs one iteration of the event loop, blocking on waiting for events
    /// for at most `max_wait` (forever if `None`).
    ///
    /// It only makes sense to call this method if you've previously spawned
    /// a future onto this event loop.
    ///
    /// `loop { lp.turn(None) }` is equivalent to calling `run` with an
    /// empty future (one that never finishes).
    pub fn turn(&mut self, max_wait: Option<Duration>) {
        self.poll(max_wait);
    }

    /// Get the ID of this loop
    pub fn id(&self) -> CoreId {
        CoreId(self.id)
    }

    fn poll(&mut self, max_wait: Option<Duration>) -> bool {
        drop(self.timer.poll_future_notify(&&self.reactor, 0));
        drop(self.tasks.poll_future_notify(&&self.reactor, 0));
        self.timer.get_mut().advance();
        let max_wait = match self.timer.get_ref().next_event() {
            Some(at) => {
                let now = Instant::now();
                let dur = if at < now {
                    Duration::new(0, 0)
                } else {
                    at - now
                };
                match max_wait {
                    Some(a) => Some(cmp::min(a, dur)),
                    None => Some(dur),
                }
            }
            None => max_wait,
        };
        self.reactor.turn(max_wait).last_notify_id().is_some()
    }
}

impl<F> Executor<F> for Core
    where F: Future<Item = (), Error = ()> + 'static,
{
    fn execute(&self, future: F) -> Result<(), ExecuteError<F>> {
        self.handle().execute(future)
    }
}

impl fmt::Debug for Core {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Core")
         .field("id", &self.id())
         .finish()
    }
}


impl Remote {
    /// Spawns a new future into the event loop this remote is associated with.
    ///
    /// This function takes a closure which is executed within the context of
    /// the I/O loop itself. The future returned by the closure will be
    /// scheduled on the event loop and run to completion.
    ///
    /// Note that while the closure, `F`, requires the `Send` bound as it might
    /// cross threads, the future `R` does not.
    ///
    /// # Panics
    ///
    /// This method will **not** catch panics from polling the future `f`. If
    /// the future panics then it's the responsibility of the caller to catch
    /// that panic and handle it as appropriate.
    pub fn spawn<F, R>(&self, f: F)
        where F: FnOnce(&Handle) -> R + Send + 'static,
              R: IntoFuture<Item=(), Error=()>,
              R::Future: 'static,
    {
        drop(self.tx.unbounded_send(Box::new(|handle: &Handle| {
            let f = f(handle);
            Box::new(f.into_future()) as Box<Future<Item = _, Error = _>>
        })));
    }

    /// Return the ID of the represented Core
    pub fn id(&self) -> CoreId {
        CoreId(self.id)
    }

    /// Attempts to "promote" this remote to a handle, if possible.
    ///
    /// This function is intended for structures which typically work through a
    /// `Remote` but want to optimize runtime when the remote doesn't actually
    /// leave the thread of the original reactor. This will attempt to return a
    /// handle if the `Remote` is on the same thread as the event loop and the
    /// event loop is running.
    ///
    /// If this `Remote` has moved to a different thread or if the event loop is
    /// running, then `None` may be returned. If you need to guarantee access to
    /// a `Handle`, then you can call this function and fall back to using
    /// `spawn` above if it returns `None`.
    pub fn handle(&self) -> Option<Handle> {
        None
    }

    fn tokio(&self) -> &TokioHandle {
        &self.handle
    }
}

impl<F> Executor<F> for Remote
    where F: Future<Item = (), Error = ()> + Send + 'static,
{
    fn execute(&self, future: F) -> Result<(), ExecuteError<F>> {
        self.spawn(|_| future);
        Ok(())
    }
}

impl fmt::Debug for Remote {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Remote")
         .field("id", &self.id())
         .finish()
    }
}

impl Handle {
    /// Returns a reference to the underlying remote handle to the event loop.
    pub fn remote(&self) -> &Remote {
        &self.remote
    }

    fn tokio(&self) -> &TokioHandle {
        self.remote.tokio()
    }

    fn timer(&self) -> TimerHandle {
        self.timer.clone()
    }

    /// Spawns a new future on the event loop this handle is associated with.
    ///
    /// # Panics
    ///
    /// This method will **not** catch panics from polling the future `f`. If
    /// the future panics then it's the responsibility of the caller to catch
    /// that panic and handle it as appropriate.
    pub fn spawn<F>(&self, f: F)
        where F: Future<Item=(), Error=()> + 'static,
    {
        self.tasks.spawn(f);
    }

    /// Spawns a closure on this event loop.
    ///
    /// This function is a convenience wrapper around the `spawn` function above
    /// for running a closure wrapped in `futures::lazy`. It will spawn the
    /// function `f` provided onto the event loop, and continue to run the
    /// future returned by `f` on the event loop as well.
    ///
    /// # Panics
    ///
    /// This method will **not** catch panics from polling the future `f`. If
    /// the future panics then it's the responsibility of the caller to catch
    /// that panic and handle it as appropriate.
    pub fn spawn_fn<F, R>(&self, f: F)
        where F: FnOnce() -> R + 'static,
              R: IntoFuture<Item=(), Error=()> + 'static,
    {
        self.spawn(future::lazy(f))
    }

    /// Return the ID of the represented Core
    pub fn id(&self) -> CoreId {
        self.remote.id()
    }
}

impl<F> Executor<F> for Handle
    where F: Future<Item = (), Error = ()> + 'static,
{
    fn execute(&self, future: F) -> Result<(), ExecuteError<F>> {
        self.spawn(future);
        Ok(())
    }
}

impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Handle")
         .field("id", &self.id())
         .finish()
    }
}

trait FnBox: Send + 'static {
    fn call_box(self: Box<Self>, handle: &Handle)
        -> Box<Future<Item = (), Error = ()>>;
}

impl<F> FnBox for F
    where F: FnOnce(&Handle) -> Box<Future<Item = (), Error = ()>> + Send + 'static,
{
    fn call_box(self: Box<Self>, handle: &Handle)
        -> Box<Future<Item = (), Error = ()>>
    {
        (*self)(handle)
    }
}
