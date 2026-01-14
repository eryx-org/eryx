// Mutex poisoning is acceptable here - if a panic occurs while holding the lock,
// the entire pool is in an inconsistent state anyway.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Managed pool of warm sandbox instances for high-throughput scenarios.
//!
//! This module provides a pool of pre-created sandbox instances that can be
//! acquired and released efficiently. The pool maintains a configurable number
//! of warm instances to avoid the overhead of creating new sandboxes for each
//! request.
//!
//! # Features
//!
//! - **Pre-warming**: Maintain minimum idle instances for immediate availability
//! - **Bounded concurrency**: Limit maximum concurrent sandbox usage via semaphore
//! - **Statistics tracking**: Monitor pool usage and performance metrics
//! - **Automatic cleanup**: Evict idle instances after configurable timeout
//! - **State reset**: Optionally clear session state when returning to pool
//!
//! # Example
//!
//! ```rust,ignore
//! use eryx::{Sandbox, SandboxPool, PoolConfig};
//! use std::time::Duration;
//!
//! // Create a pool with custom configuration
//! let config = PoolConfig {
//!     max_size: 10,
//!     min_idle: 2,
//!     idle_timeout: Duration::from_secs(300),
//!     ..Default::default()
//! };
//!
//! let pool = SandboxPool::new(Sandbox::embedded(), config).await?;
//!
//! // Acquire a sandbox from the pool
//! let sandbox = pool.acquire().await?;
//!
//! // Use the sandbox
//! let result = sandbox.execute("print('Hello!')").await?;
//!
//! // Sandbox is automatically returned to pool when dropped
//! drop(sandbox);
//!
//! // Check pool statistics
//! let stats = pool.stats();
//! println!("Total acquisitions: {}", stats.total_acquisitions);
//! ```

use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio::time::timeout;

use crate::sandbox::{Sandbox, SandboxBuilder, state};
use crate::Error;

/// Configuration for a sandbox pool.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Maximum number of sandboxes in the pool.
    ///
    /// This is the upper bound on concurrent sandbox usage. Requests beyond
    /// this limit will block until a sandbox becomes available.
    ///
    /// Default: 10
    pub max_size: usize,

    /// Minimum number of idle sandboxes to maintain.
    ///
    /// The pool will pre-warm this many sandboxes on creation and maintain
    /// at least this many idle instances for immediate availability.
    ///
    /// Default: 1
    pub min_idle: usize,

    /// How long to keep idle sandboxes before evicting them.
    ///
    /// Sandboxes that have been idle longer than this duration may be
    /// evicted to free resources. This only applies to sandboxes beyond
    /// the `min_idle` threshold.
    ///
    /// Default: 300 seconds (5 minutes)
    pub idle_timeout: Duration,

    /// Maximum time to wait for an available sandbox.
    ///
    /// If no sandbox becomes available within this duration, the acquire
    /// operation will fail with a timeout error.
    ///
    /// Default: 30 seconds
    pub acquire_timeout: Duration,

    /// Whether to reset session state when returning a sandbox to the pool.
    ///
    /// When enabled, `InProcessSession::reset()` is called before returning
    /// the sandbox to ensure a clean state for the next user.
    ///
    /// Default: true
    pub reset_on_release: bool,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_size: 10,
            min_idle: 1,
            idle_timeout: Duration::from_secs(300),
            acquire_timeout: Duration::from_secs(30),
            reset_on_release: true,
        }
    }
}

/// Error type for pool operations.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    /// Pool has reached maximum capacity and no sandboxes are available.
    #[error("pool exhausted: all {0} sandboxes are in use")]
    Exhausted(usize),

    /// Timed out waiting for an available sandbox.
    #[error("acquire timeout after {0:?}")]
    Timeout(Duration),

    /// Failed to create a new sandbox instance.
    #[error("sandbox creation failed: {0}")]
    Creation(String),

    /// Failed to reset sandbox state.
    #[error("sandbox reset failed: {0}")]
    Reset(String),

    /// Pool has been shut down.
    #[error("pool is closed")]
    Closed,
}

impl From<Error> for PoolError {
    fn from(err: Error) -> Self {
        PoolError::Creation(err.to_string())
    }
}

