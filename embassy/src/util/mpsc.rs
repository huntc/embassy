//! A multi-producer, single-consumer queue for sending values between
//! asynchronous tasks, based on critical sections.
//!
//! # Safety
//!
//! **This channel is only safe on single-core systems.**
//!
//! On multi-core systems, a `CriticalSection` **is not sufficient** to ensure exclusive access.
//!
//! This module provides a bounded channel that has a limit on the number of
//! messages that it can store, and if this limit is reached, trying to send
//! another message will result in an error being returned.
//!
//! Similar to the `mpsc` channels provided by `std`, the channel constructor
//! functions provide separate send and receive handles, [`Sender`] and
//! [`Receiver`]. If there is no message to read, the current task will be
//! notified when a new value is sent. [`Sender`] allows sending values into
//! the channel. If the bounded channel is at capacity, the send is rejected.
//!
//! # Disconnection
//!
//! When all [`Sender`] handles have been dropped, it is no longer
//! possible to send values into the channel. This is considered the termination
//! event of the stream.
//!
//! If the [`Receiver`] handle is dropped, then messages can no longer
//! be read out of the channel. In this case, all further attempts to send will
//! result in an error.
//!
//! # Clean Shutdown
//!
//! When the [`Receiver`] is dropped, it is possible for unprocessed messages to
//! remain in the channel. Instead, it is usually desirable to perform a "clean"
//! shutdown. To do this, the receiver first calls `close`, which will prevent
//! any further messages to be sent into the channel. Then, the receiver
//! consumes the channel to completion, at which point the receiver can be
//! dropped.
//!
//! This channel and its associated types were derived from https://docs.rs/tokio/0.1.22/tokio/sync/mpsc/fn.channel.html

use core::borrow::BorrowMut;
use core::cell::UnsafeCell;
use core::fmt;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::task::Context;
use core::task::Poll;
use core::task::Waker;

use critical_section::CriticalSection;
use futures::Future;

use super::CriticalSectionMutex;
use super::Mutex;
use super::ThreadModeMutex;

/// Send values to the associated `Receiver`.
///
/// Instances are created by the [`channel`](channel) function.
pub struct Sender<'ch, M, T, const N: usize>
where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>,
{
    channel: *mut Channel<M, T, N>,
    phantom_data: &'ch PhantomData<T>,
}

// Safe to pass the sender around
unsafe impl<'ch, M, T, const N: usize> Send for Sender<'ch, M, T, N> where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>
{
}
unsafe impl<'ch, M, T, const N: usize> Sync for Sender<'ch, M, T, N> where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>
{
}

/// Receive values from the associated `Sender`.
///
/// Instances are created by the [`channel`](channel) function.
pub struct Receiver<'ch, M, T, const N: usize>
where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>,
{
    channel: *mut Channel<M, T, N>,
    _phantom_data: &'ch PhantomData<T>,
}

// Safe to pass the receiver around
unsafe impl<'ch, M, T, const N: usize> Send for Receiver<'ch, M, T, N> where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>
{
}
unsafe impl<'ch, M, T, const N: usize> Sync for Receiver<'ch, M, T, N> where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>
{
}

/// Splits a bounded mpsc channel into a `Sender` and `Receiver`.
///
/// All data sent on `Sender` will become available on `Receiver` in the same
/// order as it was sent.
///
/// The `Sender` can be cloned to `send` to the same channel from multiple code
/// locations. Only one `Receiver` is supported.
///
/// If the `Receiver` is disconnected while trying to `send`, the `send` method
/// will return a `SendError`. Similarly, if `Sender` is disconnected while
/// trying to `recv`, the `recv` method will return a `RecvError`.
///
/// Note that when splitting the channel, the sender and receiver cannot outlive
/// their channel:
////
/// ```compile_fail
/// use embassy::util::mpsc;
///
/// let (sender, receiver) = {
///    let mut channel = mpsc::Channel::<ThreadModeMutex, u32, 3>::new();
///     mpsc::split(&mut channel)
/// };
/// ```
pub fn split<'ch, M, T, const N: usize>(
    channel: &'ch mut Channel<M, T, N>,
) -> (Sender<'ch, M, T, N>, Receiver<'ch, M, T, N>)
where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>,
{
    let sender = Sender {
        channel,
        phantom_data: &PhantomData,
    };
    let receiver = Receiver {
        channel,
        _phantom_data: &PhantomData,
    };
    channel.register_receiver();
    channel.register_sender();
    (sender, receiver)
}

