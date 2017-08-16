//! The core reactor driving all I/O
//!
//! This module contains the `Core` type which is the reactor for all I/O
//! happening in `tokio-core`. This reactor (or event loop) is used to run
//! futures, schedule tasks, issue I/O requests, etc.

use std::cell::RefCell;
use std::cmp;
use std::fmt;
use std::io::{self, ErrorKind};
use std::mem;
use std::rc::Rc;
use std::sync::{Arc, Weak as WeakArc};
use std::sync::atomic::{AtomicUsize, ATOMIC_USIZE_INIT, Ordering};
use std::time::{Instant, Duration};

use futures::{Future, Async};
use futures::executor::{self, Spawn, Notify};
use futures::sync::mpsc;
use futures::task::Task;
use mio;
use mio::event::Evented;

use atomic_slab::AtomicSlab;
use heap::{Heap, Slot};
use self::cell::{CoreCell, CoreProof};
use self::weak_notify::WeakHandle;

mod cell;
mod io_token;
mod timeout_token;
mod weak_notify;

mod poll_evented;
mod timeout;
mod interval;
pub use self::poll_evented::PollEvented;
pub use self::timeout::Timeout;
pub use self::interval::Interval;

static NEXT_LOOP_ID: AtomicUsize = ATOMIC_USIZE_INIT;
scoped_thread_local!(static CURRENT_LOOP: Core);

/// An event loop.
///
/// The event loop is the main source of blocking in an application which drives
/// all other I/O events and notifications happening. Each event loop can have
/// multiple handles pointing to it, each of which can then be used to create
/// various I/O objects to interact with the event loop in interesting ways.
// TODO: expand this
pub struct Core {
    events: mio::Events,
    io: Arc<CoreIo>,
    inner: Rc<RefCell<Inner>>,
}

struct Inner {
    io: Arc<CoreIo>,

    // Timer wheel keeping track of all timeouts. The `usize` stored in the
    // timer wheel is an index into the slab below.
    //
    // The slab below keeps track of the timeouts themselves as well as the
    // state of the timeout itself. The `TimeoutToken` type is an index into the
    // `timeouts` slab.
    timer_heap: Heap<(Instant, usize)>,

    proof_i_have_the_lock: CoreProof,
}

struct CoreIo {
    id: usize,

    // Actual event loop itself, aka an "epoll set"
    io: mio::Poll,

    // An mpsc channel to send messages to the event loop
    tx: mpsc::UnboundedSender<Message>,
    rx: CoreCell<Spawn<mpsc::UnboundedReceiver<Message>>>,

    // All known I/O objects and the tasks that are blocked on them. This slab
    // gives each scheduled I/O an index which is used to refer to it elsewhere.
    io_dispatch: AtomicSlab<ScheduledIo>,

    timeouts: AtomicSlab<ScheduledTimer>,

    // Used for determining when the future passed to `run` is ready. Once the
    // registration is passed to `io` above we never touch it again, just keep
    // it alive.
    future_readiness: mio::SetReadiness,
    _future_registration: mio::Registration,

    // Used to keep track of when there are messages ready to be received by the
    // event loop.
    rx_readiness: mio::SetReadiness,
    _rx_registration: mio::Registration,
}

fn _assert() {
    fn _assert<T: Send + Sync>() {}
    _assert::<CoreIo>();
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
    id: usize,
    core: WeakArc<CoreIo>,
}

struct ScheduledIo {
    readiness: Arc<AtomicUsize>,
    inner: CoreCell<ScheduledIoInner>,
}

// TODO: comment this
unsafe impl Send for ScheduledIo {}
unsafe impl Sync for ScheduledIo {}

struct ScheduledIoInner {
    reader: Option<Task>,
    writer: Option<Task>,
}

struct ScheduledTimer {
    heap_slot: CoreCell<Option<Slot>>,
    state: CoreCell<TimeoutState>,
}

// TODO: comment this
unsafe impl Send for ScheduledTimer {}
unsafe impl Sync for ScheduledTimer {}

enum TimeoutState {
    NotFired,
    Fired,
    Waiting(Task),
}

