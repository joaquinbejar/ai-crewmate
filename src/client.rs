//! Console client: everything the MCP tools can do, from a human terminal.
//!
//! Connects to the bus over the same Streamable HTTP transport and the same
//! bearer token a coding agent would use, so a human on the shell is
//! just another agent on the bus:
//!
//! ```text
//! export BUS_URL=https://bus.internal.example/mcp
//! export BUS_TOKEN=acs_...
//! ai-crew-sync client whoami
//! ai-crew-sync client send --channel deploys --body "staging is on 1.4.2"
//! ai-crew-sync client read --scope inbox
//! ai-crew-sync client task claim --key refactor-auth
//! ```

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ClientInfo},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::{Value, json};

#[derive(Args)]
pub struct ClientArgs {
    /// URL of the bus MCP endpoint, e.g. https://bus.example.com/mcp
    #[arg(long, env = "BUS_URL", default_value = "http://localhost:8787/mcp")]
    pub url: String,

    /// Your agent token (issued with `ai-crew-sync token issue`).
    #[arg(long, env = "BUS_TOKEN", hide_env_values = true)]
    pub token: String,

    /// Print raw JSON instead of the human-readable rendering.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: ClientCmd,
}

#[derive(Subcommand)]
pub enum ClientCmd {
    /// Who am I on the bus, and is anything waiting for me?
    Whoami,
    /// List the tools the server exposes (sanity check).
    Tools,
    /// Send a message: --channel to broadcast, --to for a direct message.
    Send {
        #[arg(long, conflicts_with = "to")]
        channel: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        body: String,
        #[arg(long)]
        reply_to: Option<i64>,
    },
    /// Ask a teammate's agent and wait for the answer.
    Ask {
        to: String,
        /// The question. Omit when resuming with --resume-id.
        question: Option<String>,
        #[arg(long)]
        timeout_seconds: Option<i64>,
        /// Keep waiting on an earlier question (its question_message_id).
        #[arg(long)]
        resume_id: Option<i64>,
    },
    /// Read messages ("all", "inbox", or a channel name).
    Read {
        #[arg(long, default_value = "all")]
        scope: String,
        /// Re-read history instead of only unread messages.
        #[arg(long)]
        history: bool,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Full-text search messages.
    Search {
        query: String,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// List channels.
    Channels,
    /// Create a channel.
    ChannelCreate {
        name: String,
        #[arg(long)]
        topic: Option<String>,
    },
    /// Who is on the bus and what are they doing?
    Agents {
        #[arg(long)]
        online: bool,
    },
    /// Publish your own presence.
    Beat {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        activity: Option<String>,
        #[arg(long)]
        ttl_seconds: Option<i64>,
    },
    /// List tasks.
    Tasks {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        mine: bool,
    },
    /// Operate on a single task.
    #[command(subcommand)]
    Task(TaskCmd),
    /// List notes.
    Notes {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        tag: Option<String>,
    },
    /// Operate on a single note.
    #[command(subcommand)]
    Note(NoteCmd),
    /// Block until something happens on the bus (or the timeout passes).
    Wait {
        #[arg(long)]
        timeout_seconds: Option<i64>,
        /// Restrict to kinds: message, task, lock, note.
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
    },
    /// Advisory locks on shared resources.
    #[command(subcommand)]
    Lock(LockCmd),
    /// Summary of the team's recent activity.
    Digest {
        #[arg(long, default_value_t = 24)]
        hours: i64,
    },
    /// Escape hatch: call any tool with raw JSON arguments.
    Call {
        tool: String,
        /// JSON object with the arguments, e.g. '{"key": "x"}'.
        #[arg(long, default_value = "{}")]
        args: String,
    },
}

#[derive(Subcommand)]
pub enum LockCmd {
    Acquire {
        name: String,
        #[arg(long)]
        ttl_seconds: Option<i64>,
        #[arg(long)]
        purpose: Option<String>,
    },
    Release {
        name: String,
    },
    List,
}

#[derive(Subcommand)]
pub enum TaskCmd {
    Create {
        key: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        description: Option<String>,
        /// Comma-separated keys of tasks this one depends on.
        #[arg(long, value_delimiter = ',')]
        depends_on: Vec<String>,
    },
    Show {
        key: String,
    },
    Claim {
        key: String,
        #[arg(long)]
        lease_seconds: Option<i64>,
    },
    /// Claim the oldest available task, whatever it is.
    Next {
        #[arg(long)]
        lease_seconds: Option<i64>,
    },
    Renew {
        key: String,
        #[arg(long)]
        lease_seconds: Option<i64>,
    },
    Release {
        key: String,
    },
    Done {
        key: String,
        #[arg(long)]
        result: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum NoteCmd {
    Get {
        key: String,
        #[arg(long)]
        scope: Option<String>,
    },
    Set {
        key: String,
        #[arg(long)]
        value: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,
    },
    Rm {
        key: String,
        #[arg(long)]
        scope: Option<String>,
    },
    Search {
        query: String,
        #[arg(long)]
        scope: Option<String>,
    },
}

/// Strip nulls so optional flags the user did not pass are simply absent.
fn compact(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, compact(v)))
                .collect(),
        ),
        other => other,
    }
}

