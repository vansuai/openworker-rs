use std::sync::Arc;

use clap::Parser;
use ocw_provider::Router;
use ocw_server::app;
use ocw_server::state::AppState;

#[derive(Parser, Debug)]
#[command(name = "ocw-server")]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value = "8765")]
    port: u16,
    /// Optional seed/default workspace (enables the `.coworker/config.toml` layer).
    #[arg(long)]
    workspace: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    // Layered TOML config: built-in defaults < global < workspace (mirror of
    // `config.py::load_config`). The workspace is not trusted at startup.
    let config = ocw_server::config::load_config(args.workspace.as_deref(), false);
    // Use default model prefix to select the provider router.
    // When Config::default_model is empty, fall back to "anthropic" so the
    // Router always has a non-empty default (the effective model will be
    // resolved dynamically at session-creation time).
    let default_provider = if !config.default_model.is_empty() {
        let prefix = config.default_model.split(':').next().unwrap_or("");
        if ocw_provider::get_descriptor(prefix).is_some() {
            prefix.to_string()
        } else {
            // Bare model ids (e.g. gpt-5.6-sol) route through OpenAI — mirror Python.
            "openai".to_string()
        }
    } else {
        "anthropic".to_string()
    };
    let provider: Arc<dyn ocw_provider::Provider> = Arc::new(Router::new(&default_provider));
    let mut config = config;
    config.host = args.host;
    config.port = args.port;
    let state = AppState::new(config, provider);
    app::run(state).await?;
    Ok(())
}
