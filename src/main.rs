use rmcp::ServiceExt;
use serde_json::{Value, json};
use std::process::ExitCode;
use teams_mcp::{
    auth,
    config::Config,
    server::{TeamsServer, outcome},
};

const USAGE: &str = "Usage: teams-mcp [--auth-source headless|browser|app] [--pretty] [--status | --call TOOL JSON | --help | --version], or teams-mcp auth login [--no-wait]|stop|status|compact.";
const HELP: &str = "teams-mcp [--auth-source headless|browser|app] [--pretty] [--status | --call TOOL JSON | --help | --version]

teams-mcp auth login     Guide and wait for normal sign-in in the selected existing browser or App.
teams-mcp auth login --no-wait   Show guidance and check the existing session once.
teams-mcp --auth-source headless auth login   Wait for active reads, open managed sign-in, save the selected account and close setup.
teams-mcp --auth-source headless auth login --no-wait   Open managed setup and return; finish with --auth-source headless auth stop.
teams-mcp --auth-source headless auth stop      Close only this connector's managed browser, retaining its session.
teams-mcp auth status    Show local setup evidence; this does not verify authentication or API permissions.
teams-mcp --auth-source headless auth compact   Clear the managed Teams Service Worker cache; sign-in is kept.

No action: MCP over stdio. Default: existing browser with an approved debugging connection.
Codex and Claude Code use the same error.recovery workflow. MCP OAuth login does not authenticate this stdio server.
Common: TEAMS_AUTH_SOURCE, TEAMS_TRANSPORT (page|lease), TEAMS_ACCOUNT, TEAMS_TENANT_ID, TEAMS_REQUEST_TIMEOUT_SECS, TEAMS_MCP_LOG.
Headless: TEAMS_BROWSER_PROFILE, TEAMS_BROWSER_BINARY, TEAMS_BROWSER (chrome|edge|chromium|brave),
  TEAMS_BROWSER_IDLE_SECS, TEAMS_BROWSER_FREEZE_SECS, TEAMS_BROWSER_CACHE_LIMIT_MB, TEAMS_BROWSER_LEAN, TEAMS_PREWARM.
Existing browser: TEAMS_CDP_URL, TEAMS_CDP_PORT_FILE, TEAMS_TARGET_ID, TEAMS_HIDDEN_TAB.
App (macOS): TEAMS_APP_WINDOW_ID, TEAMS_APP_CDP_URL.
All tools are read-only. See README.md for setup and platform limitations.";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let mut args = Vec::new();
    let mut pretty = false;
    let mut raw = std::env::args().skip(1);
    while let Some(arg) = raw.next() {
        match arg.as_str() {
            "--pretty" => pretty = true,
            "--auth-source" => match raw.next().and_then(|value| value.parse().ok()) {
                Some(source) => config.auth_source = source,
                None => {
                    eprintln!("--auth-source requires headless, browser or app.");
                    return ExitCode::from(2);
                }
            },
            _ => args.push(arg),
        }
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let recovery_config = config.clone();
    let print = |result: Result<Value, teams_mcp::error::Failure>| {
        print_result(
            result.map_err(|error| auth::recover(error, &recovery_config)),
            pretty,
        )
    };
    match args.as_slice() {
        ["__browser-worker", rest @ ..] => {
            let headed = match rest {
                [] => false,
                ["--headed"] => true,
                _ => return ExitCode::from(2),
            };
            if teams_mcp::managed::worker(&config, headed).await.is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        ["auth", "login"] => print(auth::cli::login(&config, true).await),
        ["auth", "login", "--no-wait"] => print(auth::cli::login(&config, false).await),
        ["auth", "stop"] => print(auth::cli::stop(&config).await),
        ["auth", "status"] => print(auth::local_status(&config)),
        ["auth", "compact"] => print(auth::cli::compact(&config).await),
        ["auth", ..] => {
            eprintln!(
                "Usage: teams-mcp auth login [--no-wait]|stop|status|compact. See --help for session configuration."
            );
            ExitCode::from(2)
        }
        [] => serve(TeamsServer::new(config)).await,
        ["--help" | "-h"] => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        ["--version"] => {
            println!("teams-mcp {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        ["--status"] => print(
            TeamsServer::new(config)
                .execute("teams_status", json!({}))
                .await,
        ),
        ["--call", tool, arguments] => {
            let Ok(arguments) = serde_json::from_str::<Value>(arguments) else {
                eprintln!("Arguments must be valid JSON.");
                return ExitCode::from(2);
            };
            print(TeamsServer::new(config).execute(tool, arguments).await)
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

async fn serve(server: TeamsServer) -> ExitCode {
    let prewarm = server.prewarm().then(|| server.clone());
    let service = match server.serve(rmcp::transport::stdio()).await {
        Ok(service) => service,
        Err(_) => {
            eprintln!("Could not initialize the Teams MCP stdio connection.");
            return ExitCode::FAILURE;
        }
    };
    if let Some(server) = prewarm {
        // Opt-in: ready the session (or mint a lease) after initialization without delaying the client.
        tokio::spawn(async move {
            let _ = server.execute("teams_status", json!({})).await;
        });
    }
    tokio::select! {
        result = service.waiting() => {
            if result.is_ok() { ExitCode::SUCCESS } else { eprintln!("Teams MCP connection ended unexpectedly."); ExitCode::FAILURE }
        }
        _ = tokio::signal::ctrl_c() => ExitCode::SUCCESS,
    }
}

fn print_result(result: Result<Value, teams_mcp::error::Failure>, pretty: bool) -> ExitCode {
    let success = result.is_ok();
    let value = outcome(result);
    if pretty {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
        );
    } else {
        println!("{value}");
    }
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
