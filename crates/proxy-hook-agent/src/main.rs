use std::collections::HashMap;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(name = "hook-agent", about = "Relays Claude Code hook events to CC Proxy dashboard")]
struct Args {
    /// Dashboard URL
    #[arg(long, default_value = "http://localhost:5000")]
    dashboard_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // 1. Read hook JSON from stdin
    let stdin = {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    };

    let hook_input: serde_json::Value = serde_json::from_str(&stdin).unwrap_or_default();

    // 2. Collect CLAUDE_* environment variables
    let env_vars: HashMap<String, String> = [
        "CLAUDE_PROJECT_DIR",
        "CLAUDE_REMOTE",
        "CLAUDE_ENV_FILE",
        "CLAUDE_PLUGIN_DIRS",
    ]
    .iter()
    .filter_map(|&k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
    .collect();

    // 3. Extract fields from hook payload
    let payload = serde_json::json!({
        "hookEventName": hook_input.get("hook_event_name").and_then(|v| v.as_str()).unwrap_or("unknown"),
        "sessionId": hook_input.get("session_id").and_then(|v| v.as_str()).unwrap_or(""),
        "cwd": hook_input.get("cwd").and_then(|v| v.as_str()).unwrap_or("."),
        "permissionMode": hook_input.get("permission_mode").and_then(|v| v.as_str()).unwrap_or(""),
        "transcriptPath": hook_input.get("transcript_path").and_then(|v| v.as_str()).unwrap_or(""),
        "hookInput": hook_input,
        "environmentVariables": env_vars,
    });

    // 4. POST to dashboard
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/hook-event", args.dashboard_url))
        .json(&payload)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;

    // 5. Handle response (or silently succeed on failure)
    match resp {
        Ok(r) => {
            if let Ok(result) = r.json::<serde_json::Value>().await {
                print!("{}", result["stdout"].as_str().unwrap_or(""));
                eprint!("{}", result["stderr"].as_str().unwrap_or(""));
                std::process::exit(result["exitCode"].as_i64().unwrap_or(0) as i32);
            }
            std::process::exit(0);
        }
        Err(_) => {
            // Dashboard not running — silently succeed to never block Claude Code
            std::process::exit(0);
        }
    }
}
