use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    error::Error,
    fmt::Display,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

use pin_project_lite::pin_project;

use crate::Id;

thread_local! { pub(crate) static TIMER_QUEUE: TimerQueue = const { TimerQueue::new() }; }

pub(crate) struct TimerQueue {
    current_id: Cell<Id>,
    // Each timer is identified by its expiry time and an incrementing ID, and ordered by the
    // expiry date. Technically it's possible for there to be conflicting identification when the
    // ID overflows and we register a duplicate expiry, but that should almost never happen.
    timers: RefCell<BTreeMap<(Instant, Id), Waker>>,
}

impl TimerQueue {
    const fn new() -> Self {
        Self {
            current_id: Cell::new(const { Id::new(1) }),
            timers: RefCell::new(BTreeMap::new()),
        }
    }

    /// Remove all expired timers and return the time from now to the next timer
    pub(crate) fn next_timeout(&self) -> Option<Duration> {
        let mut timers = self.timers.borrow_mut();
        loop {
            let now = Instant::now();
            match timers.first_entry() {
                Some(entry) => {
                    let expiry = entry.key().0;
                    if expiry <= now {
                        entry.remove().wake();
                    } else {
                        return Some(expiry - now);
                    }
                }
                None => return None,
            }
        }
    }

    /// Register a new timer with its waker, returning an ID
    ///
    /// Each timer is uniquely identified by the combination of its ID and expiry
    fn register(&self, expiry: Instant, waker: Waker) -> Id {
        let id = self.current_id.get();
        self.current_id.set(id.overflowing_incr());
        if self
            .timers
            .borrow_mut()
            .insert((expiry, id), waker)
            .is_some()
        {
            log::warn!(
                "{:?} Timer ID collision at ID = {}",
                std::thread::current().id(),
                id.0
            );
        }
        id
    }

    /// Modify the waker on an existing timer
    fn modify(&self, id: Id, expiry: Instant, waker: &Waker) {
        if let Some(wk) = self.timers.borrow_mut().get_mut(&(expiry, id)) {
            wk.clone_from(waker)
        } else {
            log::error!(
                "{:?} Modifying non-existent timer ID = {}",
                std::thread::current().id(),
                id.0
            );
        }
    }

    /// Remove a timer from the queue before it expires
    fn cancel(&self, id: Id, expiry: Instant) {
        // This timer could have expired already, in which case this becomes a noop
        self.timers.borrow_mut().remove(&(expiry, id));
    }
}

/// Async timer
#[derive(Debug)]
pub struct Timer {
    expiry: Instant,
    timer_id: Option<Id>,
    // Make the future !Send, since it relies on thread-locals
    _phantom: PhantomData<*const ()>,
}

// Future can be Sync because you can't poll timers across thread boundaries anyways, since poll()
// takes &mut self.
unsafe impl Sync for Timer {}

impl Timer {
    /// Timer that expires at a point in time
    pub fn at(expiry: Instant) -> Self {
        Timer {
            expiry,
            timer_id: None,
            _phantom: PhantomData,
        }
    }

    /// Timer that expires after a set duration
    pub fn delay(delay: Duration) -> Self {
        Self::at(Instant::now() + delay)
    }
}

impl Future for Timer {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.expiry <= Instant::now() {
            if let Some(id) = self.timer_id {
                TIMER_QUEUE.with(|q| q.cancel(id, self.expiry));
                self.timer_id = None;
            }
            return Poll::Ready(());
        }

        TIMER_QUEUE.with(|q| match self.timer_id {
            None => {
                self.timer_id = Some(q.register(self.expiry, cx.waker().clone()));
            }
            Some(id) => q.modify(id, self.expiry, cx.waker()),
        });
        Poll::Pending
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let Some(id) = self.timer_id {
            TIMER_QUEUE.with(|q| q.cancel(id, self.expiry));
        }
    }
}

#[derive(Debug)]
pub struct TimedOut(());
impl Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Future timed out")
    }
}
impl Error for TimedOut {}

pin_project! {
    #[derive(Debug)]
    pub struct Timeout<F> {
        #[pin]
        timer: Timer,
        #[pin]
        fut: F,
    }
}

impl<F: Future> Future for Timeout<F> {
    type Output = Result<F::Output, TimedOut>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Poll::Ready(out) = self.as_mut().project().fut.poll(cx) {
            return Poll::Ready(Ok(out));
        }
        if let Poll::Ready(()) = self.as_mut().project().timer.poll(cx) {
            return Poll::Ready(Err(TimedOut(())));
        }
        Poll::Pending
    }
}

