use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use hyper::rt::{Executor, Sleep, Timer};
use send_wrapper::SendWrapper;

/// An executor service based on [`compio::runtime`]. It uses
/// [`compio::runtime::spawn`] interally.
#[derive(Debug, Default, Clone)]
pub struct CompioExecutor;

impl<F: Future<Output = ()> + Send + 'static> Executor<F> for CompioExecutor {
    fn execute(&self, fut: F) {
        compio::runtime::spawn(fut).detach();
    }
}

struct SleepFuture<T: Send + Sync + Future<Output = ()>>(T);

impl<T: Send + Sync + Future<Output = ()>> Sleep for SleepFuture<T> {}

impl<T: Send + Sync + Future<Output = ()>> Future for SleepFuture<T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe { self.map_unchecked_mut(|this| &mut this.0) }.poll(cx)
    }
}

/// An timer service based on [`compio::time`].
#[derive(Debug, Default, Clone)]
pub struct CompioTimer;

impl Timer for CompioTimer {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Sleep>> {
        Box::pin(SleepFuture(SendWrapper::new(compio::time::sleep(duration))))
    }

    fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn Sleep>> {
        Box::pin(SleepFuture(SendWrapper::new(compio::time::sleep_until(
            deadline,
        ))))
    }
}
