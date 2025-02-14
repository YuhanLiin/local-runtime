use std::{
    cell::RefCell,
    io,
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
    task::Waker,
    time::Duration,
};

use rustix::{
    event::{eventfd, poll, EventfdFlags, PollFd, PollFlags},
    pipe::{pipe_with, PipeFlags},
    time::{
        timerfd_create, timerfd_settime, Itimerspec, TimerfdClockId, TimerfdFlags,
        TimerfdTimerFlags, Timespec,
    },
};

use super::{Interest, Notifier, Reactor};

impl From<Interest> for PollFlags {
    fn from(val: Interest) -> Self {
        let mut flags = PollFlags::empty();
        if val.read {
            flags |= PollFlags::IN | PollFlags::HUP | PollFlags::ERR | PollFlags::PRI;
        }
        if val.write {
            flags |= PollFlags::OUT | PollFlags::HUP | PollFlags::ERR;
        }
        flags
    }
}

/// Reactor that uses `poll()` to wait for events, making it compatible on all Unix platforms.
pub struct PollReactor<N: NotifierFd, T: Timeout> {
    notifier: Arc<FlagNotifier<N>>,
    timeout: T,
    inner: RefCell<Inner>,
}

// The part of reactor that requires interior mutability
#[derive(Default)]
struct Inner {
    // All the pollfds will be constructed from raw fds, so don't worry about lifetimes
    pollfds: Vec<PollFd<'static>>,
    wakers: Vec<Waker>,
}

impl<N: NotifierFd + 'static, T: Timeout> Reactor for PollReactor<N, T> {
    type Notifier = FlagNotifier<N>;

    fn new() -> io::Result<Self> {
        let notifier = Arc::new(FlagNotifier::new(NotifierFd::new()?));
        let timeout = Timeout::new()?;
        let inner = Inner::default();
        Ok(Self {
            notifier,
            timeout,
            inner: RefCell::new(inner),
        })
    }

    unsafe fn register<S: AsRawFd>(&self, source: &S, interest: Interest, waker: Waker) {
        let mut inner = self.inner.borrow_mut();
        let fd = BorrowedFd::borrow_raw(source.as_raw_fd());
        inner
            .pollfds
            .push(PollFd::from_borrowed_fd(fd, interest.into()));
        inner.wakers.push(waker);
    }

    fn wait(&self, timeout: Option<Duration>) -> io::Result<()> {
        // Drop guard to ensure the pollfds and wakers are always cleared
        struct InnerGuard<'a>(&'a mut Inner);
        impl Drop for InnerGuard<'_> {
            fn drop(&mut self) {
                self.0.pollfds.clear();
                self.0.wakers.clear();
            }
        }

        let mut borrow = self.inner.borrow_mut();
        let inner = InnerGuard(&mut borrow);

        let timeout = self.timeout.set_timeout(timeout)?;
        // SAFETY: pollfds will be cleared by the end of the call
        unsafe {
            self.notifier.inner.register(&mut inner.0.pollfds);
            self.timeout.register(&mut inner.0.pollfds);
        }

        match poll(&mut inner.0.pollfds, timeout)? {
            // If poll timed out, don't bother checking the wakers
            0 => {}
            // If the only events received are the ones without a waker, then skip the waker check
            n @ 1 | n @ 2
                if inner.0.pollfds[inner.0.wakers.len()..]
                    .iter()
                    .filter(|pfd| !pfd.revents().is_empty())
                    .count()
                    == n => {}

            _ => {
                // Now that we have awaken from the poll call, there's no need to send any
                // notifications to "wake up" from the poll, so we set the notified flag to prevent
                // our wakers from sending any notifications.
                self.notifier.set_to_notified();
                // For every FD that received an event, invoke its waker
                for (pollfd, waker) in inner.0.pollfds.iter().zip(&inner.0.wakers) {
                    if pollfd.revents().intersects(
                        PollFlags::IN
                            | PollFlags::OUT
                            | PollFlags::HUP
                            | PollFlags::ERR
                            | PollFlags::PRI,
                    ) {
                        waker.wake_by_ref();
                    }
                }
            }
        };

        let _ = self.notifier.clear();
        Ok(())
    }

    fn notifier(&self) -> Weak<Self::Notifier> {
        Arc::downgrade(&self.notifier)
    }
}