/// Statistics about pool usage.
#[derive(Debug, Clone)]
pub struct PoolStats {
    /// Total number of sandboxes (in use + available).
    pub total: usize,

    /// Number of sandboxes currently available in the pool.
    pub available: usize,

    /// Number of sandboxes currently in use.
    pub in_use: usize,

    /// Total number of successful acquisitions since pool creation.
    pub total_acquisitions: u64,

    /// Total number of sandbox creations (initial + on-demand).
    pub total_creations: u64,

    /// Number of times acquisition had to wait for a sandbox.
    pub wait_count: u64,

    /// Cumulative wait time for all acquisitions (for computing average).
    pub total_wait_time: Duration,
}

impl PoolStats {
    /// Calculate the average wait time for acquisitions that had to wait.
    #[must_use]
    pub fn average_wait_time(&self) -> Duration {
        if self.wait_count == 0 {
            Duration::ZERO
        } else {
            self.total_wait_time / self.wait_count as u32
        }
    }
}

/// Internal structure for a pooled sandbox entry.
struct PoolEntry {
    /// The sandbox instance.
    sandbox: Sandbox,
    /// When this sandbox was last returned to the pool.
    last_used: Instant,
}

/// Internal atomic counters for pool statistics.
struct PoolStatsInner {
    total_acquisitions: AtomicU64,
    total_creations: AtomicU64,
    wait_count: AtomicU64,
    total_wait_nanos: AtomicU64,
}

impl PoolStatsInner {
    fn new() -> Self {
        Self {
            total_acquisitions: AtomicU64::new(0),
            total_creations: AtomicU64::new(0),
            wait_count: AtomicU64::new(0),
            total_wait_nanos: AtomicU64::new(0),
        }
    }

    fn record_acquisition(&self) {
        self.total_acquisitions.fetch_add(1, Ordering::Relaxed);
    }

    fn record_creation(&self) {
        self.total_creations.fetch_add(1, Ordering::Relaxed);
    }

    fn record_wait(&self, duration: Duration) {
        self.wait_count.fetch_add(1, Ordering::Relaxed);
        self.total_wait_nanos
            .fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
    }
}

/// A managed pool of warm sandbox instances.
///
/// The pool pre-creates sandbox instances and manages their lifecycle,
/// providing efficient reuse for high-throughput scenarios.
pub struct SandboxPool {
    /// Configuration for this pool.
    config: PoolConfig,

    /// The sandbox builder used to create new instances.
    /// We store the built sandbox as a template and clone its configuration.
    builder_fn: Arc<dyn Fn() -> Result<Sandbox, Error> + Send + Sync>,

    /// Pool of available sandbox instances.
    /// Using std::sync::Mutex to allow synchronous access from Drop.
    pool: Arc<StdMutex<VecDeque<PoolEntry>>>,

    /// Semaphore limiting concurrent sandbox usage.
    semaphore: Arc<Semaphore>,

    /// Atomic statistics counters.
    stats: Arc<PoolStatsInner>,

    /// Current number of sandboxes (in use + available).
    current_size: Arc<AtomicUsize>,

    /// Flag indicating the pool is closed.
    closed: Arc<AtomicUsize>,
}

