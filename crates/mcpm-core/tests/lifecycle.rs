use mcpm_core::{Core, discovery, edit, harness, restore, snapshot, store, takeover};
use std::{collections::BTreeMap, fs, path::Path};
fn fixture() -> (tempfile::TempDir, Core) {
    let d = tempfile::tempdir().unwrap();
    let c = Core {
        home: d.path().join("home"),
        appdata: d.path().join("appdata"),
        state_dir: d.path().join("state"),
    };
    fs::create_dir_all(&c.home).unwrap();
    (d, c)
}
fn harness(c: &Core, id: &str) -> harness::HarnessDef {
    harness::registry(&c.home, &c.appdata)
        .into_iter()
        .find(|h| h.id == id)
        .unwrap()
}
fn write(p: &Path, s: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, s).unwrap();
}
fn take(c: &Core, h: &harness::HarnessDef) -> snapshot::Snapshot {
    let p = takeover::plan_takeover(c, h, Path::new("mcpm.exe")).unwrap();
    takeover::apply_takeover(c, &p).unwrap()
}
#[test]
fn jsonc_exact_restore() {
    let (_d, c) = fixture();
    let h = harness(&c, "opencode");
    let f = &h.files[0];
    let raw = "{\r\n// user's comment\r\n\"mcp\": {\"mine\": {\"type\":\"local\",\"command\":[\"node\",\"x.js\"],},},\r\n\"theme\":\"dark\",\r\n}";
    write(&f.path, raw);
    let s = take(&c, &h);
    assert!(
        fs::read_to_string(&f.path)
            .unwrap()
            .contains("// user's comment")
    );
    assert_eq!(store::load(&c.state_dir).unwrap().servers.len(), 1);
    let r = restore::restore(&c, &s, false).unwrap();
    assert!(!r[0].merged);
    assert_eq!(fs::read(&f.path).unwrap(), raw.as_bytes());
    assert!(store::load(&c.state_dir).unwrap().servers.is_empty());
    assert_eq!(snapshot::list(&c.state_dir).unwrap().len(), 2);
    snapshot::delete(&c.state_dir, &s.id).unwrap();
}
#[test]
fn codex_tables() {
    let (_d, c) = fixture();
    let h = harness(&c, "codex");
    let f = &h.files[0];
    let raw = "# preferences\nmodel = 'gpt'\n[mcp_servers.\"my server\"]\ncommand = 'node'\nargs = ['x.js']\n[mcp_servers.\"my server\".env]\nTOKEN = 'secret'\n[other]\nvalue = 12 # leave me\n";
    write(&f.path, raw);
    let s = take(&c, &h);
    let text = fs::read_to_string(&f.path).unwrap();
    assert!(text.contains("[other]\nvalue = 12 # leave me"));
    assert_eq!(edit::entries(&text, f).unwrap().len(), 1);
    restore::restore(&c, &s, false).unwrap();
    assert_eq!(fs::read_to_string(&f.path).unwrap(), raw);
}
#[test]
fn merged_restore_preserves_new_entry() {
    let (_d, c) = fixture();
    let h = harness(&c, "claude");
    let f = &h.files[0];
    write(
        &f.path,
        r#"{"mcpServers":{"old":{"command":"node"}},"other":1}"#,
    );
    let s = take(&c, &h);
    let text = fs::read_to_string(&f.path).unwrap();
    let text = edit::set(
        &text,
        f,
        "new",
        Some(&serde_json::json!({"url":"https://new"})),
    )
    .unwrap()
    .replace("\"other\":1", "\"other\":2");
    write(&f.path, &text);
    assert!(restore::restore(&c, &s, false).unwrap()[0].merged);
    let text = fs::read_to_string(&f.path).unwrap();
    let entries = edit::entries(&text, f).unwrap();
    assert!(
        entries.contains_key("old") && entries.contains_key("new") && !entries.contains_key("mcpm")
    );
    assert_eq!(edit::read(&text, f.format).unwrap()["other"], 2);
}
#[test]
fn nonexistent_file_is_deleted() {
    let (_d, c) = fixture();
    let h = harness(&c, "cursor");
    let s = take(&c, &h);
    assert!(h.files[0].path.exists());
    restore::restore(&c, &s, false).unwrap();
    assert!(!h.files[0].path.exists());
}
#[test]
fn dedupe_and_restore_one_harness() {
    let (_d, c) = fixture();
    let a = harness(&c, "claude");
    let b = harness(&c, "cursor");
    for h in [&a, &b] {
        write(
            &h.files[0].path,
            r#"{"mcpServers":{"same":{"command":"node","args":["x"]}}}"#,
        );
    }
    let found = discovery::discover(&[a.clone(), b.clone()], &BTreeMap::new()).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].harnesses.len(), 2);
    let sa = take(&c, &a);
    take(&c, &b);
    let cfg = store::load(&c.state_dir).unwrap();
    assert_eq!(cfg.servers.len(), 1);
    restore::restore(&c, &sa, false).unwrap();
    assert_eq!(
        store::load(&c.state_dir).unwrap().servers[0].harnesses,
        ["cursor"]
    );
}
#[test]
fn projects_are_read_only() {
    let (_d, c) = fixture();
    let h = harness(&c, "claude");
    write(
        &h.files[0].path,
        r#"{"projects":{"C:/project":{"mcpServers":{"local":{"command":"node"}}}},"mcpServers":{"t3-relay":{"url":"http://legacy"},"mcpm":{"url":"http://manager"}}}"#,
    );
    let found = discovery::discover(std::slice::from_ref(&h), &BTreeMap::new()).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].origin.as_ref().unwrap().scope,
        mcpm_core::model::Scope::Project
    );
    let p = takeover::plan_takeover(&c, &h, Path::new("mcpm.exe")).unwrap();
    assert!(p.files[0].imports.is_empty());
    assert!(p.files[0].final_text.contains("C:/project"));
    assert!(p.files[0].final_text.contains("http://legacy"));
}
#[test]
fn keep_in_manager() {
    let (_d, c) = fixture();
    let h = harness(&c, "claude");
    write(
        &h.files[0].path,
        r#"{"mcpServers":{"x":{"command":"node"}}}"#,
    );
    let s = take(&c, &h);
    restore::restore(&c, &s, true).unwrap();
    assert_eq!(store::load(&c.state_dir).unwrap().servers.len(), 1);
}
#[test]
fn invalid_config_never_overwritten() {
    let (_d, c) = fixture();
    let h = harness(&c, "claude");
    write(&h.files[0].path, "{ bad");
    assert!(takeover::plan_takeover(&c, &h, Path::new("mcpm.exe")).is_err());
    assert_eq!(fs::read_to_string(&h.files[0].path).unwrap(), "{ bad");
}

