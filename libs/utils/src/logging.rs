use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use metrics::{IntCounter, IntCounterVec};
use once_cell::sync::Lazy;
use strum_macros::{EnumString, VariantNames};
use tokio::time::Instant;
use tracing::{info, warn};

/// Logs a critical error, similarly to `tracing::error!`. This will:
///
/// * Emit an ERROR log message with prefix "CRITICAL:" and a backtrace.
/// * Trigger a pageable alert (via the metric below).
/// * Increment libmetrics_tracing_event_count{level="critical"}, and indirectly level="error".
/// * In debug builds, panic the process.
///
/// When including errors in the message, please use {err:?} to include the error cause and original
/// backtrace.
#[macro_export]
macro_rules! critical {
    ($($arg:tt)*) => {{
        if cfg!(debug_assertions) {
            panic!($($arg)*);
        }
        // Increment both metrics
        $crate::logging::TRACING_EVENT_COUNT_METRIC.inc_critical();
        let backtrace = std::backtrace::Backtrace::capture();
        tracing::error!("CRITICAL: {}\n{backtrace}", format!($($arg)*));
    }};
}

#[macro_export]
macro_rules! critical_timeline {
    ($tenant_shard_id:expr, $timeline_id:expr, $corruption_detected:expr, $($arg:tt)*) => {{
        if cfg!(debug_assertions) {
            panic!($($arg)*);
        }
        // Increment both metrics
        $crate::logging::TRACING_EVENT_COUNT_METRIC.inc_critical();
        $crate::logging::HADRON_CRITICAL_STORAGE_EVENT_COUNT_METRIC.inc(&$tenant_shard_id.to_string(), &$timeline_id.to_string());
        if let Some(c) = $corruption_detected.as_ref() {
            c.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let backtrace = std::backtrace::Backtrace::capture();
        tracing::error!("CRITICAL: [tenant_shard_id: {}, timeline_id: {}] {}\n{backtrace}",
                       $tenant_shard_id, $timeline_id, format!($($arg)*));
    }};
}

#[derive(EnumString, strum_macros::Display, VariantNames, Eq, PartialEq, Debug, Clone, Copy)]
#[strum(serialize_all = "snake_case")]
pub enum LogFormat {
    Plain,
    Json,
    Test,
}

impl LogFormat {
    pub fn from_config(s: &str) -> anyhow::Result<LogFormat> {
        use strum::VariantNames;
        LogFormat::from_str(s).with_context(|| {
            format!(
                "Unrecognized log format. Please specify one of: {:?}",
                LogFormat::VARIANTS
            )
        })
    }
}

pub struct TracingEventCountMetric {
    /// CRITICAL is not a `tracing` log level. Instead, we increment it in the `critical!` macro,
    /// and also emit it as a regular error. These are thus double-counted, but that seems fine.
    critical: IntCounter,
    error: IntCounter,
    warn: IntCounter,
    info: IntCounter,
    debug: IntCounter,
    trace: IntCounter,
}

// Begin Hadron: Add a HadronCriticalStorageEventCountMetric metric that is sliced by tenant_id and timeline_id
pub struct HadronCriticalStorageEventCountMetric {
    critical: IntCounterVec,
}

pub static HADRON_CRITICAL_STORAGE_EVENT_COUNT_METRIC: Lazy<HadronCriticalStorageEventCountMetric> =
    Lazy::new(|| {
        let vec = metrics::register_int_counter_vec!(
            "hadron_critical_storage_event_count",
            "Number of critical storage events, by tenant_id and timeline_id",
            &["tenant_shard_id", "timeline_id"]
        )
        .expect("failed to define metric");
        HadronCriticalStorageEventCountMetric::new(vec)
    });

impl HadronCriticalStorageEventCountMetric {
    fn new(vec: IntCounterVec) -> Self {
        Self { critical: vec }
    }

    // Allow public access from `critical!` macro.
    pub fn inc(&self, tenant_shard_id: &str, timeline_id: &str) {
        self.critical
            .with_label_values(&[tenant_shard_id, timeline_id])
            .inc();
    }
}
// End Hadron

pub static TRACING_EVENT_COUNT_METRIC: Lazy<TracingEventCountMetric> = Lazy::new(|| {
    let vec = metrics::register_int_counter_vec!(
        "libmetrics_tracing_event_count",
        "Number of tracing events, by level",
        &["level"]
    )
    .expect("failed to define metric");
    TracingEventCountMetric::new(vec)
});

impl TracingEventCountMetric {
    fn new(vec: IntCounterVec) -> Self {
        Self {
            critical: vec.with_label_values(&["critical"]),
            error: vec.with_label_values(&["error"]),
            warn: vec.with_label_values(&["warn"]),
            info: vec.with_label_values(&["info"]),
            debug: vec.with_label_values(&["debug"]),
            trace: vec.with_label_values(&["trace"]),
        }
    }

    // Allow public access from `critical!` macro.
    pub fn inc_critical(&self) {
        self.critical.inc();
    }

    fn inc_for_level(&self, level: tracing::Level) {
        let counter = match level {
            tracing::Level::ERROR => &self.error,
            tracing::Level::WARN => &self.warn,
            tracing::Level::INFO => &self.info,
            tracing::Level::DEBUG => &self.debug,
            tracing::Level::TRACE => &self.trace,
        };
        counter.inc();
    }
}

struct TracingEventCountLayer(&'static TracingEventCountMetric);

impl<S> tracing_subscriber::layer::Layer<S> for TracingEventCountLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        self.0.inc_for_level(*event.metadata().level());
    }
}

