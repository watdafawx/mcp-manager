//! Human-readable output for the CLI. Everything here returns a `String` so it is unit-testable.

use mcpm_core::model::Transport;
use mcpm_gui::model::{HarnessRow, Plan, ServerRow, SnapRow, Status, format_time, format_uptime};

/// Left-aligned table; the last column is not padded.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let mut s = String::new();
        for (i, c) in cells.iter().enumerate() {
            if i + 1 == cols {
                s.push_str(c);
            } else {
                s.push_str(&format!("{c:<w$}  ", w = widths[i]));
            }
        }
        s.trim_end().to_string()
    };
    let mut out = line(headers.to_vec());
    for r in rows {
        out.push('\n');
        out.push_str(&line(r.iter().map(String::as_str).collect()));
    }
    out
}

fn transport_text(t: &Transport) -> String {
    match t {
        Transport::Stdio { command, args, .. } => {
            if args.is_empty() {
                format!("stdio {command}")
            } else {
                format!("stdio {command} {}", args.join(" "))
            }
        }
        Transport::Http { url, .. } => format!("http {url}"),
    }
}

pub fn servers(rows: &[ServerRow]) -> String {
    if rows.is_empty() {
        return "no servers".into();
    }
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            vec![
                r.cfg.id.clone(),
                if r.cfg.enabled {
                    "on".into()
                } else {
                    "off".into()
                },
                r.state.label().to_string(),
                r.tool_count.to_string(),
                if r.cfg.harnesses.is_empty() {
                    "all".into()
                } else {
                    r.cfg.harnesses.join(",")
                },
                transport_text(&r.cfg.transport),
            ]
        })
        .collect();
    table(
        &["ID", "ENABLED", "STATE", "TOOLS", "HARNESSES", "TRANSPORT"],
        &body,
    )
}

pub fn harnesses(rows: &[HarnessRow]) -> String {
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|h| {
            let foreign = h.foreign_entries();
            vec![
                h.id.clone(),
                if h.detected {
                    h.state.label().to_string()
                } else {
                    "not detected".into()
                },
                foreign.len().to_string(),
                h.main_path().to_string(),
            ]
        })
        .collect();
    table(&["HARNESS", "STATE", "ENTRIES", "CONFIG"], &body)
}

pub fn snapshots(rows: &[SnapRow]) -> String {
    if rows.is_empty() {
        return "no snapshots".into();
    }
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|s| {
            vec![
                s.id.clone(),
                format_time(s.timestamp),
                s.operation.clone(),
                s.harnesses().join(","),
                s.files.len().to_string(),
            ]
        })
        .collect();
    table(
        &["ID", "WHEN (UTC)", "OPERATION", "HARNESS", "FILES"],
        &body,
    )
}

/// Dry-run summary of one takeover plan.
pub fn plan(p: &Plan, with_text: bool) -> String {
    let mut out = format!(
        "{}: import {} server{}, remove {} entr{}, add `mcpm`",
        p.harness,
        p.import_count(),
        if p.import_count() == 1 { "" } else { "s" },
        p.remove_count(),
        if p.remove_count() == 1 { "y" } else { "ies" },
    );
    for f in &p.files {
        out.push_str(&format!("\n  {}", f.path));
        if !f.imports.is_empty() {
            let names: Vec<&str> = f.imports.iter().map(|s| s.id.as_str()).collect();
            out.push_str(&format!("\n    import: {}", names.join(", ")));
        }
        if !f.remove.is_empty() {
            out.push_str(&format!("\n    remove: {}", f.remove.join(", ")));
        }
        if with_text {
            out.push_str("\n    ---- resulting file ----");
            for l in f.final_text.lines() {
                out.push_str(&format!("\n    {l}"));
            }
        }
    }
    out
}

