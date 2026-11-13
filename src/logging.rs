//! tracing-subscriber init with JSON output.

use tracing::Subscriber;
use tracing_subscriber::{
    EnvFilter, Layer, fmt, fmt::MakeWriter, prelude::*, registry::LookupSpan,
};

pub fn init(default_level: &str) {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_level.to_lowercase()));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(json_layer(std::io::stdout))
        .init();
}

/// JSON lines to `writer`. A line logged inside a span has the span fields in a `span` object.
pub fn json_layer<S, W>(writer: W) -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + 'static,
{
    fmt::layer()
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .with_target(true)
        .with_writer(writer)
}
