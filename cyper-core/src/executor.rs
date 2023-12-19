use std::future::Future;

use hyper::rt::Executor;

/// An executor service based on [`compio::runtime`]. It uses
/// [`compio::runtime::spawn`] interally.
#[derive(Debug, Default, Clone)]
pub struct CompioExecutor;

impl<F: Future<Output = ()> + Send + 'static> Executor<F> for CompioExecutor {
    fn execute(&self, fut: F) {
        compio::runtime::spawn(fut).detach();
    }
}