impl std::fmt::Debug for SandboxPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxPool")
            .field("config", &self.config)
            .field("current_size", &self.current_size.load(Ordering::Relaxed))
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl SandboxPool {
    /// Create a new sandbox pool with the given builder and configuration.
    ///
    /// The builder will be used to create new sandbox instances as needed.
    /// The pool will be pre-warmed with `config.min_idle` instances.
    ///
    /// # Errors
    ///
    /// Returns an error if pre-warming fails (unable to create initial sandboxes).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let pool = SandboxPool::new(
    ///     Sandbox::embedded(),
    ///     PoolConfig::default(),
    /// ).await?;
    /// ```
    pub async fn new(
        _builder: SandboxBuilder<state::Has, state::Has>,
        config: PoolConfig,
    ) -> Result<Self, PoolError> {
        // Validate configuration
        if config.max_size == 0 {
            return Err(PoolError::Creation(
                "max_size must be greater than 0".to_string(),
            ));
        }
        if config.min_idle > config.max_size {
            return Err(PoolError::Creation(
                "min_idle cannot exceed max_size".to_string(),
            ));
        }

        // Create a builder function that can create sandboxes on demand.
        // We build one sandbox first to validate the configuration, then
        // create a closure that rebuilds sandboxes with the same settings.
        //
        // Since SandboxBuilder consumes self on build(), we need to store
        // enough information to recreate sandboxes. The embedded() builder
        // is stateless, so we can just call it again.
        let builder_fn: Arc<dyn Fn() -> Result<Sandbox, Error> + Send + Sync> =
            Arc::new(move || {
                // For now, we only support the embedded builder pattern.
                // This creates a fresh embedded sandbox each time.
                #[cfg(feature = "embedded")]
                {
                    Sandbox::embedded().build()
                }
                #[cfg(not(feature = "embedded"))]
                {
                    Err(Error::Initialization(
                        "Pool requires embedded feature".to_string(),
                    ))
                }
            });

        let pool = Arc::new(StdMutex::new(VecDeque::with_capacity(config.max_size)));
        let semaphore = Arc::new(Semaphore::new(config.max_size));
        let stats = Arc::new(PoolStatsInner::new());
        let current_size = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));

        let pool_instance = Self {
            config: config.clone(),
            builder_fn,
            pool,
            semaphore,
            stats,
            current_size,
            closed,
        };

        // Pre-warm the pool with min_idle instances
        pool_instance.prewarm(config.min_idle)?;

        Ok(pool_instance)
    }

    /// Create a new sandbox pool with a custom builder function.
    ///
    /// This allows creating pools with custom sandbox configurations that
    /// go beyond what `Sandbox::embedded()` provides.
    ///
    /// # Arguments
    ///
    /// * `builder_fn` - A function that creates new sandbox instances
    /// * `config` - Pool configuration
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let pool = SandboxPool::with_builder(
    ///     || {
    ///         Sandbox::embedded()
    ///             .with_callback(MyCallback)
    ///             .with_resource_limits(limits)
    ///             .build()
    ///     },
    ///     PoolConfig::default(),
    /// ).await?;
    /// ```
    pub async fn with_builder<F>(builder_fn: F, config: PoolConfig) -> Result<Self, PoolError>
    where
        F: Fn() -> Result<Sandbox, Error> + Send + Sync + 'static,
    {
        // Validate configuration
        if config.max_size == 0 {
            return Err(PoolError::Creation(
                "max_size must be greater than 0".to_string(),
            ));
        }
        if config.min_idle > config.max_size {
            return Err(PoolError::Creation(
                "min_idle cannot exceed max_size".to_string(),
            ));
        }

        let builder_fn: Arc<dyn Fn() -> Result<Sandbox, Error> + Send + Sync> =
            Arc::new(builder_fn);

        let pool = Arc::new(StdMutex::new(VecDeque::with_capacity(config.max_size)));
        let semaphore = Arc::new(Semaphore::new(config.max_size));
        let stats = Arc::new(PoolStatsInner::new());
        let current_size = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));

        let pool_instance = Self {
            config: config.clone(),
            builder_fn,
            pool,
            semaphore,
            stats,
            current_size,
            closed,
        };

        // Pre-warm the pool with min_idle instances
        pool_instance.prewarm(config.min_idle)?;

        Ok(pool_instance)
    }

    /// Pre-warm the pool with the specified number of instances.
    fn prewarm(&self, count: usize) -> Result<(), PoolError> {
        for _ in 0..count {
            let sandbox = (self.builder_fn)().map_err(PoolError::from)?;
            self.stats.record_creation();
            self.current_size.fetch_add(1, Ordering::Relaxed);

            let mut pool = self.pool.lock().unwrap();
            pool.push_back(PoolEntry {
                sandbox,
                last_used: Instant::now(),
            });
        }
        Ok(())
    }

    /// Acquire a sandbox from the pool.
    ///
    /// This method blocks until a sandbox is available or the acquire timeout
    /// is reached. The returned `PooledSandbox` will automatically return the
    /// sandbox to the pool when dropped.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The pool is closed
    /// - The acquire timeout is reached
    /// - A new sandbox cannot be created when the pool is not at capacity
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let sandbox = pool.acquire().await?;
    /// let result = sandbox.execute("print('Hello!')").await?;
    /// // Sandbox automatically returned when `sandbox` is dropped
    /// ```
    pub async fn acquire(&self) -> Result<PooledSandbox, PoolError> {
        self.acquire_timeout(self.config.acquire_timeout).await
    }

    /// Acquire a sandbox from the pool with a custom timeout.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The pool is closed
    /// - The specified timeout is reached
    /// - A new sandbox cannot be created when the pool is not at capacity
    pub async fn acquire_timeout(&self, acquire_timeout: Duration) -> Result<PooledSandbox, PoolError> {
        // Check if pool is closed
        if self.closed.load(Ordering::Relaxed) != 0 {
            return Err(PoolError::Closed);
        }

        let start = Instant::now();

        // Try to acquire a semaphore permit with timeout
        let permit = match timeout(acquire_timeout, self.semaphore.clone().acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(PoolError::Closed), // Semaphore closed
            Err(_) => {
                return Err(PoolError::Timeout(acquire_timeout));
            }
        };

        let wait_time = start.elapsed();
        if wait_time > Duration::from_millis(1) {
            self.stats.record_wait(wait_time);
        }

        // Try to get an existing sandbox from the pool
        let sandbox = {
            let mut pool = self.pool.lock().unwrap();
            pool.pop_front().map(|entry| entry.sandbox)
        };

        let sandbox = match sandbox {
            Some(s) => s,
            None => {
                // No sandbox available, create a new one
                let s = (self.builder_fn)().map_err(PoolError::from)?;
                self.stats.record_creation();
                self.current_size.fetch_add(1, Ordering::Relaxed);
                s
            }
        };

        self.stats.record_acquisition();

        Ok(PooledSandbox {
            sandbox: Some(sandbox),
            pool: Arc::new(PoolHandle {
                pool: Arc::clone(&self.pool),
                current_size: Arc::clone(&self.current_size),
                closed: Arc::clone(&self.closed),
            }),
            _permit: permit,
        })
    }

    /// Try to acquire a sandbox without blocking.
    ///
    /// Returns `None` if no sandbox is immediately available.
    ///
    /// # Errors
    ///
    /// Returns an error if the pool is closed or sandbox creation fails.
    pub fn try_acquire(&self) -> Result<Option<PooledSandbox>, PoolError> {
        // Check if pool is closed
        if self.closed.load(Ordering::Relaxed) != 0 {
            return Err(PoolError::Closed);
        }

        // Try to acquire a semaphore permit without blocking
        let permit = match self.semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => return Ok(None), // No permit available
        };

        // Try to get an existing sandbox from the pool
        let sandbox = {
            let mut pool = self.pool.lock().unwrap();
            pool.pop_front().map(|entry| entry.sandbox)
        };

        let sandbox = match sandbox {
            Some(s) => s,
            None => {
                // No sandbox available, create a new one
                let s = (self.builder_fn)().map_err(PoolError::from)?;
                self.stats.record_creation();
                self.current_size.fetch_add(1, Ordering::Relaxed);
                s
            }
        };

        self.stats.record_acquisition();

        Ok(Some(PooledSandbox {
            sandbox: Some(sandbox),
            pool: Arc::new(PoolHandle {
                pool: Arc::clone(&self.pool),
                current_size: Arc::clone(&self.current_size),
                closed: Arc::clone(&self.closed),
            }),
            _permit: permit,
        }))
    }

    /// Get current pool statistics.
    #[must_use]
    pub fn stats(&self) -> PoolStats {
        let current_size = self.current_size.load(Ordering::Relaxed);
        let available = self.semaphore.available_permits();
        let in_use = self.config.max_size - available;

        PoolStats {
            total: current_size,
            available,
            in_use,
            total_acquisitions: self.stats.total_acquisitions.load(Ordering::Relaxed),
            total_creations: self.stats.total_creations.load(Ordering::Relaxed),
            wait_count: self.stats.wait_count.load(Ordering::Relaxed),
            total_wait_time: Duration::from_nanos(
                self.stats.total_wait_nanos.load(Ordering::Relaxed),
            ),
        }
    }

    /// Get the pool configuration.
    #[must_use]
    pub fn config(&self) -> &PoolConfig {
        &self.config
    }

    /// Close the pool, preventing new acquisitions.
    ///
    /// Existing `PooledSandbox` instances will continue to work, but they
    /// will not be returned to the pool when dropped.
    pub fn close(&self) {
        self.closed.store(1, Ordering::Relaxed);
    }

    /// Check if the pool is closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed) != 0
    }

    /// Evict idle sandboxes that have exceeded the idle timeout.
    ///
    /// This method removes sandboxes that have been idle longer than
    /// `config.idle_timeout`, but maintains at least `config.min_idle`
    /// instances in the pool.
    ///
    /// Returns the number of sandboxes evicted.
    pub fn evict_idle(&self) -> usize {
        let mut pool = self.pool.lock().unwrap();
        let now = Instant::now();
        let min_idle = self.config.min_idle;
        let idle_timeout = self.config.idle_timeout;

        let mut evicted = 0;
        while pool.len() > min_idle {
            // Check the oldest entry (front of the queue)
            if let Some(entry) = pool.front() {
                if now.duration_since(entry.last_used) > idle_timeout {
                    pool.pop_front();
                    self.current_size.fetch_sub(1, Ordering::Relaxed);
                    evicted += 1;
                } else {
                    // If the oldest isn't expired, none of the newer ones are
                    break;
                }
            } else {
                break;
            }
        }

        evicted
    }
}

