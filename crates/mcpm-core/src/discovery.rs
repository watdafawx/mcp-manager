//! Harness server normalization and cross-harness deduplication.
use crate::{edit, harness::HarnessDef, model::*};
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeMap;
/// Expands shell-style and OpenCode environment references using explicit values.
pub fn expand_string(value: &str, env: &BTreeMap<String, String>) -> String {
    let re = regex::Regex::new(
        r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}|\{env:([A-Za-z_][A-Za-z0-9_]*)\}",
    )
    .unwrap();
    re.replace_all(value, |c: &regex::Captures<'_>| {
        env.get(c.get(1).or_else(|| c.get(3)).unwrap().as_str())
            .cloned()
            .unwrap_or_else(|| c.get(2).map_or("", |v| v.as_str()).to_string())
    })
    .into_owned()
}
fn string(v: &Value) -> String {
    v.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| v.to_string())
}
fn map(v: &Value, env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    v.as_object()
        .map(|o| {
            o.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), expand_string(&string(v), env)))
                .collect()
        })
        .unwrap_or_default()
}
/// Converts supported harness connection fields, including OpenCode command arrays.
pub fn parse_entry(harness: &str, v: &Value, env: &BTreeMap<String, String>) -> Option<Transport> {
    let url = ["url", "serverUrl", "httpUrl"]
        .iter()
        .find_map(|k| v[*k].as_str().filter(|s| !s.trim().is_empty()));
    if let Some(url) = url {
        let url = expand_string(url, env).trim().to_string();
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return None;
        }
        let mut headers = map(
            v.get("headers")
                .or_else(|| v.get("http_headers"))
                .unwrap_or(&Value::Null),
            env,
        );
        if harness == "codex"
            && let Some(token) = v["bearer_token_env_var"]
                .as_str()
                .and_then(|key| env.get(key))
        {
            headers.insert("Authorization".into(), format!("Bearer {token}"));
        }
        return Some(Transport::Http { url, headers });
    }
    let mut args = vec![];
    let command = if let Some(array) = v["command"].as_array() {
        args.extend(array.iter().skip(1).map(string));
        array.first()?.as_str()?
    } else {
        v["command"].as_str()?
    };
    args.extend(v["args"].as_array().into_iter().flatten().map(string));
    let command = expand_string(command, env).trim().to_string();
    if command.is_empty() {
        return None;
    }
    Some(Transport::Stdio {
        command,
        args: args.iter().map(|s| expand_string(s, env)).collect(),
        env: map(
            v.get("env")
                .or_else(|| v.get("environment"))
                .unwrap_or(&Value::Null),
            env,
        ),
        cwd: v["cwd"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(|s| expand_string(s, env)),
    })
}
/// Returns the relay-compatible identity (credentials deliberately excluded).
pub fn identity(t: &Transport) -> String {
    match t {
        Transport::Http { url, .. } => format!("http|{}", url.trim().to_lowercase()),
        Transport::Stdio {
            command, args, cwd, ..
        } => format!(
            "stdio|{command}|{}|{}",
            args.join("\0"),
            cwd.as_deref().unwrap_or("")
        ),
    }
}
/// Produces a unique valid identifier from a display name.
pub fn sanitize_id(name: &str, taken: &[ServerConfig]) -> String {
    let re = regex::Regex::new("[^a-z0-9_-]+").unwrap();
    let base = re
        .replace_all(&name.trim().to_lowercase(), "-")
        .trim_matches('-')
        .chars()
        .take(40)
        .collect::<String>();
    let mut base = if base.is_empty() {
        "server".into()
    } else {
        base
    };
    if !base.as_bytes()[0].is_ascii_alphanumeric() {
        base = format!("s-{base}");
    }
    let mut id = base.clone();
    let mut n = 2;
    while taken.iter().any(|s| s.id.eq_ignore_ascii_case(&id)) {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}
/// Merges equal transports' harness exposure, or inserts with a unique identifier.
pub fn merge(servers: &mut Vec<ServerConfig>, mut server: ServerConfig) {
    if let Some(existing) = servers
        .iter_mut()
        .find(|s| identity(&s.transport) == identity(&server.transport))
    {
        if !existing.harnesses.is_empty() {
            for h in server.harnesses {
                if !existing.harnesses.contains(&h) {
                    existing.harnesses.push(h);
                }
            }
        }
    } else {
        server.id = sanitize_id(&server.name, servers);
        servers.push(server);
    }
}
pub(crate) fn server(
    name: &str,
    v: &Value,
    h: &str,
    path: &std::path::Path,
    scope: Scope,
    env: &BTreeMap<String, String>,
) -> Option<ServerConfig> {
    if name == MANAGER_ENTRY || name == LEGACY_RELAY_ENTRY {
        return None;
    }
    Some(ServerConfig {
        id: sanitize_id(name, &[]),
        name: name.into(),
        transport: parse_entry(h, v, env)?,
        enabled: v["enabled"] != false && v["disabled"] != true,
        mode: InstanceMode::Shared,
        pinned_tools: vec![],
        harnesses: vec![h.into()],
        origin: Some(Origin {
            harness: h.into(),
            path: path.to_string_lossy().into(),
            entry: name.into(),
            scope,
            snapshot: None,
        }),
    })
}
/// Discovers global and read-only Claude project entries from explicit registry files.
pub fn discover(
    harnesses: &[HarnessDef],
    env: &BTreeMap<String, String>,
) -> Result<Vec<ServerConfig>> {
    let mut out = vec![];
    for h in harnesses {
        for f in &h.files {
            let text = match std::fs::read_to_string(&f.path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            for (name, v) in edit::entries(&text, f)? {
                if let Some(s) = server(&name, &v, &h.id, &f.path, f.scope, env) {
                    merge(&mut out, s);
                }
            }
            if h.id == "claude"
                && let Some(projects) = edit::read(&text, f.format)?["projects"].as_object()
            {
                for p in projects.values() {
                    if let Some(entries) = p["mcpServers"].as_object() {
                        for (name, v) in entries {
                            if let Some(s) = server(name, v, &h.id, &f.path, Scope::Project, env) {
                                merge(&mut out, s);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expansion() {
        let env = BTreeMap::from([("PRESENT".into(), "yes".into())]);
        assert_eq!(
            expand_string("a-${PRESENT}-${MISSING:-fallback}-{env:PRESENT}", &env),
            "a-yes-fallback-yes"
        );
        assert_eq!(expand_string("${MISSING}", &env), "");
    }
    #[test]
    fn opencode() {
        let t = parse_entry(
            "opencode",
            &serde_json::json!({"command":["npx","-y","thing"],"environment":{"K":"v"}}),
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(matches!(t,Transport::Stdio{args,..} if args==["-y","thing"]));
    }
}
