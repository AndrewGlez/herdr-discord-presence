use discord_rich_presence::{activity, DiscordIpc, DiscordIpcClient};
use fs2::FileExt;
use serde_json::Value;
use std::{env, fs::{create_dir_all, read_to_string, OpenOptions}, path::{Path, PathBuf}, process::Command, sync::{LazyLock, Mutex}, thread, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};
#[cfg(unix)]
use std::os::unix::process::CommandExt;

const DEFAULT_CLIENT_ID: &str = "1532444373130678282";
const OPENCODE_CACHE_TTL: Duration = Duration::from_secs(20);
const PI_CACHE_TTL: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
struct OpenCodeInfo {
    model: String,
    tokens: u64,
    active_subagents: usize,
    subagent_tokens: u64,
}

#[derive(Clone, Debug)]
struct OmpSubagent {
    state: &'static str,
}

#[derive(Clone, Debug)]
struct OmpInfo {
    model: String,
    tokens: u64,
    subagents: Vec<OmpSubagent>,
}

static OPENCODE_CACHE: LazyLock<Mutex<Option<(String, Instant, OpenCodeInfo)>>> = LazyLock::new(|| Mutex::new(None));
static PI_CACHE: LazyLock<Mutex<Option<(String, Instant, OmpInfo)>>> = LazyLock::new(|| Mutex::new(None));

fn agent_emoji(agent: &str) -> &'static str {
    match agent {
        "pi" | "omp" => "🤖",
        "opencode" => "🟩",
        "claude" => "✳️",
        "codex" => "⚡",
        "copilot" => "🐙",
        "gemini" => "♊",
        "cursor" => "🖱️",
        "grok" => "🛸",
        "qwen" => "🐲",
        "kimi" => "🌙",
        "devin" => "👨💻",
        "droid" => "🤖",
        "lazygit" => "🦥",
        _ => "💻",
    }
}

fn subagent_summary(info: &OmpInfo) -> String {
    if info.subagents.is_empty() {
        return String::new();
    }
    let count = |state: &str| info.subagents.iter().filter(|sub| sub.state == state).count();
    let mut parts: Vec<String> = Vec::new();
    for (state, emoji) in [("working", "⏳"), ("blocked", "⚠️"), ("idle", "💤")] {
        let n = count(state);
        if n > 0 {
            parts.push(format!("{emoji}{n}"));
        }
    }
    let breakdown = if parts.is_empty() { String::new() } else { format!(" · {}", parts.join(" ")) };
    format!(" · 🧩 {}{breakdown}", info.subagents.len())
}

fn session_usage(path: &Path) -> Option<(String, u64)> {
    let message = read_to_string(path).ok()?.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry.pointer("/message/role").and_then(Value::as_str) == Some("assistant"))
        .filter(|entry| entry.pointer("/message/usage").is_some()).last()?;
    let model = message.pointer("/message/model").and_then(Value::as_str)?.to_owned();
    let usage = message.pointer("/message/usage")?;
    let context = usage.get("input").and_then(Value::as_u64).unwrap_or(0)
        .checked_add(usage.get("cacheRead").and_then(Value::as_u64).unwrap_or(0))?;
    Some((model, context))
}

// The last meaningful event wins: a trailing `session_exit` marks the subagent done,
// an `ask`/`yield` tool call marks it blocked, any other activity marks it working.
fn omp_subagent_state(path: &Path) -> &'static str {
    let mut state = "idle";
    let Ok(contents) = read_to_string(path) else { return state };
    for line in contents.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        match entry.get("type").and_then(Value::as_str) {
            Some("custom") => match entry.get("customType").and_then(Value::as_str) {
                Some("session_exit") => state = "done",
                Some("tool_execution_start") => {
                    let tool = entry.pointer("/data/toolName").and_then(Value::as_str).unwrap_or("");
                    state = if matches!(tool, "ask" | "yield") { "blocked" } else { "working" };
                }
                _ => {}
            },
            Some("message") => {
                if entry.pointer("/message/role").and_then(Value::as_str) == Some("assistant") {
                    state = "working";
                }
            }
            _ => {}
        }
    }
    state
}

