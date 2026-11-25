//! End-to-end tests: a real daemon on a random port with a temp state dir, talking to a fake
//! stdio MCP server (`mcpm-fake-mcp`). Never touches real user files.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use mcpm_core::model::{InstanceMode, ServerConfig, Transport};
use mcpm_daemon::{DaemonOptions, RunningDaemon, Started};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const FAKE: &str = env!("CARGO_BIN_EXE_mcpm-fake-mcp");

fn fake(id: &str, mode: InstanceMode) -> ServerConfig {
    ServerConfig {
        id: id.into(),
        name: id.into(),
        transport: Transport::Stdio {
            command: FAKE.into(),
            args: vec![],
            env: BTreeMap::from([("FAKE_NAME".to_string(), id.to_string())]),
            cwd: None,
        },
        enabled: true,
        mode,
        pinned_tools: vec![],
        harnesses: vec![],
        origin: None,
    }
}

struct Rig {
    daemon: Option<RunningDaemon>,
    home: PathBuf,
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Rig {
    async fn start() -> Rig {
        Rig::start_with(|_| {}).await
    }

    async fn start_with(tweak: impl FnOnce(&mut DaemonOptions)) -> Rig {
        let home =
            std::env::temp_dir().join(format!("mcpm-test-{}", mcpm_daemon::timeutil::uuid()));
        let mut opts = DaemonOptions::new(home.clone());
        opts.port = Some(0);
        tweak(&mut opts);
        let Started::Running(d) = RunningDaemon::start(opts).await.expect("start daemon") else {
            panic!("already running")
        };
        Rig {
            base: d.url(),
            token: d.token.clone(),
            daemon: Some(d),
            home,
            http: reqwest::Client::new(),
        }
    }

    fn daemon(&self) -> &RunningDaemon {
        self.daemon.as_ref().unwrap()
    }

    async fn stop(mut self) {
        if let Some(d) = self.daemon.take() {
            d.shutdown().await;
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }

    /// POST a JSON-RPC message; returns (http status, json body if any, session id header).
    async fn rpc(
        &self,
        path: &str,
        sid: Option<&str>,
        body: Value,
    ) -> (u16, Option<Value>, Option<String>) {
        let mut req = self
            .http
            .post(format!("{}{path}", self.base))
            .header("content-type", "application/json")
            .body(body.to_string());
        if let Some(s) = sid {
            req = req.header("mcp-session-id", s);
        }
        let res = req.send().await.expect("post");
        let status = res.status().as_u16();
        let sid = res
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let text = res.text().await.unwrap();
        (status, serde_json::from_str(&text).ok(), sid)
    }

    /// Opens an MCP session on `/mcp/<harness>` (or `/mcp`).
    async fn init(&self, path: &str) -> String {
        let (status, body, sid) = self
            .rpc(path, None, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test-client", "version": "1" } } }))
            .await;
        assert_eq!(status, 200);
        let body = body.unwrap();
        assert_eq!(body["result"]["serverInfo"]["name"], "mcpm");
        assert_eq!(body["result"]["capabilities"]["tools"]["listChanged"], true);
        let sid = sid.expect("session id");
        let (st, _, _) = self
            .rpc(
                path,
                Some(&sid),
                json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            )
            .await;
        assert_eq!(st, 202);
        sid
    }

    async fn tools_list(&self, path: &str, sid: &str) -> Vec<Value> {
        let (_, body, _) = self
            .rpc(
                path,
                Some(sid),
                json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
            )
            .await;
        body.unwrap()["result"]["tools"].as_array().unwrap().clone()
    }

    async fn tool(&self, path: &str, sid: &str, name: &str, args: Value) -> Value {
        let (status, body, _) =
            self.rpc(path, Some(sid), json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": name, "arguments": args } })).await;
        assert_eq!(status, 200);
        body.unwrap()["result"].clone()
    }

    async fn api(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut req = self
            .http
            .request(m, format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(b) = body {
            req = req
                .header("content-type", "application/json")
                .body(b.to_string());
        }
        let res = req.send().await.unwrap();
        let status = res.status().as_u16();
        (
            status,
            serde_json::from_str(&res.text().await.unwrap()).unwrap_or(Value::Null),
        )
    }
}

fn names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

/// Parses the JSON text inside a tool result.
fn payload(result: &Value) -> Value {
    serde_json::from_str(result["content"][0]["text"].as_str().expect("text content"))
        .expect("json text")
}

fn is_error(result: &Value) -> bool {
    result["isError"] == true
}

async fn read_until(res: &mut reqwest::Response, needle: &str) -> bool {
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while let Ok(Ok(Some(chunk))) = tokio::time::timeout_at(deadline, res.chunk()).await {
        seen.push_str(&String::from_utf8_lossy(&chunk));
        if seen.contains(needle) {
            return true;
        }
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_servers_tools_and_calls() {
    let rig = Rig::start().await;
    let mut cfg = fake("docs", InstanceMode::Shared);
    if let Transport::Stdio { env, .. } = &mut cfg.transport {
        env.insert("FAKE_SECRET".into(), "s3cr3t-value-42".into());
    }
    rig.daemon().pool().add_server(cfg).unwrap();

    let sid = rig.init("/mcp/claude").await;
    let tools = rig.tools_list("/mcp/claude", &sid).await;
    assert_eq!(
        names(&tools),
        [
            "mcpm_list_servers",
            "mcpm_list_tools",
            "mcpm_call",
            "mcpm_reload_server"
        ]
    );

    let servers = payload(
        &rig.tool("/mcp/claude", &sid, "mcpm_list_servers", json!({}))
            .await,
    );
    assert_eq!(servers[0]["id"], "docs");
    assert_eq!(servers[0]["status"], "idle"); // lazy: not started yet
    let raw = servers.to_string();
    assert!(!raw.contains("s3cr3t-value-42"), "secret leaked: {raw}");
    assert_eq!(servers[0]["transport"]["env"]["FAKE_SECRET"], "••••");

    let listing = payload(
        &rig.tool(
            "/mcp/claude",
            &sid,
            "mcpm_list_tools",
            json!({ "server": "docs" }),
        )
        .await,
    );
    let tool_names: Vec<&str> = listing["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(tool_names, ["echo", "pid", "crash", "grow", "env"]);
    let filtered = payload(
        &rig.tool(
            "/mcp/claude",
            &sid,
            "mcpm_list_tools",
            json!({ "query": "ENV" }),
        )
        .await,
    );
    assert_eq!(filtered["tools"].as_array().unwrap().len(), 1);

    let res = rig
        .tool(
            "/mcp/claude",
            &sid,
            "mcpm_call",
            json!({ "server": "docs", "tool": "echo", "arguments": { "text": "hello" } }),
        )
        .await;
    assert!(!is_error(&res));
    assert_eq!(payload(&res)["echo"], "hello");
    assert_eq!(payload(&res)["server"], "docs");

    // env reaches the child; status flips to running
    let res = rig
        .tool(
            "/mcp/claude",
            &sid,
            "mcpm_call",
            json!({ "server": "docs", "tool": "env", "arguments": { "name": "FAKE_SECRET" } }),
        )
        .await;
    assert_eq!(payload(&res)["value"], "s3cr3t-value-42");
    let servers = payload(
        &rig.tool("/mcp/claude", &sid, "mcpm_list_servers", json!({}))
            .await,
    );
    assert_eq!(servers[0]["status"], "running");
    assert_eq!(servers[0]["toolCount"], 5);

    // bad calls are tool errors, not protocol errors
    assert!(is_error(
        &rig.tool(
            "/mcp/claude",
            &sid,
            "mcpm_call",
            json!({ "server": "nope", "tool": "x" })
        )
        .await
    ));
    assert!(is_error(
        &rig.tool(
            "/mcp/claude",
            &sid,
            "mcpm_call",
            json!({ "server": "docs" })
        )
        .await
    ));
    assert!(is_error(
        &rig.tool("/mcp/claude", &sid, "no_such_tool", json!({}))
            .await
    ));

    // call log
    let (_, calls) = rig.api("GET", "/api/calls", None).await;
    assert!(calls.as_array().unwrap().len() >= 2);
    let calls = calls.as_array().unwrap();
    assert!(
        calls
            .iter()
            .any(|c| c["serverId"] == "docs" && c["tool"] == "echo" && c["ok"] == true)
    );
    assert!(
        calls
            .iter()
            .any(|c| c["serverId"] == "nope" && c["ok"] == false && c["error"].is_string())
    );
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_tools_are_exposed_and_callable() {
    let rig = Rig::start().await;
    let mut cfg = fake("gh", InstanceMode::Shared);
    cfg.pinned_tools = vec!["echo".into(), "missing".into()];
    rig.daemon().pool().add_server(cfg).unwrap();

    let sid = rig.init("/mcp/codex").await;
    let tools = rig.tools_list("/mcp/codex", &sid).await;
    let n = names(&tools);
    assert!(
        n.contains(&"gh__echo".to_string()) && n.contains(&"gh__missing".to_string()),
        "{n:?}"
    );
    let echo = tools.iter().find(|t| t["name"] == "gh__echo").unwrap();
    assert!(
        echo["description"]
            .as_str()
            .unwrap()
            .contains("Echo text back")
    ); // real schema fetched
    assert_eq!(echo["inputSchema"]["properties"]["text"]["type"], "string");

    let res = rig
        .tool("/mcp/codex", &sid, "gh__echo", json!({ "text": "pinned" }))
        .await;
    assert_eq!(payload(&res)["echo"], "pinned");
    // not pinned -> not callable directly
    assert!(is_error(
        &rig.tool("/mcp/codex", &sid, "gh__pid", json!({})).await
    ));
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_restarts_on_next_call_and_reload_works() {
    let rig = Rig::start().await;
    rig.daemon()
        .pool()
        .add_server(fake("flaky", InstanceMode::Shared))
        .unwrap();
    let sid = rig.init("/mcp/claude").await;
    let call = |tool: &'static str| {
        let (rig, sid) = (&rig, sid.clone());
        async move {
            rig.tool(
                "/mcp/claude",
                &sid,
                "mcpm_call",
                json!({ "server": "flaky", "tool": tool }),
            )
            .await
        }
    };

    let first = payload(&call("pid").await)["pid"].as_u64().unwrap();
    let crashed = call("crash").await;
    assert!(is_error(&crashed), "{crashed}");
    assert!(
        crashed["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("exited")
    );

    // status shows the error until the next call restarts it
    tokio::time::sleep(Duration::from_millis(200)).await;
    let servers = payload(
        &rig.tool("/mcp/claude", &sid, "mcpm_list_servers", json!({}))
            .await,
    );
    assert_eq!(servers[0]["status"], "error");

    let second = payload(&call("pid").await)["pid"].as_u64().unwrap();
    assert_ne!(first, second, "child must be restarted");

    let reload = payload(
        &rig.tool(
            "/mcp/claude",
            &sid,
            "mcpm_reload_server",
            json!({ "server": "flaky" }),
        )
        .await,
    );
    assert_eq!(reload["status"], "running");
    assert_eq!(reload["toolCount"], 5);
    let third = payload(&call("pid").await)["pid"].as_u64().unwrap();
    assert_ne!(second, third, "reload restarts the child");
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_mode_isolates_children_shared_mode_shares() {
    let rig = Rig::start().await;
    rig.daemon()
        .pool()
        .add_server(fake("shared", InstanceMode::Shared))
        .unwrap();
    rig.daemon()
        .pool()
        .add_server(fake("browser", InstanceMode::Session))
        .unwrap();
    let (s1, s2) = (rig.init("/mcp/claude").await, rig.init("/mcp/claude").await);
    assert_ne!(s1, s2);

    let pid = |sid: String, server: &'static str| {
        let rig = &rig;
        async move {
            payload(
                &rig.tool(
                    "/mcp/claude",
                    &sid,
                    "mcpm_call",
                    json!({ "server": server, "tool": "pid" }),
                )
                .await,
            )["pid"]
                .as_u64()
                .unwrap()
        }
    };
    assert_eq!(
        pid(s1.clone(), "shared").await,
        pid(s2.clone(), "shared").await
    );
    let (b1, b2) = (
        pid(s1.clone(), "browser").await,
        pid(s2.clone(), "browser").await,
    );
    assert_ne!(b1, b2, "two sessions -> two children");
    assert_eq!(
        b1,
        pid(s1.clone(), "browser").await,
        "same session keeps its child"
    );

    let servers = payload(
        &rig.tool("/mcp/claude", &s1, "mcpm_list_servers", json!({}))
            .await,
    );
    let browser = servers
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "browser")
        .unwrap();
    assert_eq!(browser["activeSessions"], 2);

    // ending a session kills its child only
    let res = rig
        .http
        .delete(format!("{}/mcp/claude", rig.base))
        .header("mcp-session-id", &s1)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 204);
    let servers = payload(
        &rig.tool("/mcp/claude", &s2, "mcpm_list_servers", json!({}))
            .await,
    );
    let browser = servers
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "browser")
        .unwrap();
    assert_eq!(browser["activeSessions"], 1);
    assert_eq!(pid(s2, "browser").await, b2);
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_session_instances_are_reaped() {
    let rig = Rig::start_with(|o| {
        o.pool.idle_ttl = Duration::from_millis(150);
        o.pool.reap_interval = Duration::from_millis(50);
    })
    .await;
    rig.daemon()
        .pool()
        .add_server(fake("browser", InstanceMode::Session))
        .unwrap();
    rig.daemon()
        .pool()
        .add_server(fake("shared", InstanceMode::Shared))
        .unwrap();
    let sid = rig.init("/mcp/claude").await;
    for server in ["browser", "shared"] {
        rig.tool(
            "/mcp/claude",
            &sid,
            "mcpm_call",
            json!({ "server": server, "tool": "pid" }),
        )
        .await;
    }
    tokio::time::sleep(Duration::from_millis(700)).await;
    let servers = payload(
        &rig.tool("/mcp/claude", &sid, "mcpm_list_servers", json!({}))
            .await,
    );
    let get = |id: &str| {
        servers
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(
        get("browser")["activeSessions"],
        0,
        "session instance reaped"
    );
    assert_eq!(get("shared")["activeSessions"], 1, "shared instance stays");
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn harness_exposure_is_filtered() {
    let rig = Rig::start().await;
    let mut only_claude = fake("private", InstanceMode::Shared);
    only_claude.harnesses = vec!["claude".into()];
    only_claude.pinned_tools = vec!["echo".into()];
    rig.daemon().pool().add_server(only_claude).unwrap();
    rig.daemon()
        .pool()
        .add_server(fake("public", InstanceMode::Shared))
        .unwrap();

    let (claude, codex, anon) = (
        rig.init("/mcp/claude").await,
        rig.init("/mcp/codex").await,
        rig.init("/mcp").await,
    );
    let ids = |v: &Value| {
        v.as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(&payload(
            &rig.tool("/mcp/claude", &claude, "mcpm_list_servers", json!({}))
                .await
        )),
        ["private", "public"]
    );
    assert_eq!(
        ids(&payload(
            &rig.tool("/mcp/codex", &codex, "mcpm_list_servers", json!({}))
                .await
        )),
        ["public"]
    );
    // plain /mcp with an unrecognised client name counts as "unknown": restricted servers stay hidden
    assert_eq!(
        ids(&payload(
            &rig.tool("/mcp", &anon, "mcpm_list_servers", json!({}))
                .await
        )),
        ["public"]
    );

    assert!(
        names(&rig.tools_list("/mcp/claude", &claude).await).contains(&"private__echo".to_string())
    );
    assert!(
        !names(&rig.tools_list("/mcp/codex", &codex).await).contains(&"private__echo".to_string())
    );

    let denied = rig
        .tool(
            "/mcp/codex",
            &codex,
            "mcpm_call",
            json!({ "server": "private", "tool": "pid" }),
        )
        .await;
    assert!(is_error(&denied));
    assert!(
        denied["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not exposed to codex")
    );
    let denied = rig
        .tool("/mcp/codex", &codex, "private__echo", json!({}))
        .await;
    assert!(is_error(&denied));
    assert!(!is_error(
        &rig.tool(
            "/mcp/claude",
            &claude,
            "mcpm_call",
            json!({ "server": "private", "tool": "pid" })
        )
        .await
    ));

    // client name detection on plain /mcp
    let (_, body, sid) = rig
        .rpc("/mcp", None, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "clientInfo": { "name": "claude-code" } } }))
        .await;
    assert!(body.is_some());
    let detected = payload(
        &rig.tool(
            "/mcp",
            sid.as_deref().unwrap(),
            "mcpm_list_servers",
            json!({}),
        )
        .await,
    );
    assert_eq!(ids(&detected), ["private", "public"]);
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_api_requires_token_and_manages_servers() {
    let rig = Rig::start().await;
    // auth
    let res = rig
        .http
        .get(format!("{}/api/status", rig.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 401);
    let res = rig
        .http
        .get(format!("{}/api/status", rig.base))
        .bearer_auth("wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 401);
    let res = rig
        .http
        .get(format!("{}/api/servers", rig.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 401);
    let (st, status) = rig.api("GET", "/api/status", None).await;
    assert_eq!(st, 200);
    assert_eq!(status["port"], rig.daemon().port);
    assert_eq!(status["servers"], 0);

    // CRUD (name defaults to id; secrets masked in responses, kept on round trip)
    let body = json!({ "id": "svc", "transport": { "type": "stdio", "command": FAKE, "env": { "FAKE_NAME": "svc", "TOKEN": "topsecret99" } } });
    let (st, created) = rig.api("POST", "/api/servers", Some(body.clone())).await;
    assert_eq!(st, 201, "{created}");
    assert_eq!(created["name"], "svc");
    assert_eq!(created["transport"]["env"]["TOKEN"], "••••");
    assert_eq!(rig.api("POST", "/api/servers", Some(body)).await.0, 409);
    assert_eq!(
        rig.api(
            "POST",
            "/api/servers",
            Some(json!({ "id": "-bad", "transport": { "type": "stdio", "command": "x" } }))
        )
        .await
        .0,
        400
    );
    assert_eq!(
        rig.api(
            "POST",
            "/api/servers",
            Some(json!({ "id": "nourl", "transport": { "type": "http", "url": "ftp://x" } }))
        )
        .await
        .0,
        400
    );

    let (_, list) = rig.api("GET", "/api/servers", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(!list.to_string().contains("topsecret99"));

    // update with the masked placeholder keeps the stored secret
    let mut masked = created.clone();
    masked["pinnedTools"] = json!(["echo"]);
    let (st, _) = rig.api("PUT", "/api/servers/svc", Some(masked)).await;
    assert_eq!(st, 200);
    let (_, revealed) = rig.api("GET", "/api/servers/svc?reveal=1", None).await;
    assert_eq!(revealed["transport"]["env"]["TOKEN"], "topsecret99");
    assert_eq!(revealed["pinnedTools"], json!(["echo"]));
    assert_eq!(
        rig.api("PUT", "/api/servers/ghost", Some(json!({})))
            .await
            .0,
        404
    );

    // tools + restart + disable/enable
    let (st, tools) = rig.api("GET", "/api/servers/svc/tools", None).await;
    assert_eq!(st, 200);
    assert_eq!(tools.as_array().unwrap().len(), 5);
    let (st, r) = rig.api("POST", "/api/servers/svc/restart", None).await;
    assert_eq!((st, r["status"].as_str()), (200, Some("running")));
    let (_, d) = rig.api("POST", "/api/servers/svc/disable", None).await;
    assert_eq!(d["enabled"], false);
    let (_, list) = rig.api("GET", "/api/servers", None).await;
    assert_eq!(list[0]["status"], "disabled");
    assert_eq!(rig.api("GET", "/api/servers/svc/tools", None).await.0, 502);
    assert_eq!(
        rig.api("POST", "/api/servers/svc/enable", None).await.1["enabled"],
        true
    );

    // persisted to the state dir
    let saved: Value =
        serde_json::from_slice(&std::fs::read(rig.home.join("config.json")).unwrap()).unwrap();
    assert_eq!(saved["servers"][0]["id"], "svc");
    assert_eq!(
        saved["servers"][0]["transport"]["env"]["TOKEN"],
        "topsecret99"
    );

    // CoreOps routes are wired but stubbed
    assert_eq!(rig.api("GET", "/api/harnesses", None).await.0, 501);
    assert_eq!(
        rig.api(
            "POST",
            "/api/takeover/plan",
            Some(json!({ "harness": "claude" }))
        )
        .await
        .0,
        501
    );
    assert_eq!(
        rig.api("POST", "/api/takeover/plan", Some(json!({})))
            .await
            .0,
        400
    );
    assert_eq!(
        rig.api("POST", "/api/takeover/apply", Some(json!({ "all": true })))
            .await
            .0,
        501
    );
    assert_eq!(rig.api("GET", "/api/snapshots", None).await.0, 501);
    assert_eq!(
        rig.api("POST", "/api/restore", Some(json!({ "all": true })))
            .await
            .0,
        501
    );

    assert_eq!(rig.api("DELETE", "/api/servers/svc", None).await.0, 200);
    assert_eq!(rig.api("DELETE", "/api/servers/svc", None).await.0, 404);
    assert_eq!(rig.api("GET", "/api/nope", None).await.0, 404);
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_changed_after_adding_a_server_and_when_a_child_changes() {
    let rig = Rig::start().await;
    let sid = rig.init("/mcp/claude").await;
    let mut stream = rig
        .http
        .get(format!("{}/mcp/claude", rig.base))
        .header("mcp-session-id", &sid)
        .header("accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status().as_u16(), 200);
    assert!(
        stream.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    assert!(read_until(&mut stream, ": mcpm").await);

    // 1. add a server through the control API
    let body = json!({ "id": "late", "transport": { "type": "stdio", "command": FAKE, "env": { "FAKE_NAME": "late" } } });
    assert_eq!(rig.api("POST", "/api/servers", Some(body)).await.0, 201);
    assert!(
        read_until(&mut stream, "notifications/tools/list_changed").await,
        "no list_changed after add"
    );

    // 2. a child announces a new tool: cache is invalidated and the next listing has it
    let before = payload(
        &rig.tool(
            "/mcp/claude",
            &sid,
            "mcpm_list_tools",
            json!({ "server": "late" }),
        )
        .await,
    );
    assert_eq!(before["tools"].as_array().unwrap().len(), 5);
    rig.tool(
        "/mcp/claude",
        &sid,
        "mcpm_call",
        json!({ "server": "late", "tool": "grow" }),
    )
    .await;
    assert!(
        read_until(&mut stream, "notifications/tools/list_changed").await,
        "no list_changed after child change"
    );
    let after = payload(
        &rig.tool(
            "/mcp/claude",
            &sid,
            "mcpm_list_tools",
            json!({ "server": "late" }),
        )
        .await,
    );
    assert_eq!(after["tools"].as_array().unwrap().len(), 6);
    assert_eq!(after["tools"][5]["name"], "extra");

    // 3. disabling notifies too
    rig.api("POST", "/api/servers/late/disable", None).await;
    assert!(
        read_until(&mut stream, "notifications/tools/list_changed").await,
        "no list_changed after disable"
    );
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_transport_child_is_proxied() {
    // The daemon proxies a second daemon (Streamable HTTP child), which proxies the fake server.
    let inner = Rig::start().await;
    inner
        .daemon()
        .pool()
        .add_server(fake("deep", InstanceMode::Shared))
        .unwrap();
    let outer = Rig::start().await;
    outer
        .daemon()
        .pool()
        .add_server(ServerConfig {
            id: "nested".into(),
            name: "nested".into(),
            transport: Transport::Http {
                url: format!("{}/mcp/claude", inner.base),
                headers: BTreeMap::from([("X-Test".to_string(), "1".to_string())]),
            },
            enabled: true,
            mode: InstanceMode::Shared,
            pinned_tools: vec![],
            harnesses: vec![],
            origin: None,
        })
        .unwrap();
    let sid = outer.init("/mcp/claude").await;
    let listing = payload(
        &outer
            .tool(
                "/mcp/claude",
                &sid,
                "mcpm_list_tools",
                json!({ "server": "nested" }),
            )
            .await,
    );
    assert_eq!(listing["tools"].as_array().unwrap().len(), 4, "{listing}");
    // call through both layers: outer mcpm_call -> inner mcpm_call -> fake echo
    let res = outer
        .tool(
            "/mcp/claude",
            &sid,
            "mcpm_call",
            json!({ "server": "nested", "tool": "mcpm_call", "arguments": { "server": "deep", "tool": "echo", "arguments": { "text": "deep" } } }),
        )
        .await;
    assert!(!is_error(&res), "{res}");
    assert_eq!(payload(&res)["echo"], "deep");
    assert_eq!(payload(&res)["server"], "deep");
    outer.stop().await;
    inner.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_daemon_on_same_home_exits_and_daemon_json_is_cleaned() {
    let mut rig = Rig::start().await;
    let info = mcpm_daemon::storage::read_daemon_info(&rig.home).expect("daemon.json written");
    assert_eq!(info.pid, std::process::id());
    assert_eq!(info.port, rig.daemon().port);

    let mut opts = DaemonOptions::new(rig.home.clone());
    opts.port = Some(0);
    match RunningDaemon::start(opts).await.unwrap() {
        Started::AlreadyRunning(i) => assert_eq!(i.port, rig.daemon().port),
        Started::Running(_) => panic!("second daemon must not start"),
    }
    let home = rig.home.clone();
    let (st, _) = rig.api("POST", "/api/shutdown", None).await;
    assert_eq!(st, 200);
    tokio::time::timeout(Duration::from_secs(5), rig.daemon().shutdown_requested())
        .await
        .expect("shutdown requested");
    rig.daemon.take().unwrap().shutdown().await;
    assert!(
        mcpm_daemon::storage::read_daemon_info(&home).is_none(),
        "daemon.json removed on clean exit"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_foreign_origin_and_bad_json() {
    let rig = Rig::start().await;
    let res = rig
        .http
        .post(format!("{}/mcp", rig.base))
        .header("origin", "https://evil.example")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 403);
    let res = rig
        .http
        .post(format!("{}/mcp", rig.base))
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 400);
    let res = rig
        .http
        .get(format!("{}/mcp/Bad%20Id", rig.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 404);
    let res = rig
        .http
        .get(format!("{}/health", rig.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200);
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_bridge_forwards_requests_and_notifications() {
    let rig = Rig::start().await;
    rig.daemon()
        .pool()
        .add_server(fake("docs", InstanceMode::Shared))
        .unwrap();

    let (mut to_bridge, bridge_in) = tokio::io::duplex(64 * 1024);
    let (bridge_out, from_bridge) = tokio::io::duplex(64 * 1024);
    let opts = mcpm_daemon::bridge::BridgeOptions {
        home: rig.home.clone(),
        harness: "claude-desktop".into(),
        daemon_exe: None,
    };
    let task = tokio::spawn(mcpm_daemon::bridge::run_with_io(
        opts,
        BufReader::new(bridge_in),
        bridge_out,
    ));
    let mut lines = BufReader::new(from_bridge).lines();
    let mut next = async || -> Value {
        let l = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("bridge reply")
            .unwrap()
            .expect("line");
        serde_json::from_str(&l).unwrap()
    };

    to_bridge.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\"}}\n").await.unwrap();
    let init = next().await;
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "mcpm");
    to_bridge
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    to_bridge
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"mcpm_call\",\"arguments\":{\"server\":\"docs\",\"tool\":\"echo\",\"arguments\":{\"text\":\"via bridge\"}}}}\n")
        .await
        .unwrap();
    let reply = next().await;
    assert_eq!(reply["id"], 2);
    assert_eq!(payload(&reply["result"])["echo"], "via bridge");

    // list_changed arrives through the bridge's SSE listener
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.daemon()
        .pool()
        .add_server(fake("more", InstanceMode::Shared))
        .unwrap();
    let note = next().await;
    assert_eq!(note["method"], "notifications/tools/list_changed");

    drop(to_bridge); // stdin EOF ends the bridge
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("bridge exits")
        .unwrap()
        .unwrap();
    rig.stop().await;
}
