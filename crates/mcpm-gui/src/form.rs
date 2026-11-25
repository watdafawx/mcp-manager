//! Edit/add form state for one server, and its conversion to and from `ServerConfig`.

use std::collections::BTreeMap;

use mcpm_core::model::{InstanceMode, ServerConfig, Transport, valid_server_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Stdio,
    Http,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerForm {
    /// `Some` while editing an existing server (id is then fixed).
    pub editing: Option<String>,
    pub id: String,
    pub name: String,
    pub kind: Kind,
    pub command: String,
    /// One argument per line.
    pub args: String,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub mode: InstanceMode,
    pub enabled: bool,
    pub pinned: Vec<String>,
    pub harnesses: Vec<String>,
}

impl Default for ServerForm {
    fn default() -> Self {
        Self {
            editing: None,
            id: String::new(),
            name: String::new(),
            kind: Kind::Stdio,
            command: String::new(),
            args: String::new(),
            cwd: String::new(),
            env: Vec::new(),
            url: String::new(),
            headers: Vec::new(),
            mode: InstanceMode::Shared,
            enabled: true,
            pinned: Vec::new(),
            harnesses: Vec::new(),
        }
    }
}

fn pairs(m: &BTreeMap<String, String>) -> Vec<(String, String)> {
    m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn to_map(rows: &[(String, String)]) -> BTreeMap<String, String> {
    rows.iter()
        .filter(|(k, _)| !k.trim().is_empty())
        .map(|(k, v)| (k.trim().to_string(), v.clone()))
        .collect()
}

impl ServerForm {
    pub fn from_config(c: &ServerConfig) -> Self {
        let mut f = Self {
            editing: Some(c.id.clone()),
            id: c.id.clone(),
            name: c.name.clone(),
            mode: c.mode,
            enabled: c.enabled,
            pinned: c.pinned_tools.clone(),
            harnesses: c.harnesses.clone(),
            ..Self::default()
        };
        f.load_transport(&c.transport);
        f
    }

    /// Replaces command/args/env/url/headers (used when toggling secret reveal).
    pub fn load_transport(&mut self, t: &Transport) {
        match t {
            Transport::Stdio { command, args, env, cwd } => {
                self.kind = Kind::Stdio;
                self.command = command.clone();
                self.args = args.join("\n");
                self.cwd = cwd.clone().unwrap_or_default();
                self.env = pairs(env);
            }
            Transport::Http { url, headers } => {
                self.kind = Kind::Http;
                self.url = url.clone();
                self.headers = pairs(headers);
            }
        }
    }

    pub fn to_config(&self) -> Result<ServerConfig, String> {
        let id = self.id.trim().to_string();
        if !valid_server_id(&id) {
            return Err("id: use letters, digits, `-` or `_` (max 48, must start with a letter or digit)".into());
        }
        let transport = match self.kind {
            Kind::Stdio => {
                let command = self.command.trim().to_string();
                if command.is_empty() {
                    return Err("command is required".into());
                }
                let args = self
                    .args
                    .lines()
                    .map(|l| l.trim_end_matches('\r'))
                    .filter(|l| !l.trim().is_empty())
                    .map(str::to_string)
                    .collect();
                let cwd = self.cwd.trim();
                Transport::Stdio {
                    command,
                    args,
                    env: to_map(&self.env),
                    cwd: (!cwd.is_empty()).then(|| cwd.to_string()),
                }
            }
            Kind::Http => {
                let url = self.url.trim().to_string();
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err("url must start with http:// or https://".into());
                }
                Transport::Http { url, headers: to_map(&self.headers) }
            }
        };
        let name = self.name.trim();
        Ok(ServerConfig {
            name: if name.is_empty() { id.clone() } else { name.to_string() },
            id,
            transport,
            enabled: self.enabled,
            mode: self.mode,
            pinned_tools: self.pinned.clone(),
            harnesses: self.harnesses.clone(),
            // The daemon keeps the stored origin on update; new servers have none.
            origin: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio() -> ServerForm {
        ServerForm {
            id: "fs".into(),
            command: "npx".into(),
            args: "-y\n@x/server\n\n".into(),
            env: vec![("K".into(), "v".into()), (" ".into(), "ignored".into())],
            ..Default::default()
        }
    }

    #[test]
    fn stdio_form_to_config() {
        let c = stdio().to_config().unwrap();
        assert_eq!(c.name, "fs");
        let Transport::Stdio { command, args, env, cwd } = c.transport else { panic!() };
        assert_eq!((command.as_str(), args.len(), env.len(), cwd), ("npx", 2, 1, None));
    }

    #[test]
    fn validation_errors() {
        let mut f = stdio();
        f.id = "Bad Id".into();
        assert!(f.to_config().unwrap_err().starts_with("id:"));
        let mut f = stdio();
        f.command = " ".into();
        assert!(f.to_config().is_err());
        let f = ServerForm { id: "h".into(), kind: Kind::Http, url: "ftp://x".into(), ..Default::default() };
        assert!(f.to_config().is_err());
    }

    #[test]
    fn round_trips_through_config() {
        let f = ServerForm {
            id: "web".into(),
            name: "Web".into(),
            kind: Kind::Http,
            url: "https://h/mcp".into(),
            headers: vec![("Authorization".into(), "••••".into())],
            mode: InstanceMode::Session,
            pinned: vec!["a".into()],
            harnesses: vec!["claude".into()],
            ..Default::default()
        };
        let c = f.to_config().unwrap();
        let back = ServerForm::from_config(&c);
        assert_eq!(back.editing.as_deref(), Some("web"));
        assert_eq!(back.to_config().unwrap(), c);
    }
}
