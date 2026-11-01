//! Background tasks and timers on either target: tokio on the desktop, the browser's event
//! loop on the web (`wasm32`), where there is one thread and no tokio runtime.

use std::future::Future;
use std::time::Duration;

use web_time::Instant;

/// Run `fut` in the background.
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn<F: Future<Output = ()> + Send + 'static>(fut: F) {
    tokio::spawn(fut);
}

/// Run `fut` in the background.
#[cfg(target_arch = "wasm32")]
pub fn spawn<F: Future<Output = ()> + 'static>(fut: F) {
    wasm_bindgen_futures::spawn_local(fut);
}

/// Run CPU-bound work off the async workers; `None` if it panicked. The web has one thread,
/// so there it runs inline.
#[cfg(not(target_arch = "wasm32"))]
pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

/// Run CPU-bound work off the async workers; `None` if it panicked. The web has one thread,
/// so there it runs inline.
#[cfg(target_arch = "wasm32")]
pub async fn blocking<T>(f: impl FnOnce() -> T) -> Option<T> {
    Some(f())
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn sleep(d: Duration) {
    tokio::time::sleep(d).await;
}

/// Resolves after `d`, through a `setTimeout` set on the first poll and cleared if the sleep is
/// dropped before it fires.
#[cfg(target_arch = "wasm32")]
pub fn sleep(d: Duration) -> impl Future<Output = ()> + Send {
    struct Timer(Option<i32>);
    impl Drop for Timer {
        fn drop(&mut self) {
            if let (Some(id), Some(w)) = (self.0, web_sys::window()) {
                w.clear_timeout_with_handle(id);
            }
        }
    }
    let ms = d.as_millis().min(i32::MAX as u128) as i32;
    // A promise isn't `Send`. The web build has one thread, so wrapping it is sound, and the provider
    // traits' `Send` futures can then wait on it.
    send_wrapper::SendWrapper::new(async move {
        let mut timer = Timer(None);
        let promise = js_sys::Promise::new(&mut |resolve, _| {
            timer.0 = web_sys::window()
                .and_then(|w| w.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms).ok());
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
        timer.0 = None; // it fired
    })
}

/// `fut` ran past its deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("deadline has elapsed")
    }
}

impl std::error::Error for Elapsed {}

/// Run `fut` for at most `d`.
pub async fn timeout<F: Future>(d: Duration, fut: F) -> Result<F::Output, Elapsed> {
    let (fut, deadline) = (std::pin::pin!(fut), std::pin::pin!(sleep(d)));
    match futures::future::select(fut, deadline).await {
        futures::future::Either::Left((out, _)) => Ok(out),
        futures::future::Either::Right(_) => Err(Elapsed),
    }
}

/// A periodic timer: the first tick completes at once, later ones every `period`. A late
/// tick fires immediately and the next one is a full period after it.
pub struct Interval {
    period: Duration,
    next: Instant,
}

pub fn interval(period: Duration) -> Interval {
    Interval { period, next: Instant::now() }
}

impl Interval {
    /// Wait for the next tick. Dropping the future part-way (in `select!`) loses nothing.
    pub async fn tick(&mut self) {
        let now = Instant::now();
        if self.next > now {
            sleep(self.next - now).await;
        }
        self.next = self.next.max(Instant::now()) + self.period;
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn interval_spaces_ticks() {
        let mut iv = interval(Duration::from_millis(20));
        let t0 = Instant::now();
        iv.tick().await;
        assert!(t0.elapsed() < Duration::from_millis(15), "first tick is immediate");
        iv.tick().await;
        iv.tick().await;
        assert!(t0.elapsed() >= Duration::from_millis(40));
        assert_eq!(blocking(|| 6 * 7).await, Some(42));
    }

    #[tokio::test]
    async fn timeout_stops_at_the_deadline() {
        assert_eq!(timeout(Duration::from_secs(5), async { 7 }).await, Ok(7));
        let t0 = Instant::now();
        assert_eq!(timeout(Duration::from_millis(20), std::future::pending::<()>()).await, Err(Elapsed));
        assert!(t0.elapsed() >= Duration::from_millis(20));
    }
}