#[test]
fn parser_handles_codex_headers_and_bearer() {
    let (_d, c) = fixture();
    let h = harness(&c, "codex");
    let f = &h.files[0];
    let text = "[mcp_servers.http]\nurl = 'https://example/mcp'\nhttp_headers = { X = '${VALUE}' }\nbearer_token_env_var = 'TOKEN'\n";
    let entries = edit::entries(text, f).unwrap();
    let env = BTreeMap::from([
        ("VALUE".into(), "yes".into()),
        ("TOKEN".into(), "secret".into()),
    ]);
    let t = discovery::parse_entry("codex", &entries["http"], &env).unwrap();
    assert!(
        matches!(t,mcpm_core::model::Transport::Http{headers,..} if headers["X"]=="yes" && headers["Authorization"]=="Bearer secret")
    );
}

#[test]
fn last_entry_removal_keeps_neighbor_comments() {
    let (_d, c) = fixture();
    let h = harness(&c, "opencode");
    let f = &h.files[0];
    let text = "{\"mcp\":{\"a\":{}, // retain this\n\"b\":{}}}";
    let text = edit::set(text, f, "b", None).unwrap();
    assert!(text.contains("// retain this"));
    assert_eq!(edit::entries(&text, f).unwrap().len(), 1);
}

#[test]
fn empty_profile_is_not_detected_as_claude() {
    let (_d, c) = fixture();
    assert!(!harness::detected(&harness(&c, "claude")));
}

#[test]
fn snapshot_backup_tampering_is_rejected() {
    let (_d, c) = fixture();
    let h = harness(&c, "cursor");
    let s = take(&c, &h);
    write(
        &c.state_dir
            .join("snapshots")
            .join(&s.id)
            .join("files/0.bak"),
        "corrupt",
    );
    assert!(restore::restore(&c, &s, false).is_err());
}

#[test]
fn manager_entry_shapes() {
    let exe = Path::new("mcpm.exe");
    assert_eq!(
        edit::manager_entry("antigravity", 39494, exe)["serverUrl"],
        "http://127.0.0.1:39494/mcp/antigravity"
    );
    assert_eq!(
        edit::manager_entry("gemini-cli", 39494, exe)["httpUrl"],
        "http://127.0.0.1:39494/mcp/gemini-cli"
    );
    assert_eq!(
        edit::manager_entry("claude-desktop", 39494, exe)["args"],
        serde_json::json!(["bridge", "--harness", "claude-desktop"])
    );
}