fn to_call(cmd: &ClientCmd) -> Option<(&'static str, Value)> {
    let (tool, args): (&str, Value) = match cmd {
        ClientCmd::Whoami => ("whoami", json!({})),
        ClientCmd::Tools => return None,
        ClientCmd::Send {
            channel,
            to,
            body,
            reply_to,
        } => (
            "post_message",
            json!({"channel": channel, "to": to, "body": body, "reply_to": reply_to}),
        ),
        ClientCmd::Ask {
            to,
            question,
            timeout_seconds,
            resume_id,
        } => (
            "ask_agent",
            json!({
                "to": to, "question": question,
                "timeout_seconds": timeout_seconds, "resume_message_id": resume_id
            }),
        ),
        ClientCmd::Read {
            scope,
            history,
            limit,
        } => (
            "read_messages",
            json!({"scope": scope, "only_new": !history, "limit": limit}),
        ),
        ClientCmd::Search { query, limit } => {
            ("search_messages", json!({"query": query, "limit": limit}))
        }
        ClientCmd::Channels => ("list_channels", json!({})),
        ClientCmd::ChannelCreate { name, topic } => {
            ("create_channel", json!({"name": name, "topic": topic}))
        }
        ClientCmd::Agents { online } => ("list_agents", json!({"online_only": online})),
        ClientCmd::Beat {
            status,
            repo,
            branch,
            activity,
            ttl_seconds,
        } => (
            "heartbeat",
            json!({
                "status": status, "repo": repo, "branch": branch,
                "activity": activity, "ttl_seconds": ttl_seconds
            }),
        ),
        ClientCmd::Tasks { status, mine } => {
            ("list_tasks", json!({"status": status, "mine_only": mine}))
        }
        ClientCmd::Wait {
            timeout_seconds,
            kinds,
        } => (
            "wait_for_updates",
            json!({
                "timeout_seconds": timeout_seconds,
                "kinds": if kinds.is_empty() { Value::Null } else { json!(kinds) }
            }),
        ),
        ClientCmd::Digest { hours } => ("team_digest", json!({"hours": hours})),
        ClientCmd::Lock(lock) => match lock {
            LockCmd::Acquire {
                name,
                ttl_seconds,
                purpose,
            } => (
                "acquire_lock",
                json!({"name": name, "ttl_seconds": ttl_seconds, "purpose": purpose}),
            ),
            LockCmd::Release { name } => ("release_lock", json!({"name": name})),
            LockCmd::List => ("list_locks", json!({})),
        },
        ClientCmd::Task(task) => match task {
            TaskCmd::Create {
                key,
                title,
                description,
                depends_on,
            } => (
                "create_task",
                json!({
                    "key": key, "title": title, "description": description,
                    "depends_on": if depends_on.is_empty() { Value::Null } else { json!(depends_on) }
                }),
            ),
            TaskCmd::Show { key } => ("get_task", json!({"key": key})),
            TaskCmd::Claim { key, lease_seconds } => (
                "claim_task",
                json!({"key": key, "lease_seconds": lease_seconds}),
            ),
            TaskCmd::Next { lease_seconds } => {
                ("claim_next_task", json!({"lease_seconds": lease_seconds}))
            }
            TaskCmd::Renew { key, lease_seconds } => (
                "renew_task_lease",
                json!({"key": key, "lease_seconds": lease_seconds}),
            ),
            TaskCmd::Release { key } => ("release_task", json!({"key": key})),
            TaskCmd::Done { key, result } => {
                ("complete_task", json!({"key": key, "result": result}))
            }
        },
        ClientCmd::Notes { scope, tag } => ("list_notes", json!({"scope": scope, "tag": tag})),
        ClientCmd::Note(note) => match note {
            NoteCmd::Get { key, scope } => ("get_note", json!({"key": key, "scope": scope})),
            NoteCmd::Set {
                key,
                value,
                scope,
                tags,
            } => (
                "set_note",
                json!({
                    "key": key, "value": value, "scope": scope,
                    "tags": if tags.is_empty() { Value::Null } else { json!(tags) }
                }),
            ),
            NoteCmd::Rm { key, scope } => ("delete_note", json!({"key": key, "scope": scope})),
            NoteCmd::Search { query, scope } => {
                ("search_notes", json!({"query": query, "scope": scope}))
            }
        },
        ClientCmd::Call { .. } => unreachable!("handled by caller"),
    };
    Some((tool, compact(args)))
}

