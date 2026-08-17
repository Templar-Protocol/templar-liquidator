use templar_liquidator::{Args, LiquidatorService, RunMode};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_target(false)
                .with_thread_ids(false)
                .with_line_number(false)
                .with_file(false),
        )
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,templar_liquidator=debug")),
        )
        .init();

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