/// Whether to add the `tracing_error` crate's `ErrorLayer`
/// to the global tracing subscriber.
///
pub enum TracingErrorLayerEnablement {
    /// Do not add the `ErrorLayer`.
    Disabled,
    /// Add the `ErrorLayer` with the filter specified by RUST_LOG, defaulting to `info` if `RUST_LOG` is unset.
    EnableWithRustLogFilter,
}

/// Where the logging should output to.
#[derive(Clone, Copy)]
pub enum Output {
    Stdout,
    Stderr,
}

pub fn init(
    log_format: LogFormat,
    tracing_error_layer_enablement: TracingErrorLayerEnablement,
    output: Output,
) -> anyhow::Result<()> {
    // We fall back to printing all spans at info-level or above if
    // the RUST_LOG environment variable is not set.
    let rust_log_env_filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
    };

    // NB: the order of the with() calls does not matter.
    // See https://docs.rs/tracing-subscriber/0.3.16/tracing_subscriber/layer/index.html#per-layer-filtering
    use tracing_subscriber::prelude::*;
    let r = tracing_subscriber::registry();
    let r = r.with({
        let log_layer = tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_ansi(false)
            .with_writer(move || -> Box<dyn std::io::Write> {
                match output {
                    Output::Stdout => Box::new(std::io::stdout()),
                    Output::Stderr => Box::new(std::io::stderr()),
                }
            });
        let log_layer = match log_format {
            LogFormat::Json => log_layer.json().boxed(),
            LogFormat::Plain => log_layer.boxed(),
            LogFormat::Test => log_layer.with_test_writer().boxed(),
        };
        log_layer.with_filter(rust_log_env_filter())
    });

    let r = r.with(
        TracingEventCountLayer(&TRACING_EVENT_COUNT_METRIC).with_filter(rust_log_env_filter()),
    );
    match tracing_error_layer_enablement {
        TracingErrorLayerEnablement::EnableWithRustLogFilter => r
            .with(tracing_error::ErrorLayer::default().with_filter(rust_log_env_filter()))
            .init(),
        TracingErrorLayerEnablement::Disabled => r.init(),
    }

    Ok(())
}

/// Disable the default rust panic hook by using `set_hook`.
///
/// For neon binaries, the assumption is that tracing is configured before with [`init`], after
/// that sentry is configured (if needed). sentry will install it's own on top of this, always
/// processing the panic before we log it.
///
/// When the return value is dropped, the hook is reverted to std default hook (prints to stderr).
/// If the assumptions about the initialization order are not held, use
/// [`TracingPanicHookGuard::forget`] but keep in mind, if tracing is stopped, then panics will be
/// lost.
#[must_use]
pub fn replace_panic_hook_with_tracing_panic_hook() -> TracingPanicHookGuard {
    std::panic::set_hook(Box::new(tracing_panic_hook));
    TracingPanicHookGuard::new()
}

