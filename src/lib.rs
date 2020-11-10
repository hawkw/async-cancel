use pin_project_lite::pin_project;
use std::{
    fmt,
    future::Future,
    mem,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::watch;

macro_rules! trace {
    ($($arg:tt)*) => {
        #[cfg(any(feature = "tracing", test))]
        tracing::trace!($($arg)*)
    }
}

/// A sender for cancellation signals.
///
/// This is used to signal cancellation, and to wait for associated tasks to
/// complete.
pub struct Cancel {
    tx: watch::Sender<()>,
}

pub struct Completed(Pin<Box<dyn Future<Output = ()> + Send + Sync>>);

#[derive(Clone)]
pub struct Watch {
    rx: watch::Receiver<()>,
}

pin_project! {
    pub struct Watching<F, N = fn(Pin<&mut F>)> {
        state: State<N>,
        #[pin]
        future: F,
        _rx: watch::Receiver<()>,
    }
}

pin_project! {
    pub struct Forceful<F> {
        #[pin]
        future: Option<F>
    }
}

enum State<N> {
    Watching {
        signal: Pin<Box<dyn Future<Output = ()> + Send + Sync>>,
        action: N,
    },
    Canceled,
}

impl Cancel {
    pub fn cancel(self) -> Completed {
        // XXX(eliza): should this return an error if all watchers were dropped?
        let _ = self.tx.send(());

        trace!("sent cancelation signal");

        Canceled(Box::pin(async move {
            trace!("waiting for watchers to cancel...");
            self.tx.closed().await;
            trace!("all watchers canceled!");
        }))
    }
}

pub fn channel() -> (Cancel, Watch) {
    let (tx, rx) = watch::channel(());
    (Cancel { tx }, Watch { rx })
}

// === impl Watch ===

impl Watch {
    pub fn watch<F, N>(self, future: F, action: N) -> Watching<F, N>
    where
        F: Future,
        N: FnOnce(Pin<&mut F>),
    {
        let mut rx = self.rx.clone();
        let signal = Box::pin(async move {
            let _ = rx.changed().await;
        });
        Watching {
            state: State::Watching { signal, action },
            future,
            _rx: self.rx,
        }
    }

    pub fn watch_forceful<F, N>(self, future: F) -> Watching<Forceful<F>>
    where
        F: Future,
    {
        self.watch(
            Forceful {
                future: Some(future),
            },
            Forceful::cancel_now,
        )
    }
}

impl fmt::Debug for Watch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("Watch { .. }")
    }
}

// === impl Watching ===

impl<F, N> Future for Watching<F, N>
where
    F: Future,
    N: FnOnce(Pin<&mut F>),
{
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        *this.state = match mem::replace(this.state, State::Canceled) {
            State::Watching { mut signal, action } => {
                if Pin::new(&mut signal).poll(cx).is_ready() {
                    trace!(canceled = true);
                    action(this.future.as_mut());
                    State::Canceled
                } else {
                    State::Watching { signal, action }
                }
            }
            State::Canceled => State::Canceled,
        };

        let ret = this.future.poll(cx);
        trace!(state = ?this.state, completed = ret.is_ready());
        ret
    }
}

impl<F, N> fmt::Debug for Watching<F, N>
where
    F: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Watching")
            .field("state", &self.state)
            .field("future", &self.future)
            .finish()
    }
}

// === impl Forceful ===

impl<F> Forceful<F> {
    pub fn cancel_now(self: Pin<&mut Self>) {
        self.project().future.set(None);
    }
}

impl<F: Future> Future for Forceful<F> {
    type Output = Option<F::Output>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(f) = self.project().future.as_pin_mut() {
            f.poll(cx).map(Some)
        } else {
            Poll::Ready(None)
        }
    }
}

impl<F> fmt::Debug for Forceful<F>
where
    F: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Forceful").field(&self.future).finish()
    }
}

// === impl Canceled ===

impl Future for Canceled {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0.as_mut()).poll(cx)
    }
}

impl<N> fmt::Debug for State<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            State::Watching { .. } => f.pad("State::Watching(..)"),
            State::Canceled { .. } => f.pad("State::Canceled"),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
        Arc,
    };
    use tokio_test::{assert_pending, assert_ready, task};

    struct TestMe {
        draining: AtomicBool,
        finished: AtomicBool,
        poll_cnt: AtomicUsize,
    }

    struct TestMeFut(Arc<TestMe>);

    impl Future for TestMeFut {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.as_ref();
            this.0.poll_cnt.fetch_add(1, Relaxed);
            if this.0.finished.load(Relaxed) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }

    fn trace_init() -> impl Drop {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::TRACE)
            .set_default()
    }

    #[test]
    fn watch() {
        let _trace = trace_init();

        let (tx, rx) = channel();
        let fut = Arc::new(TestMe {
            draining: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            poll_cnt: AtomicUsize::new(0),
        });

        let mut watch = task::spawn(rx.watch(TestMeFut(fut.clone()), |fut| {
            fut.0.draining.store(true, Relaxed)
        }));

        assert_eq!(fut.poll_cnt.load(Relaxed), 0);

        // First poll should poll the inner future, 1);
        assert_pending!(watch.poll());
        assert_eq!(fut.poll_cnt.load(Relaxed), 1);

        // Second poll should poll the inner future again
        assert_pending!(watch.poll());
        assert_eq!(fut.poll_cnt.load(Relaxed), 2);

        let mut draining = task::spawn(tx.cancel());
        // Drain signaled, but needs another poll to be noticed.
        assert!(!fut.draining.load(Relaxed));
        assert_eq!(fut.poll_cnt.load(Relaxed), 2);

        // Now, poll after drain has been signaled.
        assert_pending!(watch.poll());
        assert!(fut.draining.load(Relaxed));
        assert_eq!(fut.poll_cnt.load(Relaxed), 3);

        // Draining is not ready until watcher completes
        assert_pending!(draining.poll());

        // Finishing up the watch future
        fut.finished.store(true, Relaxed);
        assert_ready!(watch.poll());
        drop(watch);

        assert_ready!(draining.poll());
    }

    #[test]
    fn watch_clones() {
        let _trace = trace_init();
        let (tx, rx) = channel();
        let fut1 = Arc::new(TestMe {
            draining: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            poll_cnt: AtomicUsize::new(0),
        });
        let fut2 = Arc::new(TestMe {
            draining: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            poll_cnt: AtomicUsize::new(0),
        });

        let watch1 = task::spawn(rx.clone().watch(TestMeFut(fut1.clone()), |fut| {
            fut.0.draining.store(true, Relaxed)
        }));

        let watch2 = task::spawn(rx.watch(TestMeFut(fut2.clone()), |fut| {
            fut.0.draining.store(true, Relaxed)
        }));

        let mut draining = task::spawn(tx.cancel());

        // Still 2 outstanding watchers
        assert_pending!(draining.poll());

        // drop 1 for whatever reason
        drop(watch1);

        // Still not ready, 1 other watcher still pending
        assert_pending!(draining.poll());

        drop(watch2);

        // Now all watchers are gone, draining is complete
        assert_ready!(draining.poll())
    }
}