/// Method of notifying the reactor to wake it up
pub trait NotifierFd: 'static {
    fn new() -> io::Result<Self>
    where
        Self: Sized;
    fn clear(&self) -> io::Result<()>;
    fn notify(&self) -> io::Result<()>;
    unsafe fn register(&self, pollfds: &mut Vec<PollFd<'static>>);
}

/// Linux eventfd for notifying the reactor
#[cfg(any(target_os = "linux", target_os = "android"))]
pub struct EventFd {
    fd: OwnedFd,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl NotifierFd for EventFd {
    fn new() -> io::Result<Self> {
        let eventfd = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;
        Ok(Self { fd: eventfd })
    }

    fn clear(&self) -> io::Result<()> {
        // Sets eventfd to 0
        rustix::io::read(&self.fd, &mut [0u8; 8]).map(drop)?;
        Ok(())
    }

    fn notify(&self) -> io::Result<()> {
        // Eventfd should write all 8 bytes in a single call
        rustix::io::write(&self.fd, &1u64.to_ne_bytes()).map(drop)?;
        Ok(())
    }

    unsafe fn register(&self, pollfds: &mut Vec<PollFd<'static>>) {
        pollfds.push(PollFd::from_borrowed_fd(
            BorrowedFd::borrow_raw(self.fd.as_raw_fd()),
            PollFlags::IN,
        ));
    }
}

/// Unix pipe for notifying the reactor on non-Linux platforms
pub struct PipeFd {
    read: OwnedFd,
    write: OwnedFd,
}

impl NotifierFd for PipeFd {
    fn new() -> io::Result<Self>
    where
        Self: Sized,
    {
        let (read, write) = pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK)?;
        Ok(Self { read, write })
    }

    fn clear(&self) -> io::Result<()> {
        // Ideally we want to clear every notification, but each notification requires one byte of
        // memory to read out. Since this pipe will be wrapped in a `FlagNotifier`, there shouldn't
        // be more than one notification written at a time, so reading 8 bytes should suffice.
        rustix::io::read(&self.read, &mut [0u8; 8])?;
        Ok(())
    }

    fn notify(&self) -> io::Result<()> {
        // Write one byte to the pipe
        rustix::io::write(&self.write, &[0])?;
        Ok(())
    }

    unsafe fn register(&self, pollfds: &mut Vec<PollFd<'static>>) {
        // Register the read end of the pipe
        pollfds.push(PollFd::from_borrowed_fd(
            BorrowedFd::borrow_raw(self.read.as_raw_fd()),
            PollFlags::IN,
        ));
    }
}

/// Wraps a `NotifierFd` implementation with an atomic flag so that the notification is only sent
/// once to the FD.
pub struct FlagNotifier<N: NotifierFd> {
    inner: N,
    is_notified: AtomicBool,
}

impl<N: NotifierFd> FlagNotifier<N> {
    fn new(inner: N) -> Self {
        Self {
            inner,
            is_notified: AtomicBool::new(false),
        }
    }

    fn clear(&self) -> io::Result<()> {
        let res = self.inner.clear();
        // Release memory ordering is used to ensure the inner notifier is cleared before clearing
        // the atomic flag.
        self.is_notified.store(false, Ordering::Release);
        res
    }

    fn set_to_notified(&self) {
        self.is_notified.store(true, Ordering::Relaxed);
    }
}

impl<N: NotifierFd> Notifier for FlagNotifier<N> {
    fn notify(&self) -> io::Result<()> {
        // Use atomic flag to ensure that the inner notifier will only be called once even with
        // multiple notify() calls. Acquire memory order is used to ensure operations on the inner
        // notifier happen after checking the atomic flag.
        if self
            .is_notified
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            self.inner.notify()?;
        }
        Ok(())
    }
}