enum Direction {
    Read,
    Write,
}

enum Message {
    DropSource(usize),
    Schedule(usize, Task, Direction),
    UpdateTimeout(usize, Task),
    ResetTimeout(usize, Instant),
    CancelTimeout(usize),
}

const TOKEN_MESSAGES: mio::Token = mio::Token(0);
const TOKEN_FUTURE: mio::Token = mio::Token(1);
const TOKEN_START: usize = 2;

impl Core {
    /// Creates a new event loop, returning any error that happened during the
    /// creation.
    pub fn new() -> io::Result<Core> {
        let io = try!(mio::Poll::new());
        let future_pair = mio::Registration::new2();
        try!(io.register(&future_pair.0,
                         TOKEN_FUTURE,
                         mio::Ready::readable(),
                         mio::PollOpt::level()));
        let (tx, rx) = mpsc::unbounded();
        let channel_pair = mio::Registration::new2();
        try!(io.register(&channel_pair.0,
                         TOKEN_MESSAGES,
                         mio::Ready::readable(),
                         mio::PollOpt::level()));

        let io = Arc::new(CoreIo {
            id: NEXT_LOOP_ID.fetch_add(1, Ordering::Relaxed),
            tx: tx,
            rx: CoreCell::new(executor::spawn(rx)),
            io: io,
            io_dispatch: AtomicSlab::new(),
            timeouts: AtomicSlab::new(),

            future_readiness: future_pair.1,
            _future_registration: future_pair.0,
            rx_readiness: channel_pair.1,
            _rx_registration: channel_pair.0,
        });
        io.rx_readiness.set_readiness(mio::Ready::readable()).unwrap();

        Ok(Core {
            events: mio::Events::with_capacity(1024),
            io: io.clone(),
            inner: Rc::new(RefCell::new(Inner {
                timer_heap: Heap::new(),
                io: io,
                proof_i_have_the_lock: unsafe { CoreProof::new() },
            })),
        })
    }

