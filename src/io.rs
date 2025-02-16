#[cfg(unix)]
use std::os::fd::{AsFd, BorrowedFd};
use std::{
    future::poll_fn,
    io::{self, BufRead, ErrorKind, Read, Write},
    marker::PhantomData,
    net::{SocketAddr, TcpListener, TcpStream},
    pin::Pin,
    task::{Context, Poll},
};

use futures_io::{AsyncBufRead, AsyncRead, AsyncWrite};

use crate::{
    reactor::{EventHandle, Interest},
    Reactor, REACTOR,
};

// Deregisters the event handle on drop to ensure I/O safety
struct GuardedHandle(EventHandle);
impl Drop for GuardedHandle {
    fn drop(&mut self) {
        REACTOR.with(|r| r.deregister(&self.0));
    }
}

pub struct Async<T> {
    // Make sure the handle is dropped before the inner I/O type
    handle: GuardedHandle,
    inner: T,
    _phantom: PhantomData<*const ()>,
}

impl<T> Unpin for Async<T> {}

#[cfg(unix)]
impl<T: AsFd> Async<T> {
    pub fn without_nonblocking(inner: T) -> Self {
        // SAFETY: GuardedHandle's Drop impl will deregister the FD
        let handle = GuardedHandle(unsafe { REACTOR.with(|r| r.register(&inner)) });
        Self {
            inner,
            handle,
            _phantom: PhantomData,
        }
    }

    pub fn new(inner: T) -> io::Result<Self> {
        set_nonblocking(inner.as_fd())?;
        Ok(Self::without_nonblocking(inner))
    }
}

#[cfg(unix)]
fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    {
        let previous = rustix::fs::fcntl_getfl(fd)?;
        let new = previous | rustix::fs::OFlags::NONBLOCK;
        if new != previous {
            rustix::fs::fcntl_setfl(fd, new)?;
        }
    }
    Ok(())
}

impl<T> Async<T> {
    pub fn get_ref(&self) -> &T {
        &self.inner
    }

    pub fn into_inner(self) -> T {
        self.inner
    }

    fn poll_event<'a, P, F>(
        &'a self,
        interest: Interest,
        cx: &mut Context<'_>,
        f: F,
    ) -> Poll<io::Result<P>>
    where
        F: FnOnce(&'a T) -> io::Result<P>,
    {
        match f(&self.inner) {
            Ok(n) => return Poll::Ready(Ok(n)),
            Err(err) if err.kind() == ErrorKind::WouldBlock => {}
            Err(err) => return Poll::Ready(Err(err)),
        }
        REACTOR.with(|r| r.enable_event(&self.handle.0, interest, cx.waker()));
        Poll::Pending
    }

    fn poll_event_mut<'a, P, F>(
        &'a mut self,
        interest: Interest,
        cx: &mut Context<'_>,
        f: F,
    ) -> Poll<io::Result<P>>
    where
        F: FnOnce(&'a mut T) -> io::Result<P>,
    {
        match f(&mut self.inner) {
            Ok(n) => return Poll::Ready(Ok(n)),
            Err(err) if err.kind() == ErrorKind::WouldBlock => {}
            Err(err) => return Poll::Ready(Err(err)),
        }
        REACTOR.with(|r| r.enable_event(&self.handle.0, interest, cx.waker()));
        Poll::Pending
    }
}

impl<T: Read> AsyncRead for Async<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_event_mut(Interest::Read, cx, |inner| inner.read(buf))
    }
}

impl<'a, T> AsyncRead for &'a Async<T>
where
    &'a T: Read,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_event(Interest::Read, cx, |mut inner| inner.read(buf))
    }
}

impl<T: Write> AsyncWrite for Async<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_event_mut(Interest::Write, cx, |inner| inner.write(buf))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_event_mut(Interest::Write, cx, |inner| inner.flush())
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl<'a, T> AsyncWrite for &'a Async<T>
where
    &'a T: Write,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_event(Interest::Write, cx, |mut inner| inner.write(buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_event(Interest::Write, cx, |mut inner| inner.flush())
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl<T: BufRead> AsyncBufRead for Async<T> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        this.poll_event_mut(Interest::Read, cx, |inner| inner.fill_buf())
    }

    fn consume(mut self: Pin<&mut Self>, amt: usize) {
        BufRead::consume(&mut self.inner, amt);
    }
}

impl Async<TcpListener> {
    pub fn bind<A: Into<SocketAddr>>(addr: A) -> io::Result<Self> {
        Async::new(TcpListener::bind(addr.into())?)
    }

    pub async fn accept(&self) -> io::Result<(Async<TcpStream>, SocketAddr)> {
        poll_fn(|cx| self.poll_event(Interest::Read, cx, |inner| inner.accept()))
            .await
            .and_then(|(st, addr)| Async::new(st).map(|st| (st, addr)))
    }
}

impl Async<TcpStream> {
    pub async fn connect<A: Into<SocketAddr>>(addr: A) -> io::Result<Self> {
        let addr = addr.into();
        let stream = Async::without_nonblocking(tcp_socket(&addr)?);
        poll_fn(|cx| stream.poll_event(Interest::Write, cx, |inner| connect(inner, &addr))).await?;
        Ok(stream)
    }

    pub async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        poll_fn(|cx| self.poll_event(Interest::Read, cx, |inner| inner.peek(buf))).await
    }
}

#[cfg(unix)]
fn tcp_socket(addr: &SocketAddr) -> io::Result<TcpStream> {
    use rustix::net::*;

    let af = match addr {
        SocketAddr::V4(_) => AddressFamily::INET,
        SocketAddr::V6(_) => AddressFamily::INET6,
    };
    let type_ = SocketType::STREAM;
    let flags = SocketFlags::CLOEXEC | SocketFlags::NONBLOCK;
    let socket = socket_with(af, type_, flags, None)?;

    Ok(socket.into())
}

#[cfg(unix)]
fn connect(tcp: &TcpStream, addr: &SocketAddr) -> io::Result<()> {
    rustix::net::connect(tcp.as_fd(), addr)?;
    Ok(())
}
