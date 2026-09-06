//! TCP traffic shaping: bandwidth capping, per-chunk jitter, and a
//! configurable copy chunk size, ported from the original `redir`'s
//! `-m`/`-o`/`-w`/`-z` flags. `RedirectConfig::shaping()` (see `config.rs`)
//! returns `None` when none of the four fields differ from their defaults,
//! so callers can skip this module entirely and use a plain fast copy for
//! the common unshaped case. When it returns `Some`, `bufsize` always
//! controls the copy loop's chunk size; the bandwidth cap and jitter are
//! independently optional on top of that (see `shaped_copy_blocking`/
//! `shaped_copy_async`).

use std::time::{Duration, Instant};

use crate::plugin::Direction;

/// Which direction(s) of a connection shaping applies to. Named after the
/// original `redir`'s `-o`/`--wait-in-out` flag: "in" is client-to-target
/// traffic flowing in to the target, "out" is target-to-client traffic
/// flowing back out to the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum WaitInOut {
    In,
    Out,
    #[default]
    Both,
}

impl WaitInOut {
    pub fn applies_to(self, direction: Direction) -> bool {
        match (self, direction) {
            (WaitInOut::Both, _) => true,
            (WaitInOut::In, Direction::ClientToTarget) => true,
            (WaitInOut::Out, Direction::TargetToClient) => true,
            _ => false,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            WaitInOut::In => "in",
            WaitInOut::Out => "out",
            WaitInOut::Both => "both",
        }
    }
}

impl std::str::FromStr for WaitInOut {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "in" => Ok(WaitInOut::In),
            "out" => Ok(WaitInOut::Out),
            "both" => Ok(WaitInOut::Both),
            other => Err(format!(
                "invalid wait-in-out value {other:?} (expected in, out, or both)"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ShapingConfig {
    /// Bits per second; `None` means no bandwidth cap.
    pub max_bandwidth_bps: Option<u64>,
    pub wait_in_out: WaitInOut,
    /// Upper bound (milliseconds) of a random per-chunk delay; `None` means
    /// no jitter.
    pub random_wait_ms: Option<u64>,
    /// Read/write chunk size shaping is applied at. Smaller values apply
    /// shaping more finely (closer to true byte-level pacing) at the cost
    /// of more syscalls/sleeps; larger values are coarser but cheaper.
    pub bufsize: usize,
}

impl ShapingConfig {
    pub fn is_active(&self) -> bool {
        self.max_bandwidth_bps.is_some() || self.random_wait_ms.is_some()
    }
}

/// Tracks bytes sent against a bits/sec cap and reports how long the caller
/// should sleep before the next chunk to keep the average rate under it.
pub struct RateLimiter {
    bits_per_sec: u64,
    window_start: Instant,
    bytes_sent: u64,
}

impl RateLimiter {
    pub fn new(bits_per_sec: u64) -> Self {
        Self {
            bits_per_sec,
            window_start: Instant::now(),
            bytes_sent: 0,
        }
    }

    /// Records `n` more bytes sent and returns how long to sleep so the
    /// average rate since this limiter's start (or last reset) doesn't
    /// exceed the configured cap.
    pub fn wait_for(&mut self, n: usize) -> Duration {
        self.bytes_sent += n as u64;
        let expected_secs = (self.bytes_sent as f64 * 8.0) / self.bits_per_sec as f64;
        let expected = Duration::from_secs_f64(expected_secs);
        let elapsed = self.window_start.elapsed();

        // Reset periodically so long-lived connections don't let
        // `bytes_sent` (and the float precision of `expected_secs`) grow
        // unbounded.
        if elapsed > Duration::from_secs(10) {
            self.window_start = Instant::now();
            self.bytes_sent = 0;
        }

        expected.saturating_sub(elapsed)
    }
}

/// Tiny self-contained xorshift64 PRNG: good enough for jitter, and avoids
/// pulling in the `rand` crate for this one call site.
pub struct Jitter(u64);

impl Jitter {
    pub fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15)
            | 1; // xorshift requires a nonzero state
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A random duration in `[0, max_ms]` milliseconds.
    pub fn random_wait(&mut self, max_ms: u64) -> Duration {
        if max_ms == 0 {
            return Duration::ZERO;
        }
        Duration::from_millis(self.next_u64() % (max_ms + 1))
    }
}

impl Default for Jitter {
    fn default() -> Self {
        Self::new()
    }
}

/// Blocking chunked copy, used by the Unix per-connection worker
/// (`conn_worker.rs`), which copies with plain blocking I/O on its own
/// thread. Falls back to a plain `std::io::copy` when `shaping` is `None`
/// entirely. When present, `shaping.bufsize` always controls the chunk
/// size (even with no bandwidth cap or jitter configured); the rate-limit
/// and jitter sleeps only apply when configured *and* `wait_in_out`
/// selects `direction`.
pub fn shaped_copy_blocking<R: std::io::Read, W: std::io::Write>(
    reader: &mut R,
    writer: &mut W,
    direction: Direction,
    shaping: Option<&ShapingConfig>,
) -> std::io::Result<u64> {
    let Some(shaping) = shaping else {
        return std::io::copy(reader, writer);
    };
    let applies = shaping.wait_in_out.applies_to(direction);

    let mut buf = vec![0u8; shaping.bufsize.max(1)];
    let mut limiter = applies
        .then_some(shaping.max_bandwidth_bps)
        .flatten()
        .map(RateLimiter::new);
    let mut jitter = applies
        .then_some(shaping.random_wait_ms)
        .flatten()
        .map(|_| Jitter::new());
    let mut total = 0u64;

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        total += n as u64;

        if let Some(limiter) = &mut limiter {
            std::thread::sleep(limiter.wait_for(n));
        }
        if let (Some(jitter), Some(max_ms)) = (&mut jitter, shaping.random_wait_ms) {
            std::thread::sleep(jitter.random_wait(max_ms));
        }
    }
    Ok(total)
}

/// Async equivalent of `shaped_copy_blocking`, used by the non-Unix
/// fallback path (`proxy.rs`'s `forward`), which copies with tokio's async
/// I/O directly in-process rather than through a child worker.
pub async fn shaped_copy_async<R, W>(
    reader: &mut R,
    writer: &mut W,
    direction: Direction,
    shaping: Option<&ShapingConfig>,
) -> std::io::Result<u64>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let Some(shaping) = shaping else {
        return tokio::io::copy(reader, writer).await;
    };
    let applies = shaping.wait_in_out.applies_to(direction);