/// Drop guard which restores the std panic hook on drop.
///
/// Tracing should not be used when it's not configured, but we cannot really latch on to any
/// imaginary lifetime of tracing.
pub struct TracingPanicHookGuard {
    act: bool,
}

impl TracingPanicHookGuard {
    fn new() -> Self {
        TracingPanicHookGuard { act: true }
    }

    /// Make this hook guard not do anything when dropped.
    pub fn forget(&mut self) {
        self.act = false;
    }
}

impl Drop for TracingPanicHookGuard {
    fn drop(&mut self) {
        if self.act {
            let _ = std::panic::take_hook();
        }
    }
}

/// Named symbol for our panic hook, which logs the panic.
fn tracing_panic_hook(info: &std::panic::PanicHookInfo) {
    // following rust 1.66.1 std implementation:
    // https://github.com/rust-lang/rust/blob/90743e7298aca107ddaa0c202a4d3604e29bfeb6/library/std/src/panicking.rs#L235-L288
    let location = info.location();

    let msg = match info.payload().downcast_ref::<&'static str>() {
        Some(s) => *s,
        None => match info.payload().downcast_ref::<String>() {
            Some(s) => &s[..],
            None => "Box<dyn Any>",
        },
    };

    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("<unnamed>");
    let backtrace = std::backtrace::Backtrace::capture();

    let _entered = if let Some(location) = location {
        tracing::error_span!("panic", %thread, location = %PrettyLocation(location))
    } else {
        // very unlikely to hit here, but the guarantees of std could change
        tracing::error_span!("panic", %thread)
    }
    .entered();

    if backtrace.status() == std::backtrace::BacktraceStatus::Captured {
        // this has an annoying extra '\n' in the end which anyhow doesn't do, but we cannot really
        // get rid of it as we cannot get in between of std::fmt::Formatter<'_>; we could format to
        // string, maybe even to a TLS one but tracing already does that.
        tracing::error!("{msg}\n\nStack backtrace:\n{backtrace}");
    } else {
        tracing::error!("{msg}");
    }

    // ensure that we log something on the panic if this hook is left after tracing has been
    // unconfigured. worst case when teardown is racing the panic is to log the panic twice.
    tracing::dispatcher::get_default(|d| {
        if let Some(_none) = d.downcast_ref::<tracing::subscriber::NoSubscriber>() {
            let location = location.map(PrettyLocation);
            log_panic_to_stderr(thread, msg, location, &backtrace);
        }
    });
}

#[cold]
fn log_panic_to_stderr(
    thread: &str,
    msg: &str,
    location: Option<PrettyLocation<'_, '_>>,
    backtrace: &std::backtrace::Backtrace,
) {
    eprintln!(
        "panic while tracing is unconfigured: thread '{thread}' panicked at '{msg}', {location:?}\nStack backtrace:\n{backtrace}"
    );
}

struct PrettyLocation<'a, 'b>(&'a std::panic::Location<'b>);

impl std::fmt::Display for PrettyLocation<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.0.file(), self.0.line(), self.0.column())
    }
}

impl std::fmt::Debug for PrettyLocation<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        <Self as std::fmt::Display>::fmt(self, f)
    }
}

/// When you will store a secret but want to make sure it won't
/// be accidentally logged, wrap it in a SecretString, whose Debug
/// implementation does not expose the contents.
#[derive(Clone, Eq, PartialEq)]
pub struct SecretString(String);

impl SecretString {
    pub fn get_contents(&self) -> &str {
        self.0.as_str()
    }
}

impl From<String> for SecretString {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl FromStr for SecretString {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_string()))
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[SECRET]")
    }
}

/// Logs a periodic message if a future is slow to complete.
///
/// This is performance-sensitive as it's used on the GetPage read path.
///
/// TODO: consider upgrading this to a warning, but currently it fires too often.
#[inline]
pub async fn log_slow<O>(
    name: &str,
    threshold: Duration,
    f: Pin<&mut impl Future<Output = O>>,
) -> O {
    monitor_slow_future(
        threshold,
        threshold, // period = threshold
        f,
        |MonitorSlowFutureCallback {
             ready,
             is_slow,
             elapsed_total,
             elapsed_since_last_callback: _,
         }| {
            if !is_slow {
                return;
            }
            let elapsed = elapsed_total.as_secs_f64();
            if ready {
                info!("slow {name} completed after {elapsed:.3}s");
            } else {
                info!("slow {name} still running after {elapsed:.3}s");
            }
        },
    )
    .await
}

