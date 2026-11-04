use rmcp::ServiceExt;
use serde_json::{Value, json};
use std::process::ExitCode;
use teams_mcp::{
    config::Config,
    server::{TeamsServer, outcome},
};

const USAGE: &str = "Usage: teams-mcp [--auth-source headless|browser|app] [--pretty] [--status | --call TOOL JSON | --help | --version], or teams-mcp auth login [--no-wait]|stop|status|compact.";
const HELP: &str = "teams-mcp [--auth-source headless|browser|app] [--pretty] [--status | --call TOOL JSON | --help | --version]

teams-mcp auth login     Open the managed profile for normal Teams sign-in, wait for it, save the account and close the window.
teams-mcp auth login --no-wait   Open the sign-in window and return immediately; finish with auth stop.
teams-mcp auth stop      Close only this connector's managed browser, retaining its session.
teams-mcp auth status    Show local setup and browser state without starting Chrome or contacting Teams.
teams-mcp auth compact   Clear the Teams Service Worker cache in the managed profile; sign-in is kept.

No action: MCP over stdio. Default: managed headless Chrome, shared by MCP clients.
Common: TEAMS_AUTH_SOURCE, TEAMS_ACCOUNT, TEAMS_TENANT_ID, TEAMS_REQUEST_TIMEOUT_SECS, TEAMS_MCP_LOG.
Headless: TEAMS_BROWSER_PROFILE, TEAMS_BROWSER_BINARY, TEAMS_BROWSER (chrome|edge|chromium|brave),
  TEAMS_BROWSER_IDLE_SECS, TEAMS_BROWSER_FREEZE_SECS, TEAMS_BROWSER_CACHE_LIMIT_MB, TEAMS_BROWSER_LEAN, TEAMS_PREWARM.
Existing browser: TEAMS_CDP_URL, TEAMS_CDP_PORT_FILE, TEAMS_TARGET_ID.
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
    let print = |result| print_result(result, pretty);
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
        ["auth", "login"] => print(teams_mcp::managed::login(&config, true).await),
        ["auth", "login", "--no-wait"] => print(teams_mcp::managed::login(&config, false).await),
        ["auth", "stop"] => print(teams_mcp::managed::stop(&config).await),
        ["auth", "status"] => print(teams_mcp::managed::status(&config)),
        ["auth", "compact"] => print(teams_mcp::managed::compact(&config).await),
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
    let prewarm = server.prewarm_config();
    let service = match server.serve(rmcp::transport::stdio()).await {
        Ok(service) => service,
        Err(_) => {
            eprintln!("Could not initialize the Teams MCP stdio connection.");
            return ExitCode::FAILURE;
        }
    };
    if let Some(config) = prewarm {
        // Opt-in: start the managed browser after initialization without delaying the client.
        tokio::spawn(async move {
            let _ = teams_mcp::managed::endpoint(&config).await;
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