pub async fn run(args: ClientArgs) -> anyhow::Result<()> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(args.url.clone());
    config.auth_header = Some(args.token.clone());
    config.allow_stateless = true;
    let transport = StreamableHttpClientTransport::from_config(config);

    let client = ClientInfo::default()
        .serve(transport)
        .await
        .context("could not connect to the bus (check --url and --token)")?;

    let outcome = run_command(&client, &args).await;
    let _ = client.cancel().await;
    outcome
}

async fn run_command(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ClientInfo>,
    args: &ClientArgs,
) -> anyhow::Result<()> {
    // `tools` is the one command that is not a tool call.
    if matches!(args.command, ClientCmd::Tools) {
        let tools = client.list_all_tools().await?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&tools)?);
        } else {
            for tool in tools {
                println!(
                    "{:<20} {}",
                    tool.name,
                    tool.description.as_deref().unwrap_or_default().trim()
                );
            }
        }
        return Ok(());
    }

    let (tool, call_args) = match &args.command {
        ClientCmd::Call { tool, args: raw } => {
            let parsed: Value = serde_json::from_str(raw)
                .with_context(|| format!("--args is not valid JSON: {raw}"))?;
            if !parsed.is_object() {
                bail!("--args must be a JSON object");
            }
            (tool.clone(), parsed)
        }
        other => {
            let (tool, call_args) = to_call(other).expect("tools handled above");
            (tool.to_string(), call_args)
        }
    };

    let arguments: serde_json::Map<String, Value> =
        serde_json::from_value(call_args).expect("compact() always yields an object");
    let result = client
        .call_tool(CallToolRequestParams::new(tool.clone()).with_arguments(arguments))
        .await
        .map_err(|e| anyhow::anyhow!("{tool} failed: {e}"))?;

    if result.is_error == Some(true) {
        bail!("{tool} returned an error: {:?}", result.content);
    }
    let value = result
        .structured_content
        .clone()
        .unwrap_or_else(|| json!({ "ok": true }));

    if args.json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        render(&args.command, &value);
    }
    Ok(())
}

// ------------------------------------------------------------- rendering --

fn field<'v>(v: &'v Value, key: &str) -> &'v str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn render_messages(value: &Value) {
    let messages = value["messages"].as_array().cloned().unwrap_or_default();
    if messages.is_empty() {
        println!("(no messages)");
        return;
    }
    for m in &messages {
        let target = m["channel"]
            .as_str()
            .map(|c| format!("#{c}"))
            .or_else(|| m["to"].as_str().map(|t| format!("@{t}")))
            .unwrap_or_default();
        println!(
            "[{}] {} {} → {}: {}",
            m["id"],
            field(m, "created_at"),
            field(m, "from"),
            target,
            field(m, "body"),
        );
    }
    if value["truncated"].as_bool() == Some(true) {
        println!("(truncated at limit; raise --limit to see more)");
    }
}