// OMP writes each subagent session under `<parent-stem>/<name>.jsonl` beside the
// parent session file. Titles come from the parent's `task` tool calls, in order.
fn omp_info(result: &Value, pane_id: &str) -> Option<OmpInfo> {
    let session_path = result.get("agents")?.as_array()?.iter()
        .find(|agent| agent.get("pane_id").and_then(Value::as_str) == Some(pane_id))?
        .pointer("/agent_session/value")?.as_str()?;
    let session = Path::new(session_path);
    let stale = session.metadata().ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|modified| modified.elapsed().ok())
        .map_or(true, |age| age > Duration::from_secs(600));
    if stale {
        return None;
    }
    let cache = &PI_CACHE;
    if let Some((cached, refreshed, info)) = cache.lock().ok()?.as_ref() {
        if cached == session_path && refreshed.elapsed() < PI_CACHE_TTL {
            return Some(info.clone());
        }
    }
    let mut subagents: Vec<OmpSubagent> = Vec::new();
    if let (Some(parent), Some(stem)) = (session.parent(), session.file_stem()) {
        if let Ok(entries) = std::fs::read_dir(parent.join(stem)) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map_or(true, |ext| ext != "jsonl") {
                    continue;
                }
                let state = omp_subagent_state(&path);
                if state != "done" {
                    subagents.push(OmpSubagent { state });
                }
            }
        }
    }
    let (model, tokens) = session_usage(session).unwrap_or_else(|| ("unknown".to_owned(), 0));
    let info = OmpInfo { model, tokens, subagents };
    *cache.lock().ok()? = Some((session_path.to_owned(), Instant::now(), info.clone()));
    Some(info)
}

fn acquire_broker_lock() -> Result<std::fs::File, Box<dyn std::error::Error>> {
    let state_dir = env::var("HERDR_PLUGIN_STATE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| env::temp_dir().join("herdr-discord-presence"));
    create_dir_all(&state_dir)?;
    let lock = OpenOptions::new().create(true).write(true).open(state_dir.join("presence.lock"))?;
    match lock.try_lock_exclusive() {
        Ok(()) => Ok(lock),
        Err(_) => Err("Discord presence is already managed by another plugin process".into()),
    }
}

fn snapshot() -> Value {
    Command::new("herdr")
        .args(["api", "snapshot"])
        .output()
        .ok()
        .and_then(|output| serde_json::from_slice(&output.stdout).ok())
        .unwrap_or_else(|| Value::Object(Default::default()))
}

fn foreground_app(pane_id: &str) -> Option<String> {
    let output = Command::new("herdr")
        .args(["pane", "process-info", "--pane", pane_id])
        .output().ok()?;
    let response: Value = serde_json::from_slice(&output.stdout).ok()?;
    let processes = response.pointer("/result/process_info/foreground_processes")?.as_array()?;
    for app in ["lazygit", "opencode", "omp"] {
        if processes.iter().any(|process| {
            [process.get("name"), process.get("cmdline")].into_iter().flatten()
                .filter_map(Value::as_str).any(|value| value.to_ascii_lowercase().contains(app))
        }) {
            return Some(app.to_owned());
        }
    }
    None
}

fn agent_usage(result: &Value, pane_id: &str) -> Option<(String, u64)> {
    let session_path = result.get("agents")?.as_array()?.iter()
        .find(|agent| agent.get("pane_id").and_then(Value::as_str) == Some(pane_id))?
        .pointer("/agent_session/value")?.as_str()?;
    let message = read_to_string(session_path).ok()?.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry.pointer("/message/role").and_then(Value::as_str) == Some("assistant"))
        .filter(|entry| entry.pointer("/message/usage").is_some()).last()?;
    let model = message.pointer("/message/model").and_then(Value::as_str)?.to_owned();
    let usage = message.pointer("/message/usage")?;
    let context = usage.get("input").and_then(Value::as_u64).unwrap_or(0)
        .checked_add(usage.get("cacheRead").and_then(Value::as_u64).unwrap_or(0))?;
    Some((model, context))
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000 { format!("{:.1}k", tokens as f64 / 1_000.0) } else { tokens.to_string() }
}

