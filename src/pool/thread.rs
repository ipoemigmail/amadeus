use async_channel::{bounded, Sender};
use futures::{future::RemoteHandle, FutureExt, TryFutureExt};
use serde_closure::traits;
use std::{
	any::Any, future::Future, io, panic::{AssertUnwindSafe, RefUnwindSafe, UnwindSafe}, pin::Pin, sync::Arc
};
use tokio::{
	runtime::Handle, task::{JoinError, LocalSet}
};

use super::util::{assert_sync_and_send, Panicked};

const DEFAULT_TASKS_PER_CORE: usize = 100;

#[derive(Debug)]
struct ThreadPoolInner {
	logical_cores: usize,
	tasks_per_core: usize,
	pool: Pool,
}

#[derive(Debug)]
pub struct ThreadPool(Arc<ThreadPoolInner>);
#[cfg_attr(not(feature = "doc"), serde_closure::generalize)]
impl ThreadPool {
	pub fn new(tasks_per_core: Option<usize>) -> io::Result<Self> {
		let logical_cores = num_cpus::get();
		let tasks_per_core = tasks_per_core.unwrap_or(DEFAULT_TASKS_PER_CORE);
		let pool = Pool::new(logical_cores);
		Ok(ThreadPool(Arc::new(ThreadPoolInner {
			logical_cores,
			tasks_per_core,
			pool,
		})))
	}
	pub fn threads(&self) -> usize {
		self.0.logical_cores * self.0.tasks_per_core
	}
	pub fn spawn<F, Fut, T>(&self, work: F) -> impl Future<Output = Result<T, Panicked>> + Send
	where
		F: FnOnce() -> Fut + Send + 'static,
		Fut: Future<Output = T> + 'static,
		T: Send + 'static,
	{
		self.0
			.pool
			.spawn_pinned(|| traits::FnOnce::call_once(work, ()))
			.map_err(JoinError::into_panic)
			.map_err(Panicked::from)
	}
}

impl Clone for ThreadPool {
	/// Cloning a pool will create a new handle to the pool.
	/// The behavior is similar to [Arc](https://doc.rust-lang.org/stable/std/sync/struct.Arc.html).
	///
	/// We could for example submit jobs from multiple threads concurrently.
	fn clone(&self) -> Self {
		Self(self.0.clone())
	}
}

impl UnwindSafe for ThreadPool {}
impl RefUnwindSafe for ThreadPool {}

fn _assert() {
	let _ = assert_sync_and_send::<ThreadPool>;
}

#[derive(Debug)]
struct Pool {
	sender: Sender<(Request, Sender<RemoteHandle<Response>>)>,
}

type Request = Box<dyn FnOnce() -> Box<dyn Future<Output = Response>> + Send>;
type Response = Result<Box<dyn Any + Send>, Box<dyn Any + Send + 'static>>;

impl Pool {
	fn new(threads: usize) -> Self {
		let handle = Handle::current();
		let handle1 = handle.clone();
		let (sender, receiver) = bounded::<(Request, Sender<RemoteHandle<Response>>)>(1);
		for _ in 0..threads {
			let receiver = receiver.clone();
			let handle = handle.clone();
			let _ = handle1.spawn_blocking(move || {
				let local = LocalSet::new();
				handle.block_on(local.run_until(async {
					while let Ok((task, sender)) = receiver.recv().await {
						let _ = local.spawn_local(async move {
							let (remote, remote_handle) = Pin::from(task()).remote_handle();
							let _ = sender.send(remote_handle).await;
							remote.await;
						});
					}
				}))
			});
		}
		Self { sender }
	}
	fn spawn_pinned<F, Fut, T>(&self, task: F) -> impl Future<Output = Result<T, JoinError>> + Send
	where
		F: FnOnce() -> Fut + Send + 'static,
		Fut: Future<Output = T> + 'static,
		T: Send + 'static,
	{
		let sender = self.sender.clone();
		async move {
			let task: Request = Box::new(|| {
				Box::new(
					AssertUnwindSafe(task().map(|t| Box::new(t) as Box<dyn Any + Send>))
						.catch_unwind(),
				)
			});
			let (sender_, receiver) = bounded::<RemoteHandle<Response>>(1);
			sender.send((task, sender_)).await.unwrap();
			let res = receiver.recv().await;
			let res = res.unwrap().await;
			#[allow(deprecated)]
			res.map(|x| *Box::<dyn Any + Send>::downcast(x).unwrap())
				.map_err(JoinError::panic)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use futures::future::join_all;
	use std::sync::{
		atomic::{AtomicUsize, Ordering}, Arc
	};

	#[tokio::test]
	async fn spawn_pinned_() {
		const TASKS: usize = 1000;
		const ITERS: usize = 1000;
		const THREADS: usize = 4;
		let pool = Pool::new(THREADS);
		let count = Arc::new(AtomicUsize::new((1..TASKS).sum()));
		for _ in 0..ITERS {
			join_all((0..TASKS).map(|i| {
				let count = count.clone();
				pool.spawn_pinned(move || async move {
					let _ = count.fetch_sub(i, Ordering::Relaxed);
				})
			}))
			.await
			.into_iter()
			.collect::<Result<(), _>>()
			.unwrap();
			assert_eq!(count.load(Ordering::Relaxed), 0);
			count.store((1..TASKS).sum(), Ordering::Relaxed);
		}
	}
}