fn render_task_line(t: &Value) {
    let holder = t["claimed_by"]
        .as_str()
        .map(|h| format!(" ({h})"))
        .unwrap_or_default();
    let expired = if t["lease_expired"].as_bool() == Some(true) {
        " [lease expired]"
    } else {
        ""
    };
    println!(
        "{:<24} {:<8}{}{} {}",
        field(t, "key"),
        field(t, "status"),
        holder,
        expired,
        field(t, "title"),
    );
}

fn render(cmd: &ClientCmd, value: &Value) {
    match cmd {
        ClientCmd::Whoami => {
            println!(
                "{} @ {} — {} unread DM(s), {} claimed task(s)",
                field(value, "agent"),
                field(value, "team"),
                value["unread_direct_messages"],
                value["open_claimed_tasks"],
            );
        }
        ClientCmd::Read { .. } | ClientCmd::Search { .. } => render_messages(value),
        ClientCmd::Send { .. } => {
            let m = &value["message"];
            println!("sent [{}] to {}", m["id"], value["delivered_to"]);
        }
        ClientCmd::Ask { .. } => {
            if value["answered"].as_bool() == Some(true) {
                let a = &value["answer"];
                println!("{}: {}", field(a, "from"), field(a, "body"));
            } else {
                println!("(no answer yet)");
                println!("{}", field(value, "suggestion"));
            }
        }
        ClientCmd::ChannelCreate { .. } => {
            println!("channel #{} ready", field(value, "name"));
        }
        ClientCmd::Channels => {
            let channels = value["channels"].as_array().cloned().unwrap_or_default();
            if channels.is_empty() {
                println!("(no channels yet)");
            }
            for c in &channels {
                println!(
                    "#{:<20} {:>5} msg(s)  {}",
                    field(c, "name"),
                    c["message_count"],
                    field(c, "topic"),
                );
            }
        }
        ClientCmd::Agents { .. } => {
            for a in value["agents"].as_array().cloned().unwrap_or_default() {
                let place = match (a["repo"].as_str(), a["branch"].as_str()) {
                    (Some(r), Some(b)) => format!(" {r}@{b}"),
                    (Some(r), None) => format!(" {r}"),
                    _ => String::new(),
                };
                println!(
                    "{:<20} {:<8}{} {}",
                    field(&a, "name"),
                    field(&a, "status"),
                    place,
                    field(&a, "activity"),
                );
            }
            println!("({} online)", value["online_count"]);
        }
        ClientCmd::Tasks { .. } => {
            let tasks = value["tasks"].as_array().cloned().unwrap_or_default();
            if tasks.is_empty() {
                println!("(no tasks)");
            }
            for t in &tasks {
                render_task_line(t);
            }
            println!("({} open, {} claimed)", value["open"], value["claimed"]);
        }
        ClientCmd::Task(TaskCmd::Show { .. }) => {
            render_task_line(&value["task"]);
            if let Some(desc) = value["task"]["description"].as_str() {
                println!("  {desc}");
            }
            if let Some(result) = value["task"]["result"].as_str() {
                println!("  result: {result}");
            }
            for e in value["history"].as_array().cloned().unwrap_or_default() {
                println!(
                    "  {} {} {} {}",
                    field(&e, "created_at"),
                    field(&e, "event"),
                    field(&e, "agent"),
                    field(&e, "detail"),
                );
            }
        }
        ClientCmd::Task(TaskCmd::Claim { .. } | TaskCmd::Next { .. }) => {
            if value["claimed"].as_bool() == Some(true) {
                print!("claimed: ");
                render_task_line(&value["task"]);
            } else {
                println!(
                    "not claimed: {}",
                    value["reason"].as_str().unwrap_or("unknown reason")
                );
            }
        }
        ClientCmd::Task(_) => {
            render_task_line(value);
        }
        ClientCmd::Notes { .. } | ClientCmd::Note(NoteCmd::Search { .. }) => {
            let notes = value["notes"].as_array().cloned().unwrap_or_default();
            if notes.is_empty() {
                println!("(no notes)");
            }
            for n in &notes {
                let first_line = field(n, "value").lines().next().unwrap_or("").to_owned();
                println!("{}/{}: {}", field(n, "scope"), field(n, "key"), first_line);
            }
        }
        ClientCmd::Note(NoteCmd::Get { .. }) => {
            if value["found"].as_bool() == Some(true) {
                let n = &value["note"];
                println!(
                    "{}/{} (by {}, {})",
                    field(n, "scope"),
                    field(n, "key"),
                    field(n, "updated_by"),
                    field(n, "updated_at"),
                );
                println!("{}", field(n, "value"));
            } else {
                println!("not found");
            }
        }
        ClientCmd::Note(NoteCmd::Set { .. }) => {
            println!("saved {}/{}", field(value, "scope"), field(value, "key"));
        }
        ClientCmd::Note(NoteCmd::Rm { .. }) => {
            println!("{}", field(value, "detail"));
        }
        ClientCmd::Beat { .. } => {
            println!(
                "presence updated: {} {}",
                field(value, "status"),
                field(value, "activity"),
            );
        }
        ClientCmd::Wait { .. } => {
            if value["woke"].as_bool() == Some(true) {
                for e in value["events"].as_array().cloned().unwrap_or_default() {
                    println!("[{}] {}", field(&e, "kind"), field(&e, "summary"));
                }
            } else {
                println!("(nothing happened)");
            }
            println!("{}", field(value, "suggestion"));
        }
        ClientCmd::Lock(LockCmd::List) => {
            let locks = value["locks"].as_array().cloned().unwrap_or_default();
            if locks.is_empty() {
                println!("(no locks held)");
            }
            for l in &locks {
                println!(
                    "{:<28} {} until {}  {}",
                    field(l, "name"),
                    field(l, "holder"),
                    field(l, "expires_at"),
                    field(l, "purpose"),
                );
            }
        }
        ClientCmd::Lock(LockCmd::Acquire { .. }) => {
            if value["acquired"].as_bool() == Some(true) {
                let l = &value["lock"];
                println!(
                    "acquired {} until {}",
                    field(l, "name"),
                    field(l, "expires_at")
                );
            } else {
                println!("not acquired: {}", field(value, "reason"));
            }
        }
        ClientCmd::Lock(LockCmd::Release { .. }) => {
            println!("{}", field(value, "detail"));
        }
        ClientCmd::Digest { .. } => {
            println!("— last {}h —", value["hours"]);
            for c in value["channels"].as_array().cloned().unwrap_or_default() {
                println!("#{}: {} msg(s)", field(&c, "name"), c["message_count"]);
                for m in c["last_messages"].as_array().cloned().unwrap_or_default() {
                    println!("   {}: {}", field(&m, "from"), field(&m, "body"));
                }
            }
            let moved = value["tasks_moved"].as_array().cloned().unwrap_or_default();
            if !moved.is_empty() {
                println!("tasks:");
                for t in &moved {
                    println!(
                        "   {} → {} {}",
                        field(t, "key"),
                        field(t, "status"),
                        t["claimed_by"]
                            .as_str()
                            .map(|s| format!("({s})"))
                            .unwrap_or_default(),
                    );
                }
            }
            for n in value["notes_updated"]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                println!(
                    "note {}/{} by {}",
                    field(&n, "scope"),
                    field(&n, "key"),
                    field(&n, "updated_by")
                );
            }
            println!(
                "({} open, {} claimed, {} lock(s))",
                value["open_tasks"],
                value["claimed_tasks"],
                value["active_locks"].as_array().map(Vec::len).unwrap_or(0),
            );
        }
        _ => {
            println!("{}", serde_json::to_string_pretty(value).unwrap());
        }
    }
}