impl<'ch, M, T, const N: usize> Receiver<'ch, M, T, N>
where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>,
{
    /// Receives the next value for this receiver.
    ///
    /// This method returns `None` if the channel has been closed and there are
    /// no remaining messages in the channel's buffer. This indicates that no
    /// further values can ever be received from this `Receiver`. The channel is
    /// closed when all senders have been dropped, or when [`close`] is called.
    ///
    /// If there are no messages in the channel's buffer, but the channel has
    /// not yet been closed, this method will sleep until a message is sent or
    /// the channel is closed.
    ///
    /// Note that if [`close`] is called, but there are still outstanding
    /// messages from before it was closed, the channel is not considered
    /// closed by `recv` until they are all consumed.
    ///
    /// [`close`]: Self::close
    pub async fn recv(&mut self) -> Option<T> {
        self.await
    }

    /// Attempts to immediately receive a message on this `Receiver`
    ///
    /// This method will either receive a message from the channel immediately or return an error
    /// if the channel is empty.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        unsafe { self.channel.as_mut().unwrap().try_recv() }
    }

    /// Closes the receiving half of a channel without dropping it.
    ///
    /// This prevents any further messages from being sent on the channel while
    /// still enabling the receiver to drain messages that are buffered.
    ///
    /// To guarantee that no messages are dropped, after calling `close()`,
    /// `recv()` must be called until `None` is returned. If there are
    /// outstanding messages, the `recv` method will not return `None`
    /// until those are released.
    ///
    pub fn close(&mut self) {
        unsafe { self.channel.as_mut().unwrap().close() }
    }
}

impl<'ch, M, T, const N: usize> Future for Receiver<'ch, M, T, N>
where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>,
{
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.try_recv() {
            Ok(v) => Poll::Ready(Some(v)),
            Err(TryRecvError::Closed) => Poll::Ready(None),
            Err(TryRecvError::Empty) => {
                unsafe {
                    self.channel
                        .as_mut()
                        .unwrap()
                        .set_receiver_waker(cx.waker().clone());
                };
                Poll::Pending
            }
        }
    }
}

impl<'ch, M, T, const N: usize> Drop for Receiver<'ch, M, T, N>
where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>,
{
    fn drop(&mut self) {
        unsafe { self.channel.as_mut().unwrap().deregister_receiver() }
    }
}