    /// Generates a remote handle to this event loop which can be used to spawn
    /// tasks from other threads into this event loop.
    pub fn remote(&self) -> Remote {
        Remote {
            id: self.io.id,
            core: Arc::downgrade(&self.io),
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
            if future_fired {
                let res = try!(CURRENT_LOOP.set(self, || {
                    task.poll_future_notify(&WeakHandle(&self.io), 0)
                }));
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

    fn poll(&mut self, max_wait: Option<Duration>) -> bool {
        // Given the `max_wait` variable specified, figure out the actual
        // timeout that we're going to pass to `poll`. This involves taking a
        // look at active timers on our heap as well.
        let start = Instant::now();
        let timeout = self.inner.borrow_mut().timer_heap.peek().map(|t| {
            if t.0 < start {
                Duration::new(0, 0)
            } else {
                t.0 - start
            }
        });
        let timeout = match (max_wait, timeout) {
            (Some(d1), Some(d2)) => Some(cmp::min(d1, d2)),
            (max_wait, timeout) => max_wait.or(timeout),
        };

        // Block waiting for an event to happen, peeling out how many events
        // happened.
        let amt = match self.io.io.poll(&mut self.events, timeout) {
            Ok(a) => a,
            Err(ref e) if e.kind() == ErrorKind::Interrupted => return false,
            Err(e) => panic!("error in poll: {}", e),
        };

        let after_poll = Instant::now();
        debug!("loop poll - {:?}", after_poll - start);
        debug!("loop time - {:?}", after_poll);

        // Process all timeouts that may have just occurred, updating the
        // current time since
        self.consume_timeouts(after_poll);

        // Process all the events that came in, dispatching appropriately
        let mut fired = false;
        for i in 0..self.events.len() {
            let event = self.events.get(i).unwrap();
            let token = event.token();
            trace!("event {:?} {:?}", event.readiness(), event.token());

            if token == TOKEN_MESSAGES {
                self.io.rx_readiness.set_readiness(mio::Ready::empty()).unwrap();
                CURRENT_LOOP.set(&self, || self.consume_queue());
            } else if token == TOKEN_FUTURE {
                self.io.future_readiness.set_readiness(mio::Ready::empty()).unwrap();
                fired = true;
            } else {
                self.dispatch(token, event.readiness());
            }
        }
        debug!("loop process - {} events, {:?}", amt, after_poll.elapsed());
        return fired
    }

    fn dispatch(&mut self, token: mio::Token, ready: mio::Ready) {
        let token = usize::from(token) - TOKEN_START;
        let mut reader = None;
        let mut writer = None;
        {
            let mut inner = self.inner.borrow_mut();
            let inner = &mut *inner;
            if let Some(io) = inner.io.io_dispatch.get(token) {
                let proof = &mut inner.proof_i_have_the_lock;
                io.readiness.fetch_or(ready2usize(ready), Ordering::Relaxed);
                if ready.is_writable() {
                    writer = io.inner.get_mut(proof).writer.take();
                }
                if !(ready & (!mio::Ready::writable())).is_empty() {
                    reader = io.inner.get_mut(proof).reader.take();
                }
            }
        }
        // TODO: don't notify the same task twice
        if let Some(reader) = reader {
            self.notify_handle(reader);
        }
        if let Some(writer) = writer {
            self.notify_handle(writer);
        }
    }

    fn consume_timeouts(&mut self, now: Instant) {
        loop {
            let handle = {
                let mut inner = self.inner.borrow_mut();
                let inner = &mut *inner;
                let proof = &mut inner.proof_i_have_the_lock;
                match inner.timer_heap.peek() {
                    Some(head) if head.0 <= now => {}
                    Some(_) => break,
                    None => break,
                };
                let (_, slab_idx) = inner.timer_heap.pop().unwrap();

                trace!("firing timeout: {}", slab_idx);
                // TODO: think about whether this can panic
                let timeout = inner.io.timeouts.get(slab_idx).expect("slab index missing");
                timeout.heap_slot.get_mut(proof).take().unwrap();
                timeout.state.get_mut(proof).fire()
            };
            if let Some(handle) = handle {
                self.notify_handle(handle);
            }
        }
    }

    /// Method used to notify a task handle.
    ///
    /// Note that this should be used instead of `handle.notify()` to ensure
    /// that the `CURRENT_LOOP` variable is set appropriately.
    fn notify_handle(&self, handle: Task) {
        debug!("notifying a task handle");
        CURRENT_LOOP.set(&self, || handle.notify());
    }

    fn consume_queue(&self) {
        debug!("consuming notification queue");
        loop {
            let msg = {
                let mut inner = self.inner.borrow_mut();
                let proof = &mut inner.proof_i_have_the_lock;
                let rx = self.io.rx.get_mut(proof);
                // TODO: can we do better than `.unwrap()` here?
                rx.poll_stream_notify(&WeakHandle(&self.io), 1).unwrap()
            };
            match msg {
                Async::Ready(Some(msg)) => self.notify(msg),
                Async::NotReady |
                Async::Ready(None) => break,
            }
        }
    }

    fn notify(&self, msg: Message) {
        match msg {
            Message::DropSource(tok) => self.inner.borrow_mut().drop_source(tok),
            Message::Schedule(tok, wake, dir) => {
                let task = self.inner.borrow_mut().schedule(tok, wake, dir);
                if let Some(task) = task {
                    self.notify_handle(task);
                }
            }
            Message::UpdateTimeout(t, handle) => {
                let task = self.inner.borrow_mut().update_timeout(t, handle);
                if let Some(task) = task {
                    self.notify_handle(task);
                }
            }
            Message::ResetTimeout(t, at) => {
                self.inner.borrow_mut().reset_timeout(t, at);
            }
            Message::CancelTimeout(t) => {
                self.inner.borrow_mut().cancel_timeout(t)
            }
        }
    }

    /// Get the ID of this loop
    pub fn id(&self) -> CoreId {
        CoreId(self.io.id)
    }
}

impl fmt::Debug for Core {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Core")
         .field("id", &self.id())
         .finish()
    }
}

impl Inner {
    fn drop_source(&mut self, token: usize) {
        debug!("dropping I/O source: {}", token);
        // TODO: why unsafe comment
        unsafe {
            self.io.io_dispatch.remove(token);
        }
    }

    fn schedule(&mut self, token: usize, wake: Task, dir: Direction)
                -> Option<Task> {
        debug!("scheduling direction for: {}", token);
        let sched = self.io.io_dispatch.get(token).unwrap();
        let proof = &mut self.proof_i_have_the_lock;
        let slots = sched.inner.get_mut(proof);
        let (slot, ready) = match dir {
            Direction::Read => (&mut slots.reader, !mio::Ready::writable()),
            Direction::Write => (&mut slots.writer, mio::Ready::writable()),
        };
        if sched.readiness.load(Ordering::SeqCst) & ready2usize(ready) != 0 {
            debug!("cancelling block");
            *slot = None;
            Some(wake)
        } else {
            debug!("blocking");
            *slot = Some(wake);
            None
        }
    }

    fn update_timeout(&mut self, token: usize, handle: Task) -> Option<Task> {
        debug!("updating a timeout: {}", token);
        // TODO: think about whether this can panic
        let proof = &mut self.proof_i_have_the_lock;
        let timeout = self.io.timeouts.get(token).expect("timeout missing");
        timeout.state.get_mut(proof).block(handle)
    }

    fn reset_timeout(&mut self, token: usize, at: Instant) {
        let proof = &mut self.proof_i_have_the_lock;
        // TODO: think about whether this can panic
        let state = self.io.timeouts.get(token).expect("timeout token missing");

        // TODO: avoid remove + push and instead just do one sift of the heap?
        // In theory we could update it in place and then do the percolation
        // as necessary
        if let Some(slot) = state.heap_slot.get_mut(proof).take() {
            self.timer_heap.remove(slot);
        }
        let slot = self.timer_heap.push((at, token));
        *state.heap_slot.get_mut(proof) = Some(slot);
        *state.state.get_mut(proof) = TimeoutState::NotFired;
        debug!("set a timeout: {}", token);
    }

    fn cancel_timeout(&mut self, token: usize) {
        debug!("cancel a timeout: {}", token);
        // TODO: comment unsafe
        //
        // TODO: can we cancel a timeout twice by accident?
        unsafe {
            let ScheduledTimer { heap_slot, state } = self.io.timeouts.remove(token);
            drop(state);
            if let Some(slot) = heap_slot.into_inner() {
                self.timer_heap.remove(slot);
            }
        }
    }
}

impl Remote {
    fn send(&self, msg: Message) {
        self.with_loop(|lp| {
            match lp {
                Some(lp) => {
                    // Need to execute all existing requests first, to ensure
                    // that our message is processed "in order"
                    lp.consume_queue();
                    lp.notify(msg);
                }
                None => {
                    let err = match self.core.upgrade() {
                        Some(io) => (&io.tx).send(msg).is_err(),
                        None => true,
                    };
                    // TODO: this error should punt upwards and we should
                    //       notify the caller that the message wasn't
                    //       received. This is tokio-core#17
                    drop(err);
                }
            }
        })
    }

    fn with_loop<F, R>(&self, f: F) -> R
        where F: FnOnce(Option<&Core>) -> R
    {
        if CURRENT_LOOP.is_set() {
            CURRENT_LOOP.with(|lp| {
                let same = lp.io.id == self.id;
                if same {
                    f(Some(lp))
                } else {
                    f(None)
                }
            })
        } else {
            f(None)
        }
    }

    /// Return the ID of the represented Core
    pub fn id(&self) -> CoreId {
        CoreId(self.id)
    }
}

impl CoreIo {
    fn add_source(&self, source: &Evented)
                  -> io::Result<(Arc<AtomicUsize>, usize)> {
        debug!("adding a new I/O source");
        let sched = ScheduledIo {
            readiness: Arc::new(AtomicUsize::new(0)),
            inner: CoreCell::new(ScheduledIoInner {
                reader: None,
                writer: None,
            }),
        };
        let ret = sched.readiness.clone();
        let idx = self.io_dispatch.insert(sched);
        try!(self.io.register(source,
                              mio::Token(TOKEN_START + idx),
                              mio::Ready::readable() |
                                 mio::Ready::writable() |
                                 platform::all(),
                              mio::PollOpt::edge()));
        Ok((ret, idx))
    }

    fn add_timeout(&self) -> usize {
        self.timeouts.insert(ScheduledTimer {
            heap_slot: CoreCell::new(None),
            state: CoreCell::new(TimeoutState::NotFired),
        })
    }
}

impl fmt::Debug for Remote {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Remote")
         .field("id", &self.id())
         .finish()
    }
}

impl TimeoutState {
    fn block(&mut self, handle: Task) -> Option<Task> {
        match *self {
            TimeoutState::Fired => return Some(handle),
            _ => {}
        }
        *self = TimeoutState::Waiting(handle);
        None
    }