/// Method of handling timeouts on the reactor
pub trait Timeout {
    fn new() -> io::Result<Self>
    where
        Self: Sized;
    /// Return the desired poll timeout
    fn set_timeout(&self, duration: Option<Duration>) -> io::Result<i32>;
    unsafe fn register(&self, pollfds: &mut Vec<PollFd<'static>>);
}

/// Use the timeout argument of poll() to handle timers
///
/// Limited to only millisecond precision
struct PollTimeout;

impl Timeout for PollTimeout {
    fn new() -> io::Result<Self>
    where
        Self: Sized,
    {
        Ok(Self)
    }

    fn set_timeout(&self, duration: Option<Duration>) -> io::Result<i32> {
        // Round duration up to nearest millisecond, or -1 if there's no timeout
        let timeout = duration
            .map(|d| {
                d.as_millis()
                    .try_into()
                    .unwrap_or(i32::MAX)
                    .saturating_add(if d.as_nanos() > 0 { 1 } else { 0 })
            })
            .unwrap_or(-1);
        Ok(timeout)
    }

    unsafe fn register(&self, _pollfds: &mut Vec<PollFd<'static>>) {}
}

/// Linux timerfd that can handle timeouts of nanosecond precision
#[cfg(any(target_os = "linux", target_os = "android"))]
pub struct TimerFd {
    fd: OwnedFd,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl Timeout for TimerFd {
    fn new() -> io::Result<Self>
    where
        Self: Sized,
    {
        let fd = timerfd_create(
            TimerfdClockId::Monotonic,
            TimerfdFlags::NONBLOCK | TimerfdFlags::CLOEXEC,
        )?;
        Ok(Self { fd })
    }

    fn set_timeout(&self, duration: Option<Duration>) -> io::Result<i32> {
        let itimerspec = Itimerspec {
            it_interval: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: Timespec {
                tv_sec: duration
                    .map(|d| d.as_secs().try_into().unwrap_or(i64::MAX))
                    .unwrap_or(0),
                // If duration is 0, then the timespec needs to be at least 1 nanosecond, since
                // setting timespec to 0 disarms the timerfd
                tv_nsec: duration
                    .map(|d| d.subsec_nanos().max(1).into())
                    .unwrap_or(0),
            },
        };
        timerfd_settime(&self.fd, TimerfdTimerFlags::empty(), &itimerspec)?;
        Ok(-1)
    }

    unsafe fn register(&self, pollfds: &mut Vec<PollFd<'static>>) {
        pollfds.push(PollFd::from_borrowed_fd(
            BorrowedFd::borrow_raw(self.fd.as_raw_fd()),
            PollFlags::IN,
        ));
    }
}

// On Linux platforms, use eventfd for notification and timerfd for timeouts
#[cfg(any(target_os = "linux", target_os = "android"))]
pub type UnixReactor = PollReactor<EventFd, TimerFd>;
// On non-Linux platforms, use pipe for notification and the poll() argument for timeouts
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub type UnixReactor = PollReactor<PipeFd, PollTimeout>;

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        task::Wake,
        time::Instant,
    };

    use rustix::io::read;

    use super::*;
    use crate::reactor::{Notifier, Reactor};

    macro_rules! assert_reactor_wait {
        ($reactor:ident, $timeout:expr) => {{
            let res = $reactor.wait($timeout);
            let inner = $reactor.inner.borrow();
            assert!(inner.pollfds.is_empty());
            assert!(inner.wakers.is_empty());
            res
        }};
    }

    #[test]
    fn eventfd_notification() {
        let reactor = PollReactor::<EventFd, PollTimeout>::new().unwrap();
        let notifier = reactor.notifier();

        std::thread::scope(|s| {
            s.spawn(move || {
                // Make sure the notification is sent after the reactor starts waiting
                std::thread::sleep(Duration::from_millis(10));
                notifier.upgrade().unwrap().notify().unwrap();
            });
            assert_reactor_wait!(reactor, None).unwrap();
        });

        // Now send notification before reactor starts waiting
        reactor.notifier().upgrade().unwrap().notify().unwrap();
        assert_reactor_wait!(reactor, None).unwrap();
    }

