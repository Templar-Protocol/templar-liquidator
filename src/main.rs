use templar_liquidator::{Args, LiquidatorService, RunMode};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // LOG_FORMAT=json emits one JSON object per line for log aggregators
    // (Loki, CloudWatch, …); anything else keeps the human-readable format.
    let json_logs = std::env::var("LOG_FORMAT").is_ok_and(|v| v.eq_ignore_ascii_case("json"));
    let env_filter = || {
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info,templar_liquidator=debug"))
    };
    if json_logs {
        tracing_subscriber::registry()
            .with(
                fmt::layer()
                    .json()
                    .with_target(false)
                    .with_thread_ids(false)
                    .with_line_number(false)
                    .with_file(false),
            )
            .with(env_filter())
            .init();
    } else {
        tracing_subscriber::registry()
            .with(
                fmt::layer()
                    .with_target(false)
                    .with_thread_ids(false)
                    .with_line_number(false)
                    .with_file(false),
            )
            .with(env_filter())
            .init();
    }

    // Parse arguments and build configuration
    let args = Args::parse_args();
    args.log_startup();

    let config = args.build_config();
    let run_mode = config.run_mode;

    // Create and run service
    let service = LiquidatorService::new(config);
    match run_mode {
        RunMode::Loop => {
            service.run().await;
            std::process::ExitCode::SUCCESS
        }
        RunMode::Once => match service.run_once().await {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!(error = %e, "run-once cycle failed");
                std::process::ExitCode::FAILURE
            }
        },
    }
}
