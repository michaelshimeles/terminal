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

/// Resolves after `d`, through `setTimeout`.
#[cfg(target_arch = "wasm32")]
pub async fn sleep(d: Duration) {
    let ms = d.as_millis().min(i32::MAX as u128) as i32;
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(w) = web_sys::window() {
            let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
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
}