/// Run the future with a timeout, cancelling it if it doesn't complete in time
pub fn timeout<F: Future>(fut: F, timeout: Duration) -> Timeout<F> {
    Timeout {
        timer: Timer::delay(timeout),
        fut,
    }
}

/// Run the future until a point in time, cancelling it if it doesn't complete in time
pub fn timeout_at<F: Future>(fut: F, expiry: Instant) -> Timeout<F> {
    Timeout {
        timer: Timer::at(expiry),
        fut,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::test::MockWaker;

    use super::*;

    #[test]
    fn next_timeout() {
        let wakers: Vec<_> = (0..3).map(|_| Arc::new(MockWaker::default())).collect();
        let tq = TimerQueue::new();
        assert!(tq.next_timeout().is_none());

        // First 2 timers should expire, but 3rd should not
        tq.register(Instant::now(), wakers[0].clone().into());
        tq.register(
            Instant::now() - Duration::from_secs(1),
            wakers[1].clone().into(),
        );
        tq.register(
            Instant::now() + Duration::from_millis(50),
            wakers[2].clone().into(),
        );
        assert!(tq.next_timeout().unwrap() > Duration::from_millis(40));

        assert!(wakers[0].get());
        assert!(wakers[1].get());
        assert!(!wakers[2].get());

        // After waiting, the 3rd timer should expire
        std::thread::sleep(Duration::from_millis(50));
        assert!(tq.next_timeout().is_none());
        assert!(wakers[2].get());

        assert!(tq.timers.into_inner().is_empty());
    }

    #[test]
    fn modify() {
        let wakers: Vec<_> = (0..2).map(|_| Arc::new(MockWaker::default())).collect();
        let tq = TimerQueue::new();

        let expiry = Instant::now() + Duration::from_millis(10);
        let id = tq.register(expiry, wakers[0].clone().into());
        assert!(tq.next_timeout().is_some());

        // Replace 1st waker with 2nd one, which should fire
        tq.modify(id, expiry, &wakers[1].clone().into());
        std::thread::sleep(Duration::from_millis(10));
        assert!(tq.next_timeout().is_none());
        assert!(!wakers[0].get());
        assert!(wakers[1].get());

        assert!(tq.timers.into_inner().is_empty());
    }

    #[test]
    fn cancel() {
        let waker = Arc::new(MockWaker::default());
        let tq = TimerQueue::new();

        let expiry = Instant::now() + Duration::from_secs(10);
        let id = tq.register(expiry, waker.clone().into());
        assert!(tq.next_timeout().is_some());

        // After cancelling timer, the waker shouldn't fire
        tq.cancel(id, expiry);
        assert!(tq.next_timeout().is_none());
        assert!(!waker.get());

        assert!(tq.timers.into_inner().is_empty());
    }

    #[test]
    fn timer_expired() {
        let waker = Arc::new(MockWaker::default());
        let mut timer = Timer::at(Instant::now());

        assert!(Pin::new(&mut timer)
            .poll(&mut Context::from_waker(&waker.into()))
            .is_ready());
        assert!(timer.timer_id.is_none());

        assert!(TIMER_QUEUE.with(|q| q.timers.borrow().is_empty()));
    }

    #[test]
    fn timer() {
        let waker = Arc::new(MockWaker::default());
        let mut timer = Timer::delay(Duration::from_millis(10));

        assert!(Pin::new(&mut timer)
            .poll(&mut Context::from_waker(&waker.clone().into()))
            .is_pending());
        assert!(timer.timer_id.is_some());
        assert_eq!(TIMER_QUEUE.with(|q| q.timers.borrow().len()), 1);

        std::thread::sleep(Duration::from_millis(10));
        assert!(Pin::new(&mut timer)
            .poll(&mut Context::from_waker(&waker.into()))
            .is_ready());
        assert!(timer.timer_id.is_none());
        assert!(TIMER_QUEUE.with(|q| q.timers.borrow().is_empty()));
    }

    #[test]
    fn timeouts() {
        let waker = Arc::new(MockWaker::default()).into();

        let res1 = Pin::new(&mut timeout(
            Timer::at(Instant::now()),
            Duration::from_secs(10),
        ))
        .poll(&mut Context::from_waker(&waker));
        assert!(matches!(res1, Poll::Ready(Ok(()))));

        let res2 = Pin::new(&mut timeout_at(
            Timer::delay(Duration::from_secs(10)),
            Instant::now(),
        ))
        .poll(&mut Context::from_waker(&waker));
        assert!(matches!(res2, Poll::Ready(Err(_))));
    }
}