/// Handle for returning sandboxes to the pool.
struct PoolHandle {
    pool: Arc<StdMutex<VecDeque<PoolEntry>>>,
    current_size: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
}

impl PoolHandle {
    /// Return a sandbox to the pool.
    fn return_sandbox(&self, sandbox: Sandbox) {
        // Don't return to pool if closed
        if self.closed.load(Ordering::Relaxed) != 0 {
            self.current_size.fetch_sub(1, Ordering::Relaxed);
            return;
        }

        // Return to pool using std::sync::Mutex (safe in Drop context)
        let mut pool = self.pool.lock().unwrap();
        pool.push_back(PoolEntry {
            sandbox,
            last_used: Instant::now(),
        });
    }
}

/// A sandbox acquired from a pool.
///
/// This wrapper ensures the sandbox is returned to the pool when dropped.
/// It implements `Deref` and `DerefMut` to allow transparent access to the
/// underlying `Sandbox`.
pub struct PooledSandbox {
    /// The sandbox instance (Option for taking during drop).
    sandbox: Option<Sandbox>,
    /// Handle for returning the sandbox to the pool.
    pool: Arc<PoolHandle>,
    /// Semaphore permit (released when dropped).
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl std::fmt::Debug for PooledSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PooledSandbox")
            .field("sandbox", &self.sandbox)
            .finish_non_exhaustive()
    }
}