fn sql_quote(value: &str) -> String { value.replace('\'', "''") }

fn opencode_info(result: &Value, pane_id: &str) -> Option<OpenCodeInfo> {
    let pane = result.get("panes")?.as_array()?.iter()
        .find(|pane| pane.get("pane_id").and_then(Value::as_str) == Some(pane_id))?;
    let cwd = pane.get("foreground_cwd").or_else(|| pane.get("cwd"))?.as_str()?;
    if let Some(metadata) = pane.get("tokens") {
        let model = metadata.get("opencode_model").and_then(Value::as_str);
        let total = metadata.get("opencode_tokens_total").and_then(Value::as_str).and_then(|value| value.parse().ok());
        if let (Some(model), Some(total_tokens)) = (model, total) {
            return Some(OpenCodeInfo {
                model: model.to_owned(),
                tokens: total_tokens,
                active_subagents: metadata.get("opencode_subagents_running").and_then(Value::as_str).and_then(|value| value.parse().ok()).unwrap_or(0),
                subagent_tokens: metadata.get("opencode_subagent_tokens").and_then(Value::as_str).and_then(|value| value.parse().ok()).unwrap_or(0),
            });
        }
    }
    let session_id = result.get("agents")?.as_array()?.iter()
        .find(|agent| agent.get("pane_id").and_then(Value::as_str) == Some(pane_id))
        .and_then(|agent| agent.pointer("/agent_session/value")).and_then(Value::as_str).map(str::to_owned)
        .or_else(|| {
            let title = pane.get("terminal_title_stripped").and_then(Value::as_str)
                .and_then(|title| title.strip_prefix("OC | "));
            let filter = match title {
                Some(title) => format!("directory = '{}' AND title = '{}'", sql_quote(cwd), sql_quote(title)),
                None => format!("directory = '{}'", sql_quote(cwd)),
            };
            let sql = format!("SELECT id FROM session WHERE parent_id IS NULL AND {filter} ORDER BY time_updated DESC LIMIT 1");
            let output = Command::new("opencode").current_dir(cwd).args(["db", &sql, "--format", "json"]).output().ok()?;
            serde_json::from_slice::<Vec<Value>>(&output.stdout).ok()?.first()?
                .get("id").and_then(Value::as_str).map(str::to_owned)
        })?;
    let cache = &OPENCODE_CACHE;
    if let Some((cached_id, refreshed, info)) = cache.lock().ok()?.as_ref() {
        if cached_id == &session_id && refreshed.elapsed() < OPENCODE_CACHE_TTL { return Some(info.clone()); }
    }
    let output = Command::new("opencode").current_dir(cwd).args(["export", &session_id]).output().ok()?;
    let export: Value = serde_json::from_slice(&output.stdout).ok()?;
    let info = export.get("info")?;
    let model = info.pointer("/model/id").and_then(Value::as_str)?.to_owned();
    let tokens = info.pointer("/tokens/input").and_then(Value::as_u64).unwrap_or(0)
        + info.pointer("/tokens/output").and_then(Value::as_u64).unwrap_or(0)
        + info.pointer("/tokens/cache/read").and_then(Value::as_u64).unwrap_or(0);
    let sql = format!(
        "SELECT DISTINCT s.tokens_input, s.tokens_output, s.tokens_cache_read FROM session s WHERE s.parent_id = '{}' AND (EXISTS (SELECT 1 FROM part p WHERE p.session_id = s.id AND json_extract(p.data, '$.state.status') = 'running') OR EXISTS (SELECT 1 FROM message m WHERE m.session_id = s.id AND json_extract(m.data, '$.role') = 'assistant' AND json_extract(m.data, '$.time.completed') IS NULL))",
        session_id
    );
    let subagents: Vec<Value> = Command::new("opencode").current_dir(cwd).args(["db", &sql, "--format", "json"])
        .output().ok().and_then(|output| serde_json::from_slice(&output.stdout).ok()).unwrap_or_default();
    let subagent_tokens = subagents.iter().map(|subagent| {
        subagent.get("tokens_input").and_then(Value::as_u64).unwrap_or(0)
            + subagent.get("tokens_output").and_then(Value::as_u64).unwrap_or(0)
            + subagent.get("tokens_cache_read").and_then(Value::as_u64).unwrap_or(0)
    }).sum();
    let parsed = OpenCodeInfo { model, tokens, active_subagents: subagents.len(), subagent_tokens };
    *cache.lock().ok()? = Some((session_id, Instant::now(), parsed.clone()));
    Some(parsed)
}

