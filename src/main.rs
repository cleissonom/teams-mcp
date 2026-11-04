use rmcp::ServiceExt;
use serde_json::{Value, json};
use std::process::ExitCode;
use teams_mcp::{
    config::Config,
    server::{TeamsServer, outcome},
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut args: Vec<_> = std::env::args().skip(1).collect();
    let mut config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    if args.first().is_some_and(|arg| arg == "__browser-worker") {
        let headed = match args.as_slice() {
            [_] => false,
            [_, mode] if mode == "--headed" => true,
            _ => return ExitCode::from(2),
        };
        return if teams_mcp::managed::worker(&config, headed).await.is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    if args.first().is_some_and(|arg| arg == "auth") {
        return match args.as_slice() {
            [_, action] if action == "login" => {
                print_result(teams_mcp::managed::login(&config).await)
            }
            [_, action] if action == "stop" => {
                print_result(teams_mcp::managed::stop(&config).await)
            }
            _ => {
                eprintln!(
                    "Usage: teams-mcp auth login|stop. See --help for session configuration."
                );
                ExitCode::from(2)
            }
        };
    }
    if args.first().is_some_and(|arg| arg == "--auth-source") {
        match args.get(1).and_then(|value| value.parse().ok()) {
            Some(source) => {
                config.auth_source = source;
                args.drain(..2);
            }
            None => {
                eprintln!("--auth-source requires headless, browser or app.");
                return ExitCode::from(2);
            }
        }
    }
    let server = TeamsServer::new(config);
    match args.as_slice() {
        [] => serve(server).await,
        [arg] if arg == "--help" || arg == "-h" => {
            println!(
                "teams-mcp [--auth-source headless|browser|app] [--status | --call TOOL JSON | --help | --version]\n\nteams-mcp auth login   Open the managed profile for one-time normal Teams sign-in.\nteams-mcp auth stop    Close only this connector's managed browser, retaining its session.\n\nNo action: MCP over stdio. Default: managed headless Chrome, shared by MCP clients.\nCommon: TEAMS_AUTH_SOURCE, TEAMS_ACCOUNT, TEAMS_TENANT_ID.\nHeadless: TEAMS_BROWSER_PROFILE, TEAMS_BROWSER_BINARY (installed Chrome by default).\nExisting browser: TEAMS_CDP_URL, TEAMS_CDP_PORT_FILE, TEAMS_TARGET_ID.\nApp (macOS): TEAMS_APP_WINDOW_ID, TEAMS_APP_CDP_URL.\nAll tools are read-only. See README.md for setup and platform limitations."
            );
            ExitCode::SUCCESS
        }
        [arg] if arg == "--version" => {
            println!("teams-mcp {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        [arg] if arg == "--status" => print_result(server.execute("teams_status", json!({})).await),
        [flag, tool, args] if flag == "--call" => {
            let Ok(args) = serde_json::from_str::<Value>(args) else {
                eprintln!("Arguments must be valid JSON.");
                return ExitCode::from(2);
            };
            print_result(server.execute(tool, args).await)
        }
        _ => {
            eprintln!(
                "Usage: teams-mcp [--auth-source headless|browser|app] [--status | --call TOOL JSON | --help | --version], or teams-mcp auth login|stop."
            );
            ExitCode::from(2)
        }
    }
}

async fn serve(server: TeamsServer) -> ExitCode {
    let service = match server.serve(rmcp::transport::stdio()).await {
        Ok(service) => service,
        Err(_) => {
            eprintln!("Could not initialize the Teams MCP stdio connection.");
            return ExitCode::FAILURE;
        }
    };
    tokio::select! {
        result = service.waiting() => {
            if result.is_ok() { ExitCode::SUCCESS } else { eprintln!("Teams MCP connection ended unexpectedly."); ExitCode::FAILURE }
        }
        _ = tokio::signal::ctrl_c() => ExitCode::SUCCESS,
    }
}

fn print_result(result: Result<Value, teams_mcp::error::Failure>) -> ExitCode {
    let success = result.is_ok();
    println!("{}", outcome(result));
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