/// Logs a periodic warning if a future is slow to complete.
#[inline]
pub async fn warn_slow<O>(
    name: &str,
    threshold: Duration,
    f: Pin<&mut impl Future<Output = O>>,
) -> O {
    monitor_slow_future(
        threshold,
        threshold, // period = threshold
        f,
        |MonitorSlowFutureCallback {
             ready,
             is_slow,
             elapsed_total,
             elapsed_since_last_callback: _,
         }| {
            if !is_slow {
                return;
            }
            let elapsed = elapsed_total.as_secs_f64();
            if ready {
                warn!("slow {name} completed after {elapsed:.3}s");
            } else {
                warn!("slow {name} still running after {elapsed:.3}s");
            }
        },
    )
    .await
}

/// Poll future `fut` to completion, invoking callback `cb` at the given `threshold` and every
/// `period` afterwards, and also unconditionally when the future completes.
#[inline]
pub async fn monitor_slow_future<F, O>(
    threshold: Duration,
    period: Duration,
    mut fut: Pin<&mut F>,
    mut cb: impl FnMut(MonitorSlowFutureCallback),
) -> O
where
    F: Future<Output = O>,
{
    let started = Instant::now();
    let mut attempt = 1;
    let mut last_cb = started;
    loop {
        // NB: use timeout_at() instead of timeout() to avoid an extra clock reading in the common
        // case where the timeout doesn't fire.
        let deadline = started + threshold + (attempt - 1) * period;
        // TODO: still call the callback if the future panics? Copy how we do it for the page_service flush_in_progress counter.
        let res = tokio::time::timeout_at(deadline, &mut fut).await;
        let now = Instant::now();
        let elapsed_total = now - started;
        cb(MonitorSlowFutureCallback {
            ready: res.is_ok(),
            is_slow: elapsed_total >= threshold,
            elapsed_total,
            elapsed_since_last_callback: now - last_cb,
        });
        last_cb = now;
        if let Ok(output) = res {
            return output;
        }
        attempt += 1;
    }
}

/// See [`monitor_slow_future`].
pub struct MonitorSlowFutureCallback {
    /// Whether the future completed. If true, there will be no more callbacks.
    pub ready: bool,
    /// Whether the future is taking `>=` the specififed threshold duration to complete.
    /// Monotonic: if true in one callback invocation, true in all subsequent onces.
    pub is_slow: bool,
    /// The time elapsed since the [`monitor_slow_future`] was first polled.
    pub elapsed_total: Duration,
    /// The time elapsed since the last callback invocation.
    /// For the initial callback invocation, the time elapsed since the [`monitor_slow_future`] was first polled.
    pub elapsed_since_last_callback: Duration,
}

#[cfg(test)]
mod tests {
    use metrics::IntCounterVec;
    use metrics::core::Opts;

    use crate::logging::{TracingEventCountLayer, TracingEventCountMetric};

    #[test]
    fn tracing_event_count_metric() {
        let counter_vec =
            IntCounterVec::new(Opts::new("testmetric", "testhelp"), &["level"]).unwrap();
        let metric = Box::leak(Box::new(TracingEventCountMetric::new(counter_vec.clone())));
        let layer = TracingEventCountLayer(metric);
        use tracing_subscriber::prelude::*;

        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            tracing::trace!("foo");
            tracing::debug!("foo");
            tracing::info!("foo");
            tracing::warn!("foo");
            tracing::error!("foo");
        });

        assert_eq!(counter_vec.with_label_values(&["trace"]).get(), 1);
        assert_eq!(counter_vec.with_label_values(&["debug"]).get(), 1);
        assert_eq!(counter_vec.with_label_values(&["info"]).get(), 1);
        assert_eq!(counter_vec.with_label_values(&["warn"]).get(), 1);
        assert_eq!(counter_vec.with_label_values(&["error"]).get(), 1);
    }
}