    #[test]
    fn pipe_notification() {
        let reactor = PollReactor::<PipeFd, PollTimeout>::new().unwrap();
        let notifier = reactor.notifier();

        std::thread::scope(|s| {
            s.spawn(move || {
                // Make sure the notification is sent after the reactor starts waiting
                std::thread::sleep(Duration::from_millis(10));
                notifier.upgrade().unwrap().notify().unwrap();
            });
            assert_reactor_wait!(reactor, None).unwrap();
        });

        // Now send notification before reactor starts waiting
        reactor.notifier().upgrade().unwrap().notify().unwrap();
        assert_reactor_wait!(reactor, None).unwrap();
    }

    #[test]
    fn poll_timeout() {
        let reactor = PollReactor::<EventFd, PollTimeout>::new().unwrap();
        assert_reactor_wait!(reactor, Some(Duration::from_millis(0))).unwrap();

        let start = Instant::now();
        assert_reactor_wait!(reactor, Some(Duration::from_millis(10))).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(10));

        let start = Instant::now();
        assert_reactor_wait!(reactor, Some(Duration::from_nanos(10))).unwrap();
        // Expect time to round up to nearest millisecond
        assert!(start.elapsed() >= Duration::from_millis(1));
    }

    #[test]
    fn timerfd_timeout() {
        let reactor = PollReactor::<EventFd, TimerFd>::new().unwrap();
        assert_reactor_wait!(reactor, Some(Duration::from_millis(0))).unwrap();

        let start = Instant::now();
        assert_reactor_wait!(reactor, Some(Duration::from_millis(10))).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(10));

        let start = Instant::now();
        assert_reactor_wait!(reactor, Some(Duration::from_nanos(10))).unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_nanos(10) && elapsed < Duration::from_millis(1));
    }

    #[derive(Default)]
    struct MockWaker(AtomicBool);
    impl Wake for MockWaker {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn multiple_events() {
        const COUNT: usize = 5;
        let reactor = PollReactor::<EventFd, PollTimeout>::new().unwrap();
        let events: Vec<_> = (0..COUNT).map(|_| EventFd::new().unwrap()).collect();
        let wakers: Vec<_> = (0..COUNT).map(|_| Arc::new(MockWaker::default())).collect();

        // Register 5 events and their respective wakers
        for (ev, wk) in events.iter().zip(&wakers) {
            unsafe { reactor.register(&ev.fd, Interest::read(), wk.clone().into()) };
        }

        events[0].notify().unwrap();
        events[2].notify().unwrap();
        events[4].notify().unwrap();
        assert_reactor_wait!(reactor, None).unwrap();

        for (i, wk) in wakers.iter().enumerate() {
            let awoken = wk.0.load(Ordering::Relaxed);
            match i {
                0 | 2 | 4 => assert!(awoken),
                _ => assert!(!awoken),
            }
        }
    }

    #[test]
    fn multiple_wakes() {
        const COUNT: usize = 5;
        let reactor = PollReactor::<EventFd, PollTimeout>::new().unwrap();
        let events: Vec<_> = (0..COUNT).map(|_| EventFd::new().unwrap()).collect();
        let wakers: Vec<_> = (0..COUNT).map(|_| Arc::new(MockWaker::default())).collect();

        for i in [0, 1, 4] {
            // Register 5 events and their respective wakers
            for (ev, wk) in events.iter().zip(&wakers) {
                unsafe { reactor.register(&ev.fd, Interest::read(), wk.clone().into()) };
            }
            events[i].notify().unwrap();
            assert_reactor_wait!(reactor, None).unwrap();
            assert!(wakers[i].0.load(Ordering::Relaxed));
        }

        assert!(!wakers[2].0.load(Ordering::Relaxed));
        assert!(!wakers[3].0.load(Ordering::Relaxed));
    }

    #[test]
    fn flag_notifier() {
        let notifier = FlagNotifier::new(EventFd::new().unwrap());

        // Send 10 notifications simultaneously
        std::thread::scope(|s| {
            for _ in 0..10 {
                s.spawn(|| notifier.notify());
            }
        });

        let mut eventfd_value = [0u8; 8];
        read(&notifier.inner.fd, &mut eventfd_value).unwrap();
        // The inner eventfd should have only been written once
        assert_eq!(u64::from_ne_bytes(eventfd_value), 1);
        assert!(notifier.is_notified.load(Ordering::Relaxed));
    }
}