    fn fire(&mut self) -> Option<Task> {
        match mem::replace(self, TimeoutState::Fired) {
            TimeoutState::NotFired => None,
            TimeoutState::Fired => panic!("fired twice?"),
            TimeoutState::Waiting(handle) => Some(handle),
        }
    }
}

impl Notify for CoreIo {
    fn notify(&self, id: usize) {
        let readiness = if id == 0 {
            &self.future_readiness
        } else {
            &self.rx_readiness
        };
        readiness.set_readiness(mio::Ready::readable())
            .expect("failed to set readiness");
    }
}

trait FnBox: Send + 'static {
    fn call_box(self: Box<Self>, lp: &Core);
}

impl<F: FnOnce(&Core) + Send + 'static> FnBox for F {
    fn call_box(self: Box<Self>, lp: &Core) {
        (*self)(lp)
    }
}

fn read_ready() -> mio::Ready {
    mio::Ready::readable() | platform::hup()
}

const READ: usize = 1 << 0;
const WRITE: usize = 1 << 1;

fn ready2usize(ready: mio::Ready) -> usize {
    let mut bits = 0;
    if ready.is_readable() {
        bits |= READ;
    }
    if ready.is_writable() {
        bits |= WRITE;
    }
    bits | platform::ready2usize(ready)
}