impl<'ch, M, T, const N: usize> Sender<'ch, M, T, N>
where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>,
{
    /// Sends a value, waiting until there is capacity.
    ///
    /// A successful send occurs when it is determined that the other end of the
    /// channel has not hung up already. An unsuccessful send would be one where
    /// the corresponding receiver has already been closed. Note that a return
    /// value of `Err` means that the data will never be received, but a return
    /// value of `Ok` does not mean that the data will be received. It is
    /// possible for the corresponding receiver to hang up immediately after
    /// this function returns `Ok`.
    ///
    /// # Errors
    ///
    /// If the receive half of the channel is closed, either due to [`close`]
    /// being called or the [`Receiver`] handle dropping, the function returns
    /// an error. The error includes the value passed to `send`.
    ///
    /// [`close`]: Receiver::close
    /// [`Receiver`]: Receiver
    pub async fn send(&self, message: T) -> Result<(), SendError<T>> {
        SendFuture {
            sender: self.clone(),
            message: UnsafeCell::new(message),
        }
        .await
    }

    /// Attempts to immediately send a message on this `Sender`
    ///
    /// This method differs from [`send`] by returning immediately if the channel's
    /// buffer is full or no receiver is waiting to acquire some data. Compared
    /// with [`send`], this function has two failure cases instead of one (one for
    /// disconnection, one for a full buffer).
    ///
    /// # Errors
    ///
    /// If the channel capacity has been reached, i.e., the channel has `n`
    /// buffered values where `n` is the argument passed to [`channel`], then an
    /// error is returned.
    ///
    /// If the receive half of the channel is closed, either due to [`close`]
    /// being called or the [`Receiver`] handle dropping, the function returns
    /// an error. The error includes the value passed to `send`.
    ///
    /// [`send`]: Sender::send
    /// [`channel`]: channel
    /// [`close`]: Receiver::close
    pub fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        unsafe { self.channel.as_mut().unwrap().try_send(message) }
    }

    /// Completes when the receiver has dropped.
    ///
    /// This allows the producers to get notified when interest in the produced
    /// values is canceled and immediately stop doing work.
    pub async fn closed(&self) {
        CloseFuture {
            sender: self.clone(),
        }
        .await
    }

    /// Checks if the channel has been closed. This happens when the
    /// [`Receiver`] is dropped, or when the [`Receiver::close`] method is
    /// called.
    ///
    /// [`Receiver`]: crate::sync::mpsc::Receiver
    /// [`Receiver::close`]: crate::sync::mpsc::Receiver::close
    pub fn is_closed(&self) -> bool {
        unsafe { self.channel.as_mut().unwrap().is_closed() }
    }
}

struct SendFuture<'ch, M, T, const N: usize>
where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>,
{
    sender: Sender<'ch, M, T, N>,
    message: UnsafeCell<T>,
}

impl<'ch, M, T, const N: usize> Future for SendFuture<'ch, M, T, N>
where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>,
{
    type Output = Result<(), SendError<T>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.sender.try_send(unsafe { self.message.get().read() }) {
            Ok(..) => Poll::Ready(Ok(())),
            Err(TrySendError::Closed(m)) => Poll::Ready(Err(SendError(m))),
            Err(TrySendError::Full(..)) => {
                unsafe {
                    self.sender
                        .channel
                        .as_mut()
                        .unwrap()
                        .set_senders_waker(cx.waker().clone());
                };
                Poll::Pending
                // Note we leave the existing UnsafeCell contents - they still
                // contain the original message. We could create another UnsafeCell
                // with the message of Full, but there's no real need.
            }
        }
    }
}

struct CloseFuture<'ch, M, T, const N: usize>
where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>,
{
    sender: Sender<'ch, M, T, N>,
}

impl<'ch, M, T, const N: usize> Future for CloseFuture<'ch, M, T, N>
where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.sender.is_closed() {
            Poll::Ready(())
        } else {
            unsafe {
                self.sender
                    .channel
                    .as_mut()
                    .unwrap()
                    .set_senders_waker(cx.waker().clone());
            };
            Poll::Pending
        }
    }
}

impl<'ch, M, T, const N: usize> Drop for Sender<'ch, M, T, N>
where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>,
{
    fn drop(&mut self) {
        unsafe { self.channel.as_mut().unwrap().deregister_sender() }
    }
}

impl<'ch, M, T, const N: usize> Clone for Sender<'ch, M, T, N>
where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>,
{
    fn clone(&self) -> Self {
        unsafe { self.channel.as_mut().unwrap().register_sender() };
        Sender {
            channel: self.channel,
            phantom_data: self.phantom_data,
        }
    }
}

/// An error returned from the [`try_recv`] method.
///
/// [`try_recv`]: super::Receiver::try_recv
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum TryRecvError {
    /// A message could not be received because the channel is empty.
    Empty,

    /// The message could not be received because the channel is empty and closed.
    Closed,
}