    let mut buf = vec![0u8; shaping.bufsize.max(1)];
    let mut limiter = applies
        .then_some(shaping.max_bandwidth_bps)
        .flatten()
        .map(RateLimiter::new);
    let mut jitter = applies
        .then_some(shaping.random_wait_ms)
        .flatten()
        .map(|_| Jitter::new());
    let mut total = 0u64;

    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n]).await?;
        total += n as u64;

        if let Some(limiter) = &mut limiter {
            tokio::time::sleep(limiter.wait_for(n)).await;
        }
        if let (Some(jitter), Some(max_ms)) = (&mut jitter, shaping.random_wait_ms) {
            tokio::time::sleep(jitter.random_wait(max_ms)).await;
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_in_out_direction_matching() {
        assert!(WaitInOut::Both.applies_to(Direction::ClientToTarget));
        assert!(WaitInOut::Both.applies_to(Direction::TargetToClient));
        assert!(WaitInOut::In.applies_to(Direction::ClientToTarget));
        assert!(!WaitInOut::In.applies_to(Direction::TargetToClient));
        assert!(WaitInOut::Out.applies_to(Direction::TargetToClient));
        assert!(!WaitInOut::Out.applies_to(Direction::ClientToTarget));
    }

    #[test]
    fn wait_in_out_from_str_roundtrips() {
        for v in [WaitInOut::In, WaitInOut::Out, WaitInOut::Both] {
            assert_eq!(v.as_str().parse::<WaitInOut>().unwrap(), v);
        }
        assert!("bogus".parse::<WaitInOut>().is_err());
    }

    #[test]
    fn rate_limiter_requires_no_wait_when_under_cap() {
        // 1000 bytes/sec cap == 8000 bits/sec; sending 1 byte should expect
        // ~1ms, which is less than the near-zero elapsed time in a test, so
        // *some* positive wait is expected -- but sending 0 bytes should
        // never demand a wait.
        let mut limiter = RateLimiter::new(8_000);
        assert_eq!(limiter.wait_for(0), Duration::ZERO);
    }

    #[test]
    fn rate_limiter_demands_wait_after_burst() {
        // 800 bits/sec == 100 bytes/sec. Sending 100 bytes instantly should
        // demand close to a 1 second wait to stay under the cap.
        let mut limiter = RateLimiter::new(800);
        let wait = limiter.wait_for(100);
        assert!(
            wait > Duration::from_millis(900),
            "expected ~1s wait, got {wait:?}"
        );
    }

    #[test]
    fn jitter_stays_within_bound() {
        let mut jitter = Jitter::new();
        for _ in 0..100 {
            let wait = jitter.random_wait(50);
            assert!(wait <= Duration::from_millis(50));
        }
    }

    #[test]
    fn jitter_zero_bound_is_always_zero() {
        let mut jitter = Jitter::new();
        assert_eq!(jitter.random_wait(0), Duration::ZERO);
    }

    #[tokio::test]
    async fn shaped_copy_async_matches_plain_copy_when_inactive() {
        let data = b"hello world";
        let mut reader: &[u8] = data;
        let mut writer = Vec::new();
        let n = shaped_copy_async(&mut reader, &mut writer, Direction::ClientToTarget, None)
            .await
            .unwrap();
        assert_eq!(n, data.len() as u64);
        assert_eq!(writer, data);
    }

    #[test]
    fn shaped_copy_blocking_matches_plain_copy_when_inactive() {
        let data = b"hello world";
        let mut reader: &[u8] = data;
        let mut writer = Vec::new();
        let n = shaped_copy_blocking(&mut reader, &mut writer, Direction::ClientToTarget, None)
            .unwrap();
        assert_eq!(n, data.len() as u64);
        assert_eq!(writer, data);
    }

    /// A bufsize-only config (no bandwidth cap, no jitter) must still copy
    /// correctly, exercising the chunked loop instead of the plain
    /// `std::io::copy` fallback -- regression test for the bug where
    /// `bufsize` alone was silently ignored.
    #[test]
    fn shaped_copy_blocking_applies_custom_bufsize_with_no_rate_or_jitter() {
        let data = vec![0xABu8; 5000]; // several chunks at a small bufsize
        let mut reader: &[u8] = &data;
        let mut writer = Vec::new();
        let shaping = ShapingConfig {
            max_bandwidth_bps: None,
            wait_in_out: WaitInOut::Both,
            random_wait_ms: None,
            bufsize: 128,
        };
        let n = shaped_copy_blocking(
            &mut reader,
            &mut writer,
            Direction::ClientToTarget,
            Some(&shaping),
        )
        .unwrap();
        assert_eq!(n, data.len() as u64);
        assert_eq!(writer, data);
    }
}
