use std::future::Future;
use std::io;
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd as _, RawFd};
use tokio::io::unix::AsyncFd;
use tokio::task::LocalSet;

mod context;
pub(crate) mod driver;

pub(crate) use context::RuntimeContext;

thread_local! {
    pub(crate) static CONTEXT: RuntimeContext = RuntimeContext::new();
}

/// The Runtime Executor
///
/// This is the Runtime for `tokio-uring`.
/// It wraps the default [`Runtime`] using the platform-specific Driver.
///
/// This executes futures and tasks within the current-thread only.
///
/// [`Runtime`]: tokio::runtime::Runtime
pub struct Runtime {
    /// Tokio runtime, always current-thread
    tokio_rt: ManuallyDrop<tokio::runtime::Runtime>,

    /// LocalSet for !Send tasks
    local: ManuallyDrop<LocalSet>,

    /// Strong reference to the driver.
    driver: driver::Handle,
}

/// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
///
/// Spawning a task enables the task to execute concurrently to other tasks.
/// There is no guarantee that a spawned task will execute to completion. When a
/// runtime is shutdown, all outstanding tasks are dropped, regardless of the
/// lifecycle of that task.
///
/// This function must be called from the context of a `tokio-uring` runtime.
///
/// [`JoinHandle`]: tokio::task::JoinHandle
///
/// # Examples
///
/// In this example, a server is started and `spawn` is used to start a new task
/// that processes each received connection.
///
/// ```no_run
/// tokio_uring::start(async {
///     let handle = tokio_uring::spawn(async {
///         println!("hello from a background task");
///     });
///
///     // Let the task complete
///     handle.await.unwrap();
/// });
/// ```
pub fn spawn<T: Future + 'static>(task: T) -> tokio::task::JoinHandle<T::Output> {
    tokio::task::spawn_local(task)
}

impl Runtime {
    /// Creates a new tokio_uring runtime on the current thread.
    ///
    /// This takes the tokio-uring [`Builder`](crate::Builder) as a parameter.
    pub fn new(b: &crate::Builder) -> io::Result<Runtime> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .on_thread_park(|| {
                CONTEXT.with(|x| {
                    let driver = x
                        .handle()
                        .expect("Internal error, driver context not present when invoking hooks")
                        .flush();
                });
            })
            .enable_all()
            .build()?;

        let tokio_rt = ManuallyDrop::new(rt);
        let local = ManuallyDrop::new(LocalSet::new());
        let driver = driver::Handle::new(b)?;

        start_uring_wakes_task(&tokio_rt, &local, driver.clone());

        Ok(Runtime {
            local,
            tokio_rt,
            driver,
        })
    }

    /// Returns a reference to the local set.
    pub fn local(&self) -> &LocalSet {
        &self.local
    }

    /// Returns a reference to the underlying singlethreaded tokio runtime
    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        &self.tokio_rt
    }

    /// Returns the driver fd
    pub fn driver_fd(&self) -> RawFd {
        self.driver.as_raw_fd()
    }

    /// Runs a future to completion on the tokio-uring runtime. This is the
    /// runtime's entry point.
    ///
    /// This runs the given future on the current thread, blocking until it is
    /// complete, and yielding its resolved result. Any tasks, futures, or timers
    /// which the future spawns internally will be executed on this runtime.
    ///
    /// Any spawned tasks will be suspended after `block_on` returns. Calling
    /// `block_on` again will resume previously spawned tasks.
    ///
    /// # Panics
    ///
    /// This function panics if the provided future panics, or if called within an
    /// asynchronous execution context.
    /// Runs a future to completion on the current runtime.
    pub fn block_on<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        struct ContextGuard;

        impl Drop for ContextGuard {
            fn drop(&mut self) {
                CONTEXT.with(|cx| {
                    let _ = cx.handle().unwrap().unregister_buffers_cleanup();
                    cx.unset_driver();
                });
            }
        }

        CONTEXT.with(|cx| cx.set_handle(self.driver.clone()));

        // println!(
        //     "Setting up uring runtime on thread {:?}",
        //     std::thread::current().id()
        // );

        let _guard = ContextGuard;

        tokio::pin!(future);

        let res = self
            .tokio_rt
            .block_on(self.local.run_until(std::future::poll_fn(|cx| {
                // assert!(drive.as_mut().poll(cx).is_pending());
                future.as_mut().poll(cx)
            })));

        res
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // drop tasks in correct order
        unsafe {
            ManuallyDrop::drop(&mut self.local);
            ManuallyDrop::drop(&mut self.tokio_rt);
        }
    }
}

fn start_uring_wakes_task(
    tokio_rt: &tokio::runtime::Runtime,
    local: &LocalSet,
    driver: driver::Handle,
) {
    let _guard = tokio_rt.enter();
    driver.register_eventfd(driver.as_raw_fd()).unwrap();
    let async_driver_handle = AsyncFd::new(driver).unwrap();

    local.spawn_local(drive_uring_wakes(async_driver_handle));
}

async fn drive_uring_wakes(driver: AsyncFd<driver::Handle>) {
    const IDLE_EPOLL: u128 = 15; // ms - make this configurable?
    let mut last_success;
    'epolled: loop {
        // Wait for read-readiness - this makes a epoll_wait syscall
        let mut guard = driver.readable().await.unwrap();
        // println!("Woken up by epoll");

        guard.get_inner().drive_cq();

        'polled: loop {
            while guard.get_inner().dispatch_completions() > 0 {
                // println!("completions dispatched");
                // we busy yield while there is a stream of incoming completions
                tokio::task::yield_now().await;
                guard.get_inner().drive_cq();
            }

            last_success = std::time::Instant::now();

            loop {
                tokio::task::yield_now().await;
                if guard.get_inner().dispatch_completions() > 0 {
                    // println!("completions dispatched 2");
                    tokio::task::yield_now().await; // we dispatch_completions() again right after the continue, so yield now

                    guard.get_inner().drive_cq();
                    continue 'polled;
                }

                // could be a while loop but we want to check this at the end of the loop
                // we break when there have been no CQEs for IDLE_EPOLL
                if last_success.elapsed().as_millis() > IDLE_EPOLL {
                    // println!("Idle - now epolling");
                    break;
                }
            }

            guard.clear_ready();
            continue 'epolled;
        }
    }
}

#[cfg(test)]
mod test {

    use super::*;
    use crate::builder;

    #[test]
    fn block_on() {
        let rt = Runtime::new(&builder()).unwrap();
        rt.block_on(async move { () });
    }

    #[test]
    fn block_on_twice() {
        let rt = Runtime::new(&builder()).unwrap();
        rt.block_on(async move { () });
        rt.block_on(async move { () });
    }
}