fn git_output(cwd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new("git").arg("-C").arg(cwd).args(args).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn lazygit_state(cwd: &str) -> (String, String) {
    let branch = git_output(cwd, &["branch", "--show-current"]).filter(|value| !value.is_empty())
        .unwrap_or_else(|| "detached HEAD".to_owned());
    let commits = git_output(cwd, &["rev-list", "--count", "HEAD"]).unwrap_or_else(|| "?".to_owned());
    let changed = git_output(cwd, &["status", "--porcelain"]).map(|value| value.lines().count()).unwrap_or(0);
    let divergence = git_output(cwd, &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"])
        .and_then(|value| {
            let mut values = value.split_whitespace();
            Some((values.next()?.parse::<u64>().ok()?, values.next()?.parse::<u64>().ok()?))
        });
    let sync = match divergence { Some((behind, ahead)) => format!(" · ↑{ahead} ↓{behind}"), None => String::new() };
    (format!("{branch} · {commits} commits"), format!("Lazygit · {changed} changes{sync}"))
}

fn release_binary() -> PathBuf {
    env::var("HERDR_PLUGIN_ROOT").map(PathBuf::from)
        .unwrap_or_else(|_| env::current_dir().expect("current directory unavailable"))
        .join("target/release/herdr-discord-presence")
}

#[cfg(unix)]
fn reload_if_rebuilt(binary: &Path, built_at: SystemTime) -> Result<(), Box<dyn std::error::Error>> {
    if binary.metadata()?.modified()? <= built_at { return Ok(()); }
    eprintln!("new release binary detected; hot reloading Discord presence");
    // The inherited advisory lock stays held across exec; do not acquire a second lock.
    env::set_var("HERDR_DISCORD_BROKER_LOCK_HELD", "1");
    let error = Command::new(binary).args(env::args_os().skip(1)).exec();
    Err(format!("hot reload failed: {error}").into())
}

#[cfg(not(unix))]
fn reload_if_rebuilt(_: &Path, _: SystemTime) -> Result<(), Box<dyn std::error::Error>> { Ok(()) }

fn update(client: &mut DiscordIpcClient, started_at: i64) -> Result<(), Box<dyn std::error::Error>> {
    let root = snapshot();
    let result = root.get("result").and_then(|r| r.get("snapshot")).unwrap_or(&root);
    let focused = result.get("focused_pane_id").and_then(Value::as_str);
    let pane = result.get("panes").and_then(Value::as_array).and_then(|panes| {
        panes.iter().find(|pane| pane.get("pane_id").and_then(Value::as_str) == focused)
    });
    let cwd = pane.and_then(|p| p.get("foreground_cwd").or_else(|| p.get("cwd")))
        .and_then(Value::as_str).unwrap_or("Herdr");
    let folder = Path::new(cwd).file_name().and_then(|name| name.to_str()).unwrap_or(cwd);
    let status = pane.and_then(|p| p.get("agent_status"))
        .and_then(Value::as_str).unwrap_or("idle");
    let agent = pane.and_then(|p| p.get("agent")).and_then(Value::as_str)
        .or_else(|| result.get("agents").and_then(Value::as_array).and_then(|agents| {
            agents.iter().find(|item| item.get("focused").and_then(Value::as_bool) == Some(true))
                .and_then(|item| item.get("agent")).and_then(Value::as_str)
        }))
        .unwrap_or("terminal");
    let workspace_id = pane.and_then(|p| p.get("workspace_id")).and_then(Value::as_str);
    let workspace = result.get("workspaces").and_then(Value::as_array).and_then(|items| {
        items.iter().find(|item| item.get("workspace_id").and_then(Value::as_str) == workspace_id)
    }).and_then(|item| item.get("label")).and_then(Value::as_str).unwrap_or("Herdr");
    let running = matches!(status, "working" | "blocked");
    let pane_count = result.get("panes").and_then(Value::as_array).map_or(0, Vec::len);
    let branch = git_output(cwd, &["branch", "--show-current"]).filter(|value| !value.is_empty());
    let app = pane.and_then(|p| p.get("pane_id")).and_then(Value::as_str)
        .and_then(foreground_app);
    let using_lazygit = app.as_deref() == Some("lazygit");
    let using_opencode = app.as_deref() == Some("opencode");
    let using_omp = app.as_deref() == Some("omp");
    let agent_label = if using_omp { "omp" } else { agent };
    let emoji = agent_emoji(if using_opencode { "opencode" } else { agent_label });
    let opencode_info = pane.and_then(|p| p.get("pane_id")).and_then(Value::as_str)
        .and_then(|pane_id| if using_opencode { opencode_info(result, pane_id) } else { None });
    // OMP and Pi share the JSONL session format, so both get the subagent/telemetry path.
    let omp = pane.and_then(|p| p.get("pane_id")).and_then(Value::as_str)
        .and_then(|pane_id| if agent == "pi" || agent == "omp" { omp_info(result, pane_id) } else { None });
    let usage_info = pane.and_then(|p| p.get("pane_id")).and_then(Value::as_str)
        .and_then(|pane_id| if !using_opencode && !using_omp && agent != "pi" { agent_usage(result, pane_id) } else { None });
    let detected_activity = using_lazygit || using_opencode || using_omp || agent != "terminal";
    let default_details = match &branch {
        Some(branch) if workspace != branch => format!("📁 {workspace} · 🌿 {branch}"),
        Some(_) => format!("📁 {workspace}"),
        None => format!("📁 {workspace} · 📂 {folder}"),
    };
    let (details, lazygit_status) = if using_lazygit {
        let (branch, changes) = lazygit_state(cwd);
        (format!("🦥 {branch}"), changes)
    } else {
        (default_details, String::new())
    };
    let state = if using_lazygit {
        format!("🦥 {lazygit_status}")
    } else if using_opencode {
        match opencode_info {
            Some(info) if running => {
                let subagents = if info.active_subagents == 0 { String::new() } else {
                    format!(" · 🧩 {} · +{}", info.active_subagents, format_tokens(info.subagent_tokens))
                };
                format!("🟩 opencode · ⚙️ working · {} · used {}{subagents}", info.model, format_tokens(info.tokens))
            },
            Some(_) => format!("🟩 opencode · 💤 ready · {pane_count} panes"),
            None if running => "🟩 opencode · ⚙️ working".to_owned(),
            None => format!("🟩 opencode · 💤 ready · {pane_count} panes"),
        }
    } else if let Some(info) = omp {
        let subagents = subagent_summary(&info);
        let model = if info.model.is_empty() { "unknown".to_owned() } else { info.model.clone() };
        if running {
            format!("{emoji} {agent_label} · ⚙️ {status} · {model} · ctx {}{subagents}", format_tokens(info.tokens))
        } else {
            format!("{emoji} {agent_label} · 💤 ready · {pane_count} panes")
        }
    } else if running {
        let status_emoji = if status == "blocked" { "⚠️" } else { "⚙️" };
        match usage_info {
            Some((model, tokens)) => format!("{emoji} {agent_label} · {status_emoji} {status} · {model} · ctx {}", format_tokens(tokens)),
            None => format!("{emoji} {agent_label} · {status_emoji} {status}"),
        }
    } else {
        format!("{emoji} {agent_label} · 💤 ready · {pane_count} panes")
    };
    let image = if using_lazygit { "logo" } else if using_opencode { "opencode" } else {
        match agent { "pi" | "omp" => "pi", "claude" => "claude", "codex" => "codex", _ => if running { "agent" } else { "logo" } }
    };
    let large_text = format!("{emoji} {}", if using_lazygit { "Lazygit" } else if using_opencode { "OpenCode" } else { agent_label });
    client.set_activity(activity::Activity::new()
        .details(&details)
        .state(&state)
        .timestamps(activity::Timestamps::new().start(started_at))
        .assets({
            let assets = activity::Assets::new()
                .large_image(image)
                .large_text(&large_text);
            if detected_activity {
                assets.small_image("logo").small_text("Herdr")
            } else {
                assets
            }
        }))?;
    Ok(())
}

fn run_client(client_id: &str, interval: u64, once: bool, started_at: i64, binary: &Path, built_at: SystemTime) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = DiscordIpcClient::new(client_id)?;
    client.connect()?;
    eprintln!("connected to Discord IPC");
    update(&mut client, started_at)?;
    if once { return Ok(()); }
    loop {
        thread::sleep(Duration::from_millis(interval));
        reload_if_rebuilt(binary, built_at)?;
        update(&mut client, started_at)?;
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if env::args().any(|arg| arg == "--diagnose-opencode") {
        let root = snapshot();
        let result = root.get("result").and_then(|r| r.get("snapshot")).unwrap_or(&root);
        let pane_id = env::args().skip_while(|arg| arg != "--diagnose-opencode").nth(1)
            .unwrap_or_else(|| result.get("focused_pane_id").and_then(Value::as_str).unwrap_or("").to_owned());
        println!("pane: {pane_id}");
        println!("OpenCode telemetry: {:?}", opencode_info(result, &pane_id));
        return Ok(());
    }
    let _broker_lock = if env::var("HERDR_DISCORD_BROKER_LOCK_HELD").as_deref() == Ok("1") {
        None
    } else {
        Some(acquire_broker_lock()?)
    };
    let client_id = env::var("HERDR_DISCORD_CLIENT_ID")
        .unwrap_or_else(|_| DEFAULT_CLIENT_ID.to_owned());
    let interval = env::var("HERDR_DISCORD_INTERVAL").ok()
        .and_then(|value| value.parse().ok()).unwrap_or(5_000);
    let once = env::args().any(|arg| arg == "--once");
    let started_at = env::var("HERDR_DISCORD_STARTED_AT").ok().and_then(|value| value.parse().ok())
        .unwrap_or(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64);
    env::set_var("HERDR_DISCORD_STARTED_AT", started_at.to_string());
    let binary = release_binary();
    let built_at = binary.metadata()?.modified()?;
    let mut retry_delay = Duration::from_secs(1);
    loop {
        match run_client(&client_id, interval, once, started_at, &binary, built_at) {
            Ok(()) if once => return Ok(()),
            Ok(()) => eprintln!("Discord IPC connection ended; retrying in {} seconds", retry_delay.as_secs()),
            Err(error) => eprintln!("Discord presence lost: {error}; retrying in {} seconds", retry_delay.as_secs()),
        }
        if once { return Ok(()); }
        thread::sleep(retry_delay);
        retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
    }
}
