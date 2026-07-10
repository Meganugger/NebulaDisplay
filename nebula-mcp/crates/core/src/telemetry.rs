//! Tracing / telemetry initialisation.
//!
//! Configures a `tracing` subscriber with:
//!
//! * an `EnvFilter` derived from config (or the `RUST_LOG` env var),
//! * a **stderr** sink (never stdout — stdout is reserved for the MCP JSON
//!   transport), in JSON or pretty format,
//! * an optional rotating file sink via `tracing-appender`,
//! * an optional OpenTelemetry OTLP layer (compiled only with the `otel`
//!   feature).
//!
//! The returned [`TelemetryGuard`] must be kept alive for the process lifetime;
//! dropping it flushes the non-blocking file writer.

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::config::{LogFormat, LogRotation, LoggingConfig};
use crate::error::ToolError;

/// Keeps background telemetry workers alive. Drop to flush and shut down.
#[must_use = "dropping the guard immediately shuts telemetry down"]
pub struct TelemetryGuard {
    _file_guard: Option<WorkerGuard>,
}

/// Initialise the global tracing subscriber from `cfg`.
///
/// Safe to call at most once per process; a second call returns an error
/// because the global default subscriber can only be set once.
pub fn init(cfg: &LoggingConfig) -> Result<TelemetryGuard, ToolError> {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&cfg.level))
        .map_err(|e| ToolError::Internal(format!("invalid log filter '{}': {e}", cfg.level)))?;

    // Console layer always writes to stderr to keep stdout clean for MCP.
    let (console_layer, stderr_json) = match cfg.format {
        LogFormat::Json => (
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .json()
                .with_current_span(true)
                .with_span_list(false)
                .boxed(),
            true,
        ),
        LogFormat::Pretty => (
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(atty_stderr())
                .compact()
                .boxed(),
            false,
        ),
    };
    let _ = stderr_json;

    let mut file_guard = None;
    let file_layer = if let Some(dir) = &cfg.directory {
        std::fs::create_dir_all(dir)
            .map_err(|e| ToolError::Io(format!("creating log dir {}: {e}", dir.display())))?;
        let appender = match cfg.rotation {
            LogRotation::Minutely => tracing_appender::rolling::minutely(dir, &cfg.file_prefix),
            LogRotation::Hourly => tracing_appender::rolling::hourly(dir, &cfg.file_prefix),
            LogRotation::Daily => tracing_appender::rolling::daily(dir, &cfg.file_prefix),
            LogRotation::Never => tracing_appender::rolling::never(dir, &cfg.file_prefix),
        };
        let (nb, guard) = tracing_appender::non_blocking(appender);
        file_guard = Some(guard);
        Some(
            tracing_subscriber::fmt::layer()
                .with_writer(nb)
                .with_ansi(false)
                .json()
                .with_current_span(true)
                .with_span_list(true)
                .boxed(),
        )
    } else {
        None
    };

    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(console_layer)
        .with(file_layer);

    #[cfg(feature = "otel")]
    {
        if let Some(endpoint) = &cfg.otel_endpoint {
            let otel_layer = otel::build_layer(endpoint)?;
            registry
                .with(otel_layer)
                .try_init()
                .map_err(|e| ToolError::Internal(format!("initialising tracing: {e}")))?;
            return Ok(TelemetryGuard {
                _file_guard: file_guard,
            });
        }
    }
    #[cfg(not(feature = "otel"))]
    {
        if cfg.otel_endpoint.is_some() {
            eprintln!(
                "warning: logging.otel_endpoint is set but the server was built \
                 without the 'otel' feature; OpenTelemetry export is disabled."
            );
        }
    }

    registry
        .try_init()
        .map_err(|e| ToolError::Internal(format!("initialising tracing: {e}")))?;

    Ok(TelemetryGuard {
        _file_guard: file_guard,
    })
}

/// TTY detection for stderr colouring, via the stable `IsTerminal` trait.
fn atty_stderr() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

#[cfg(feature = "otel")]
mod otel {
    use tracing_subscriber::Layer;

    use crate::error::ToolError;

    /// Build an OpenTelemetry tracing layer exporting via OTLP/gRPC.
    pub fn build_layer<S>(endpoint: &str) -> Result<impl Layer<S>, ToolError>
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_otlp::WithExportConfig;

        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint.to_string())
            .build()
            .map_err(|e| ToolError::Internal(format!("otlp exporter: {e}")))?;

        let provider = opentelemetry_sdk::trace::TracerProvider::builder()
            .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
            .build();
        let tracer = provider.tracer("nebula-mcp");
        opentelemetry::global::set_tracer_provider(provider);

        Ok(tracing_opentelemetry::layer().with_tracer(tracer))
    }
}