/// Error returned by the `Sender`.
#[derive(Debug)]
pub struct SendError<T>(pub T);

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "channel closed")
    }
}

/// This enumeration is the list of the possible error outcomes for the
/// [try_send](super::Sender::try_send) method.
#[derive(Debug)]
pub enum TrySendError<T> {
    /// The data could not be sent on the channel because the channel is
    /// currently full and sending would require blocking.
    Full(T),

    /// The receive half of the channel was explicitly closed or has been
    /// dropped.
    Closed(T),
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            fmt,
            "{}",
            match self {
                TrySendError::Full(..) => "no available capacity",
                TrySendError::Closed(..) => "channel closed",
            }
        )
    }
}

pub struct ChannelState<T, const N: usize> {
    buf: [MaybeUninit<UnsafeCell<T>>; N],
    read_pos: usize,
    write_pos: usize,
    full: bool,
    closing: bool,
    closed: bool,
    receiver_registered: bool,
    senders_registered: u32,
    receiver_waker: Option<Waker>,
    senders_waker: Option<Waker>,
}

impl<T, const N: usize> ChannelState<T, N> {
    const INIT: MaybeUninit<UnsafeCell<T>> = MaybeUninit::uninit();

    const fn new() -> Self {
        let buf = [Self::INIT; N];
        let read_pos = 0;
        let write_pos = 0;
        let full = false;
        let closing = false;
        let closed = false;
        let receiver_registered = false;
        let senders_registered = 0;
        let receiver_waker = None;
        let senders_waker = None;
        ChannelState {
            buf,
            read_pos,
            write_pos,
            full,
            closing,
            closed,
            receiver_registered,
            senders_registered,
            receiver_waker,
            senders_waker,
        }
    }
}

/// A a bounded mpsc channel for communicating between asynchronous tasks
/// with backpressure.
///
/// The channel will buffer up to the provided number of messages.  Once the
/// buffer is full, attempts to `send` new messages will wait until a message is
/// received from the channel.
///
/// All data sent will become available in the same order as it was sent.
pub struct Channel<M, T, const N: usize>
where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>,
{
    mutex: M,
}

pub type WithCriticalSections<T, const N: usize> =
    CriticalSectionMutex<UnsafeCell<ChannelState<T, N>>>;

impl<T, const N: usize> Channel<CriticalSectionMutex<UnsafeCell<ChannelState<T, N>>>, T, N> {
    /// Establish a new bounded channel using critical sections. Critical sections
    /// should be used only single core targets where communication is required
    /// from exception mode e.g. interrupt handlers. To create one:
    ///
    /// ```
    /// use embassy::util::mpsc;
    ///
    /// // Declare a bounded channel of 3 u32s.
    /// let mut channel = mpsc::Channel::<u32, 3>::with_critical_sections();
    /// // once we have a channel, obtain its sender and receiver
    /// let (sender, receiver) = mpsc::split(&mut channel);
    /// ```
    pub const fn with_critical_sections() -> Self {
        let mutex = CriticalSectionMutex::new(UnsafeCell::new(ChannelState::new()));
        Channel { mutex }
    }
}

pub type WithThreadModeOnly<T, const N: usize> = ThreadModeMutex<UnsafeCell<ChannelState<T, N>>>;

impl<T, const N: usize> Channel<ThreadModeMutex<UnsafeCell<ChannelState<T, N>>>, T, N> {
    /// Establish a new bounded channel for use in Cortex-M thread mode. Thread
    /// mode is intended for application threads on a single core, not interrupts.
    /// As such, only one task at a time can acquire a resource and so this
    /// channel avoids all locks. To create one:
    ///
    /// ```
    /// use embassy::util::mpsc;
    ///
    /// // Declare a bounded channel of 3 u32s.
    /// let mut channel = mpsc::Channel::<u32, 3>::with_thread_mode_only();
    /// // once we have a channel, obtain its sender and receiver
    /// let (sender, receiver) = mpsc::split(&mut channel);
    /// ```
    pub const fn with_thread_mode_only() -> Self {
        let mutex = ThreadModeMutex::new(UnsafeCell::new(ChannelState::new()));
        Channel { mutex }
    }
}

