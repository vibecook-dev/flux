//! A small async token bucket. TMDB's documented ceiling is "somewhere in the
//! 40 requests per second range" (research/05 §2.1); flux defaults to 20 rps so
//! a whole household of flux servers behind one NAT stays well below it.

use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

/// Token-bucket rate limiter shared by all requests of one client.
#[derive(Debug)]
pub struct RateLimiter {
    rate: f64,
    capacity: f64,
    state: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    /// `rate` requests per second with bursts of up to `burst` requests.
    /// Non-positive values are clamped to a minimum of 0.1 rps / burst 1.
    pub fn new(rate: f64, burst: u32) -> Self {
        let rate = if rate.is_finite() && rate > 0.1 { rate } else { 0.1 };
        let capacity = f64::from(burst.max(1));
        RateLimiter {
            rate,
            capacity,
            state: Mutex::new(Bucket {
                tokens: capacity,
                last: Instant::now(),
            }),
        }
    }

    /// Wait until a request may be sent.
    pub async fn acquire(&self) {
        loop {
            let wait = {
                let mut b = self.state.lock().await;
                let now = Instant::now();
                let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
                b.tokens = (b.tokens + elapsed * self.rate).min(self.capacity);
                b.last = now;
                if b.tokens >= 1.0 {
                    b.tokens -= 1.0;
                    return;
                }
                Duration::from_secs_f64((1.0 - b.tokens) / self.rate)
            };
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn enforces_rate_after_burst() {
        let rl = RateLimiter::new(10.0, 5);
        let start = Instant::now();
        for _ in 0..25 {
            rl.acquire().await;
        }
        // 5 burst tokens, then 20 more at 10/s = 2 s (virtual time).
        let took = start.elapsed().as_secs_f64();
        assert!((1.9..2.2).contains(&took), "took {took}");
    }
}