pub fn status(s: &Status, autostart: Option<&str>, config_port: u16) -> String {
    let mut out = format!(
        "daemon:    running (pid {}, v{}, up {})\nport:      {}\nservers:   {} ({} enabled, {} running)\nsessions:  {}\nstate dir: {}",
        s.pid,
        s.version,
        format_uptime(s.uptime_secs),
        if s.port == 0 { config_port } else { s.port },
        s.servers,
        s.enabled_servers,
        s.running_instances,
        s.sessions,
        s.home,
    );
    out.push_str(&format!("\nautostart: {}", autostart.unwrap_or("off")));
    out
}

pub fn status_offline(
    state_dir: &str,
    port: u16,
    servers: usize,
    autostart: Option<&str>,
) -> String {
    format!(
        "daemon:    not running\nport:      {port} (configured)\nservers:   {servers} (in config.json)\nstate dir: {state_dir}\nautostart: {}",
        autostart.unwrap_or("off")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn table_aligns_columns_and_trims_the_tail() {
        let t = table(
            &["A", "BB"],
            &[
                vec!["xxx".into(), "1".into()],
                vec!["y".into(), "22".into()],
            ],
        );
        assert_eq!(t, "A    BB\nxxx  1\ny    22");
    }

    #[test]
    fn server_table_shows_exposure_and_state() {
        let rows = ServerRow::from_list(&json!([
            {"id":"fs","name":"fs","enabled":true,"harnesses":[],"status":"running","toolCount":4,
             "transport":{"type":"stdio","command":"npx","args":["-y","x"]}},
            {"id":"web","name":"web","enabled":false,"harnesses":["claude","codex"],"status":"disabled",
             "transport":{"type":"http","url":"http://h/mcp"}}
        ]));
        let t = servers(&rows);
        let lines: Vec<&str> = t.lines().collect();
        assert!(lines[0].starts_with("ID"));
        assert!(
            lines[1].contains("fs") && lines[1].contains("running") && lines[1].contains("all")
        );
        assert!(lines[1].ends_with("stdio npx -y x"));
        assert!(lines[2].contains("off") && lines[2].contains("claude,codex"));
        assert_eq!(servers(&[]), "no servers");
    }

    #[test]
    fn harness_and_snapshot_tables() {
        let h = HarnessRow::from_list(&json!([
            {"id":"cursor","label":"Cursor","detected":true,"state":"takenOver","files":[{"path":"p1","entries":["mcpm","fs"]}]},
            {"id":"codex","label":"Codex","detected":false,"state":"notConnected","files":[{"path":"p2","entries":[]}]}
        ]));
        let t = harnesses(&h);
        assert!(
            t.lines().nth(1).unwrap().contains("taken over")
                && t.lines().nth(1).unwrap().contains("p1")
        );
        assert!(t.lines().nth(2).unwrap().contains("not detected"));
        let s = SnapRow::from_list(
            &json!([{"id":"1-a","timestamp":0,"operation":"takeover","files":[{"harness":"cursor","file":{"path":"p"},"existed":true}]}]),
        );
        assert!(snapshots(&s).contains("1970-01-01 00:00:00"));
    }

    #[test]
    fn plan_summary_and_text() {
        let p = Plan::from_value(
            &json!({"harness":"cursor","files":[{"file":{"path":"C:/c.json"},
            "imports":[{"id":"fs","name":"fs","transport":{"type":"http","url":"http://x"}}],
            "remove":["fs"],"final_text":"{\n  \"a\": 1\n}"}]}),
        )
        .unwrap();
        let short = plan(&p, false);
        assert!(short.starts_with("cursor: import 1 server, remove 1 entry"));
        assert!(short.contains("import: fs") && short.contains("remove: fs"));
        assert!(!short.contains("resulting file"));
        assert!(plan(&p, true).contains("      \"a\": 1"));
    }

    #[test]
    fn status_text_lists_autostart() {
        let s = Status {
            pid: 7,
            version: "0.1.0".into(),
            port: 39494,
            servers: 2,
            enabled_servers: 1,
            ..Default::default()
        };
        let t = status(&s, Some("\"x\" daemon"), 39494);
        assert!(t.contains("pid 7") && t.contains("autostart: \"x\" daemon"));
        assert!(status_offline("d", 1, 0, None).contains("not running"));
    }
}