fn usize2ready(bits: usize) -> mio::Ready {
    let mut ready = mio::Ready::empty();
    if bits & READ != 0 {
        ready.insert(mio::Ready::readable());
    }
    if bits & WRITE != 0 {
        ready.insert(mio::Ready::writable());
    }
    ready | platform::usize2ready(bits)
}

#[cfg(all(unix, not(target_os = "fuchsia")))]
mod platform {
    use mio::Ready;
    use mio::unix::UnixReady;

    pub fn aio() -> Ready {
        UnixReady::aio().into()
    }

    pub fn all() -> Ready {
        hup() | aio()
    }

    pub fn hup() -> Ready {
        UnixReady::hup().into()
    }

    const HUP: usize = 1 << 2;
    const ERROR: usize = 1 << 3;
    const AIO: usize = 1 << 4;

    pub fn ready2usize(ready: Ready) -> usize {
        let ready = UnixReady::from(ready);
        let mut bits = 0;
        if ready.is_aio() {
            bits |= AIO;
        }
        if ready.is_error() {
            bits |= ERROR;
        }
        if ready.is_hup() {
            bits |= HUP;
        }
        bits
    }

    pub fn usize2ready(bits: usize) -> Ready {
        let mut ready = UnixReady::from(Ready::empty());
        if bits & AIO != 0 {
            ready.insert(UnixReady::aio());
        }
        if bits & HUP != 0 {
            ready.insert(UnixReady::hup());
        }
        if bits & ERROR != 0 {
            ready.insert(UnixReady::error());
        }
        ready.into()
    }
}

#[cfg(any(windows, target_os = "fuchsia"))]
mod platform {
    use mio::Ready;

    pub fn all() -> Ready {
        // No platform-specific Readinesses for Windows
        Ready::empty()
    }

    pub fn hup() -> Ready {
        Ready::empty()
    }

    pub fn ready2usize(_r: Ready) -> usize {
        0
    }

    pub fn usize2ready(_r: usize) -> Ready {
        Ready::empty()
    }
}