impl Deref for PooledSandbox {
    type Target = Sandbox;

    fn deref(&self) -> &Self::Target {
        self.sandbox.as_ref().expect("sandbox taken during drop")
    }
}

impl DerefMut for PooledSandbox {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.sandbox.as_mut().expect("sandbox taken during drop")
    }
}

impl Drop for PooledSandbox {
    fn drop(&mut self) {
        if let Some(sandbox) = self.sandbox.take() {
            // Note: We can't do async reset here since Drop is sync.
            // If reset_on_release is needed, users should call reset() manually
            // before dropping, or use a wrapper that handles this.
            self.pool.return_sandbox(sandbox);
        }
        // The semaphore permit is automatically released when _permit is dropped
    }
}

impl PooledSandbox {
    /// Get a reference to the underlying sandbox.
    #[must_use]
    pub fn sandbox(&self) -> &Sandbox {
        self.sandbox.as_ref().expect("sandbox taken during drop")
    }

    /// Get a mutable reference to the underlying sandbox.
    #[must_use]
    pub fn sandbox_mut(&mut self) -> &mut Sandbox {
        self.sandbox.as_mut().expect("sandbox taken during drop")
    }

    /// Detach the sandbox from the pool, preventing it from being returned.
    ///
    /// This consumes the `PooledSandbox` and returns the underlying `Sandbox`.
    /// The sandbox will not be returned to the pool.
    ///
    /// Note: This still releases the semaphore permit, so another sandbox
    /// can be created to take its place in the pool.
    #[must_use]
    pub fn detach(mut self) -> Sandbox {
        self.sandbox.take().expect("sandbox already taken")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn pool_config_default() {
        let config = PoolConfig::default();
        assert_eq!(config.max_size, 10);
        assert_eq!(config.min_idle, 1);
        assert_eq!(config.idle_timeout, Duration::from_secs(300));
        assert_eq!(config.acquire_timeout, Duration::from_secs(30));
        assert!(config.reset_on_release);
    }

    #[test]
    fn pool_stats_average_wait_time_zero_waits() {
        let stats = PoolStats {
            total: 5,
            available: 3,
            in_use: 2,
            total_acquisitions: 100,
            total_creations: 5,
            wait_count: 0,
            total_wait_time: Duration::ZERO,
        };
        assert_eq!(stats.average_wait_time(), Duration::ZERO);
    }

    #[test]
    fn pool_stats_average_wait_time_with_waits() {
        let stats = PoolStats {
            total: 5,
            available: 3,
            in_use: 2,
            total_acquisitions: 100,
            total_creations: 5,
            wait_count: 10,
            total_wait_time: Duration::from_millis(1000),
        };
        assert_eq!(stats.average_wait_time(), Duration::from_millis(100));
    }

    #[test]
    fn pool_error_display() {
        let err = PoolError::Exhausted(10);
        assert!(err.to_string().contains("10"));

        let err = PoolError::Timeout(Duration::from_secs(5));
        assert!(err.to_string().contains("5s"));

        let err = PoolError::Creation("test error".to_string());
        assert!(err.to_string().contains("test error"));

        let err = PoolError::Closed;
        assert!(err.to_string().contains("closed"));
    }

    #[cfg(feature = "embedded")]
    mod embedded_tests {
        use super::*;

        #[tokio::test]
        async fn pool_new_with_defaults() {
            let config = PoolConfig {
                min_idle: 1,
                max_size: 3,
                ..Default::default()
            };

            let pool = SandboxPool::new(Sandbox::embedded(), config).await;
            assert!(pool.is_ok(), "Failed to create pool: {:?}", pool.err());

            let pool = pool.unwrap();
            let stats = pool.stats();
            assert_eq!(stats.total, 1); // Pre-warmed with min_idle
            assert_eq!(stats.total_creations, 1);
        }

        #[tokio::test]
        async fn pool_acquire_and_release() {
            let config = PoolConfig {
                min_idle: 1,
                max_size: 3,
                ..Default::default()
            };

            let pool = SandboxPool::new(Sandbox::embedded(), config)
                .await
                .expect("Failed to create pool");

            // Acquire a sandbox
            let sandbox = pool.acquire().await.expect("Failed to acquire sandbox");

            let stats = pool.stats();
            assert_eq!(stats.total_acquisitions, 1);
            assert_eq!(stats.in_use, 1);

            // Release it
            drop(sandbox);

            let stats = pool.stats();
            assert_eq!(stats.in_use, 0);
            assert_eq!(stats.available, 3); // Permit released
        }

        #[tokio::test]
        async fn pool_execute_code() {
            let config = PoolConfig {
                min_idle: 1,
                max_size: 2,
                ..Default::default()
            };

            let pool = SandboxPool::new(Sandbox::embedded(), config)
                .await
                .expect("Failed to create pool");

            let sandbox = pool.acquire().await.expect("Failed to acquire sandbox");
            let result = sandbox.execute("print('Hello from pool!')").await;
            assert!(result.is_ok(), "Execution failed: {:?}", result.err());

            let output = result.unwrap();
            assert_eq!(output.stdout.trim(), "Hello from pool!");
        }

        #[tokio::test]
        async fn pool_multiple_acquisitions() {
            let config = PoolConfig {
                min_idle: 2,
                max_size: 3,
                ..Default::default()
            };

            let pool = SandboxPool::new(Sandbox::embedded(), config)
                .await
                .expect("Failed to create pool");

            // Acquire all sandboxes
            let s1 = pool.acquire().await.expect("Failed to acquire sandbox 1");
            let s2 = pool.acquire().await.expect("Failed to acquire sandbox 2");
            let s3 = pool.acquire().await.expect("Failed to acquire sandbox 3");

            let stats = pool.stats();
            assert_eq!(stats.in_use, 3);
            assert_eq!(stats.total_acquisitions, 3);

            // Release them
            drop(s1);
            drop(s2);
            drop(s3);

            let stats = pool.stats();
            assert_eq!(stats.in_use, 0);
        }

        #[tokio::test]
        async fn pool_timeout_on_exhaustion() {
            let config = PoolConfig {
                min_idle: 1,
                max_size: 1,
                acquire_timeout: Duration::from_millis(100),
                ..Default::default()
            };

            let pool = SandboxPool::new(Sandbox::embedded(), config)
                .await
                .expect("Failed to create pool");

            // Acquire the only sandbox
            let _s1 = pool.acquire().await.expect("Failed to acquire sandbox");

            // Try to acquire another - should timeout
            let result = pool.acquire().await;
            assert!(matches!(result, Err(PoolError::Timeout(_))));
        }

        #[tokio::test]
        async fn pool_with_custom_builder() {
            let config = PoolConfig {
                min_idle: 1,
                max_size: 2,
                ..Default::default()
            };

            let pool = SandboxPool::with_builder(
                || Sandbox::embedded().build(),
                config,
            )
            .await
            .expect("Failed to create pool");

            let sandbox = pool.acquire().await.expect("Failed to acquire sandbox");
            let result = sandbox.execute("print(2 + 2)").await;
            assert!(result.is_ok());
            assert_eq!(result.unwrap().stdout.trim(), "4");
        }

        #[tokio::test]
        async fn pool_close_prevents_new_acquisitions() {
            let config = PoolConfig {
                min_idle: 1,
                max_size: 2,
                ..Default::default()
            };

            let pool = SandboxPool::new(Sandbox::embedded(), config)
                .await
                .expect("Failed to create pool");

            // Close the pool
            pool.close();
            assert!(pool.is_closed());

            // Try to acquire - should fail
            let result = pool.acquire().await;
            assert!(matches!(result, Err(PoolError::Closed)));
        }

        #[tokio::test]
        async fn pool_detach_sandbox() {
            let config = PoolConfig {
                min_idle: 1,
                max_size: 2,
                ..Default::default()
            };

            let pool = SandboxPool::new(Sandbox::embedded(), config)
                .await
                .expect("Failed to create pool");

            let pooled = pool.acquire().await.expect("Failed to acquire sandbox");
            let sandbox = pooled.detach();

            // Sandbox is now independent
            let result = sandbox.execute("print('detached')").await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn pool_try_acquire() {
            let config = PoolConfig {
                min_idle: 1,
                max_size: 1,
                ..Default::default()
            };

            let pool = SandboxPool::new(Sandbox::embedded(), config)
                .await
                .expect("Failed to create pool");

            // First try_acquire should succeed
            let s1 = pool.try_acquire().expect("try_acquire failed");
            assert!(s1.is_some());
            let _s1 = s1.unwrap();

            // Second try_acquire should return None (pool exhausted)
            let s2 = pool.try_acquire().expect("try_acquire failed");
            assert!(s2.is_none());
        }

        #[tokio::test]
        async fn pool_evict_idle() {
            let config = PoolConfig {
                min_idle: 1,
                max_size: 3,
                idle_timeout: Duration::from_millis(10),
                ..Default::default()
            };

            let pool = SandboxPool::new(Sandbox::embedded(), config)
                .await
                .expect("Failed to create pool");

            // Create more sandboxes
            {
                let s1 = pool.acquire().await.unwrap();
                let s2 = pool.acquire().await.unwrap();
                drop(s1);
                drop(s2);
            }

            // Wait for idle timeout
            tokio::time::sleep(Duration::from_millis(50)).await;

            // Evict idle sandboxes
            let evicted = pool.evict_idle();
            // Should evict down to min_idle (1)
            assert!(evicted >= 1, "Expected at least 1 eviction, got {}", evicted);
        }
    }
}