impl<M, T, const N: usize> Channel<M, T, N>
where
    M: Mutex<Data = UnsafeCell<ChannelState<T, N>>>,
{
    fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.mutex.lock(|state| {
            let mut state = unsafe { state.get().read() };
            if !state.closed {
                if state.read_pos != state.write_pos || state.full {
                    if state.full {
                        state.full = false;
                        // self.wake_senders(); FIXME
                    }
                    let message =
                        unsafe { (state.buf[state.read_pos]).assume_init_mut().get().read() };
                    state.read_pos = (state.read_pos + 1) % state.buf.len();
                    Ok(message)
                } else if !state.closing {
                    Err(TryRecvError::Empty)
                } else {
                    state.closed = true;
                    // self.wake_senders(); FIXME
                    Err(TryRecvError::Closed)
                }
            } else {
                Err(TryRecvError::Closed)
            }
        })
    }

    fn try_send(&mut self, message: T) -> Result<(), TrySendError<T>> {
        self.mutex.lock(|state| {
            let mut state = unsafe { state.get().read() };
            if !state.closed {
                if !state.full {
                    state.buf[state.write_pos] = MaybeUninit::new(message.into());
                    state.write_pos = (state.write_pos + 1) % state.buf.len();
                    if state.write_pos == state.read_pos {
                        state.full = true;
                    }
                    // self.wake_receiver(); FIXME
                    Ok(())
                } else {
                    Err(TrySendError::Full(message))
                }
            } else {
                Err(TrySendError::Closed(message))
            }
        })
    }

    fn close(&mut self) {
        self.wake_receiver();
        self.mutex.lock(|state| {
            let mut state = unsafe { state.get().read() };
            state.closing = true;
        });
    }

    fn is_closed(&mut self) -> bool {
        self.mutex.lock(|state| {
            let state = unsafe { state.get().read() };
            state.closing || state.closed
        })
    }

    fn register_receiver(&mut self) {
        self.mutex.lock(|state| {
            let mut state = unsafe { state.get().read() };
            assert!(!state.receiver_registered);
            state.receiver_registered = true;
        });
    }

    fn deregister_receiver(&mut self) {
        self.mutex.lock(|state| {
            let mut state = unsafe { state.get().read() };
            if state.receiver_registered {
                state.closed = true;
                // self.wake_senders(); FIXME
            }
            state.receiver_registered = false;
        })
    }

    fn register_sender(&mut self) {
        self.mutex.lock(|state| {
            let mut state = unsafe { state.get().read() };
            state.senders_registered = state.senders_registered + 1;
        })
    }

    fn deregister_sender(&mut self) {
        self.mutex.lock(|state| {
            let mut state = unsafe { state.get().read() };
            assert!(state.senders_registered > 0);
            state.senders_registered = state.senders_registered - 1;
            if state.senders_registered == 0 {
                // state.close(); //FIXME
            }
        })
    }

    fn set_receiver_waker(&mut self, receiver_waker: Waker) {
        self.mutex.lock(|state| {
            let mut state = unsafe { state.get().read() };
            state.receiver_waker = Some(receiver_waker);
        })
    }

    fn wake_receiver(&mut self) {
        self.mutex.lock(|state| {
            let state = unsafe { state.get().read() };
            if let Some(waker) = state.receiver_waker.clone() {
                waker.wake();
            }
        })
    }

    fn set_senders_waker(&mut self, senders_waker: Waker) {
        self.mutex.lock(|state| {
            let mut state = unsafe { state.get().read() };

            // Dispose of any existing sender, causing them to poll again.
            // This could cause a spin given multiple concurrent senders, however given that
            // most sends only block waiting for the receiver to become active, this should
            // be a short-lived activity. The upside is a greatly simplified implementation
            // that avoids the need for intrusive linked-lists and unsafe operations on pinned
            // pointers.
            if let Some(waker) = state.senders_waker.clone() {
                if !senders_waker.will_wake(&waker) {
                    trace!("Waking an an active send waker due to being superseded with a new one. While benign, please report this.");
                    waker.wake();
                }
            }
            state.senders_waker = Some(senders_waker);
        })
    }

    fn wake_senders(&mut self) {
        self.mutex.lock(|state| {
            let mut state = unsafe { state.get().read() };
            if let Some(waker) = state.senders_waker.clone() {
                waker.wake();
                state.senders_waker = None;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use futures::task::SpawnExt;
    use futures_executor::ThreadPool;
    use futures_timer::Delay;

    use super::*;

    fn capacity<T, const N: usize>(c: &Channel<T, N>) -> usize {
        if !c.full {
            if c.write_pos > c.read_pos {
                (c.buf.len() - c.write_pos) + c.read_pos
            } else {
                (c.buf.len() - c.read_pos) + c.write_pos
            }
        } else {
            0
        }
    }

    #[test]
    fn sending_once() {
        critical_section::with(|cs| {
            let mut c = Channel::<u32, 3>::new();
            assert!(c.try_send(1, cs).is_ok());
            assert_eq!(capacity(&c), 2);
        })
    }

    #[test]
    fn sending_when_full() {
        critical_section::with(|cs| {
            let mut c = Channel::<u32, 3>::new();
            let _ = c.try_send(1, cs);
            let _ = c.try_send(1, cs);
            let _ = c.try_send(1, cs);
            match c.try_send(2, cs) {
                Err(TrySendError::Full(2)) => assert!(true),
                _ => assert!(false),
            }
            assert_eq!(capacity(&c), 0);
        })
    }

    #[test]
    fn sending_when_closed() {
        critical_section::with(|cs| {
            let mut c = Channel::<u32, 3>::new();
            c.closed = true;
            match c.try_send(2, cs) {
                Err(TrySendError::Closed(2)) => assert!(true),
                _ => assert!(false),
            }
        })
    }

    #[test]
    fn receiving_once_with_one_send() {
        critical_section::with(|cs| {
            let mut c = Channel::<u32, 3>::new();
            assert!(c.try_send(1, cs).is_ok());
            assert_eq!(c.try_recv(cs).unwrap(), 1);
            assert_eq!(capacity(&c), 3);
        })
    }

    #[test]
    fn receiving_when_empty() {
        critical_section::with(|cs| {
            let mut c = Channel::<u32, 3>::new();
            match c.try_recv(cs) {
                Err(TryRecvError::Empty) => assert!(true),
                _ => assert!(false),
            }
            assert_eq!(capacity(&c), 3);
        })
    }

    #[test]
    fn receiving_when_closed() {
        critical_section::with(|cs| {
            let mut c = Channel::<u32, 3>::new();
            c.closed = true;
            match c.try_recv(cs) {
                Err(TryRecvError::Closed) => assert!(true),
                _ => assert!(false),
            }
        })
    }

    #[test]
    fn simple_send_and_receive() {
        let mut c = Channel::<u32, 3>::new();
        let (s, r) = split(&mut c);
        assert!(s.clone().try_send(1).is_ok());
        assert_eq!(r.try_recv().unwrap(), 1);
    }

    #[test]
    fn should_close_without_sender() {
        let mut c = Channel::<u32, 3>::new();
        let (s, r) = split(&mut c);
        drop(s);
        match r.try_recv() {
            Err(TryRecvError::Closed) => assert!(true),
            _ => assert!(false),
        }
    }

    #[test]
    fn should_close_once_drained() {
        let mut c = Channel::<u32, 3>::new();
        let (s, r) = split(&mut c);
        assert!(s.try_send(1).is_ok());
        drop(s);
        assert_eq!(r.try_recv().unwrap(), 1);
        match r.try_recv() {
            Err(TryRecvError::Closed) => assert!(true),
            _ => assert!(false),
        }
    }

    #[test]
    fn should_reject_send_when_receiver_dropped() {
        let mut c = Channel::<u32, 3>::new();
        let (s, r) = split(&mut c);
        drop(r);
        match s.try_send(1) {
            Err(TrySendError::Closed(1)) => assert!(true),
            _ => assert!(false),
        }
    }

    #[test]
    fn should_reject_send_when_channel_closed() {
        let mut c = Channel::<u32, 3>::new();
        let (s, mut r) = split(&mut c);
        assert!(s.try_send(1).is_ok());
        r.close();
        assert_eq!(r.try_recv().unwrap(), 1);
        match r.try_recv() {
            Err(TryRecvError::Closed) => assert!(true),
            _ => assert!(false),
        }
        assert!(s.is_closed());
    }

    #[futures_test::test]
    async fn receiver_closes_when_sender_dropped_async() {
        let executor = ThreadPool::new().unwrap();

        static mut CHANNEL: Channel<u32, 3> = Channel::new();
        let (s, mut r) = split(unsafe { &mut CHANNEL });
        assert!(executor
            .spawn(async move {
                drop(s);
            })
            .is_ok());
        assert_eq!(r.recv().await, None);
    }

    #[futures_test::test]
    async fn receiver_receives_given_try_send_async() {
        let executor = ThreadPool::new().unwrap();

        static mut CHANNEL: Channel<u32, 3> = Channel::new();
        let (s, mut r) = split(unsafe { &mut CHANNEL });
        assert!(executor
            .spawn(async move {
                let _ = s.try_send(1);
            })
            .is_ok());
        assert_eq!(r.recv().await, Some(1));
    }

    #[futures_test::test]
    async fn sender_send_completes_if_capacity() {
        static mut CHANNEL: Channel<u32, 1> = Channel::new();
        let (s, mut r) = split(unsafe { &mut CHANNEL });
        assert!(s.send(1).await.is_ok());
        assert_eq!(r.recv().await, Some(1));
    }

    #[futures_test::test]
    async fn sender_send_completes_if_closed() {
        static mut CHANNEL: Channel<u32, 1> = Channel::new();
        let (s, r) = split(unsafe { &mut CHANNEL });
        drop(r);
        match s.send(1).await {
            Err(SendError(1)) => assert!(true),
            _ => assert!(false),
        }
    }

    #[futures_test::test]
    async fn senders_sends_wait_until_capacity() {
        let executor = ThreadPool::new().unwrap();

        static mut CHANNEL: Channel<u32, 1> = Channel::new();
        let (s0, mut r) = split(unsafe { &mut CHANNEL });
        assert!(s0.try_send(1).is_ok());
        let s1 = s0.clone();
        let send_task_1 = executor.spawn_with_handle(async move { s0.send(2).await });
        let send_task_2 = executor.spawn_with_handle(async move { s1.send(3).await });
        // Wish I could think of a means of determining that the async send is waiting instead.
        // However, I've used the debugger to observe that the send does indeed wait.
        assert!(Delay::new(Duration::from_millis(500)).await.is_ok());
        assert_eq!(r.recv().await, Some(1));
        assert!(r.recv().await.is_some());
        assert!(r.recv().await.is_some());
        assert!(send_task_1.unwrap().await.is_ok());
        assert!(send_task_2.unwrap().await.is_ok());
    }

    #[futures_test::test]
    async fn sender_close_completes_if_closing() {
        static mut CHANNEL: Channel<u32, 1> = Channel::new();
        let (s, mut r) = split(unsafe { &mut CHANNEL });
        r.close();
        s.closed().await;
    }

    #[futures_test::test]
    async fn sender_close_completes_if_closed() {
        static mut CHANNEL: Channel<u32, 1> = Channel::new();
        let (s, r) = split(unsafe { &mut CHANNEL });
        drop(r);
        s.closed().await;
    }
}
