//! Command line grammar (clap) and the pure helpers that turn arguments into values.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use mcpm_core::model::{InstanceMode, ServerConfig, Transport, valid_server_id};

#[derive(Parser)]
#[command(
    name = "mcpm",
    version,
    about = "MCP Manager: one place for every MCP server of every AI harness",
    after_help = "Run with no arguments to open the window."
)]
pub struct Cli {
    /// Print the raw JSON instead of text.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Run the background daemon (no window).
    Daemon {
        /// Listen on this port instead of the configured one.
        #[arg(long)]
        port: Option<u16>,
        /// No notification-area icon (tests, headless sessions).
        #[arg(long)]
        no_tray: bool,
        /// Wait for this process to exit first (an update starts the new daemon this way).
        #[arg(long, hide = true)]
        wait_pid: Option<u32>,
    },
    /// stdio <-> HTTP bridge for stdio-only harnesses; starts the daemon if needed.
    Bridge {
        #[arg(long)]
        harness: String,
    },
    /// Open the window (default). Starts the daemon if needed.
    Gui,
    /// Daemon state and a summary of the config.
    Status,
    /// List the harnesses and whether they are connected.
    Harnesses,
    /// Move a harness's MCP servers into the manager (snapshot first).
    #[command(group(ArgGroup::new("target").required(true).args(["harness", "all"])))]
    Takeover {
        /// Harness id (see `mcpm harnesses`).
        harness: Option<String>,
        /// Every detected harness that is not taken over yet.
        #[arg(long)]
        all: bool,
        /// Show what would change and write nothing.
        #[arg(long)]
        dry_run: bool,
        /// With --dry-run: also print the resulting file text.
        #[arg(long, requires = "dry_run")]
        text: bool,
    },
    /// Put harness configs back from their snapshots.
    #[command(group(ArgGroup::new("target").required(true).args(["harness", "all", "snapshot"])))]
    Restore {
        harness: Option<String>,
        #[arg(long)]
        all: bool,
        /// Restore one snapshot by id (see `mcpm snapshots`).
        #[arg(long)]
        snapshot: Option<String>,
        /// Keep the imported servers in the manager.
        #[arg(long)]
        keep: bool,
    },
    /// List snapshots.
    Snapshots,
    /// Manage servers.
    Servers {
        #[command(subcommand)]
        action: Option<ServersCmd>,
    },
    /// Start the daemon at sign-in (per-user Run key).
    Autostart {
        #[arg(value_enum, default_value_t = OnOff::Status)]
        state: OnOff,
    },
    /// Restore every harness, turn autostart off and stop the daemon.
    Uninstall,
    /// Look for a newer release and install it.
    Update {
        /// Only look; install nothing.
        #[arg(long)]
        check: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum OnOff {
    On,
    Off,
    Status,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once per process
pub enum ServersCmd {
    /// List servers (default).
    List,
    /// Add a server: stdio (`--command`) or Streamable HTTP (`--url`).
    Add(AddArgs),
    Remove {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
}

#[derive(Args, Debug, Default)]
#[command(group(ArgGroup::new("transport").required(true).args(["command", "url"])))]
pub struct AddArgs {
    /// Server id: letters, digits, `-`, `_`.
    pub id: String,
    #[arg(long)]
    pub name: Option<String>,
    /// Program to run (stdio).
    #[arg(long)]
    pub command: Option<String>,
    /// Argument for the program; repeat for several. Anything after `--` is appended too.
    #[arg(long = "arg", allow_hyphen_values = true)]
    pub args: Vec<String>,
    /// `NAME=value`; repeat for several.
    #[arg(long = "env")]
    pub env: Vec<String>,
    #[arg(long)]
    pub cwd: Option<String>,
    /// Streamable HTTP endpoint.
    #[arg(long)]
    pub url: Option<String>,
    /// `Name=value`; repeat for several.
    #[arg(long = "header")]
    pub headers: Vec<String>,
    #[arg(long, value_enum, default_value_t = ModeArg::Shared)]
    pub mode: ModeArg,
    /// Expose only to this harness id; repeat for several. Default: every harness.
    #[arg(long = "harness")]
    pub harnesses: Vec<String>,
    /// Add it switched off.
    #[arg(long)]
    pub disabled: bool,
    /// Extra program arguments, after `--`.
    #[arg(last = true, allow_hyphen_values = true)]
    pub trailing: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, ValueEnum)]
pub enum ModeArg {
    #[default]
    Shared,
    Session,
}

pub fn parse_pair(s: &str) -> Result<(String, String)> {
    match s.split_once('=') {
        Some((k, v)) if !k.trim().is_empty() => Ok((k.trim().to_string(), v.to_string())),
        _ => bail!("expected NAME=value, got `{s}`"),
    }
}

fn pairs(list: &[String]) -> Result<BTreeMap<String, String>> {
    list.iter().map(|s| parse_pair(s)).collect()
}

/// Validates and builds the server config for `mcpm servers add`.
pub fn build_server(a: &AddArgs) -> Result<ServerConfig> {
    let id = a.id.trim().to_string();
    if !valid_server_id(&id) {
        bail!(
            "invalid id `{id}`: use letters, digits, `-` or `_` (max 48, start with a letter or digit)"
        );
    }
    let transport = match (&a.command, &a.url) {
        (Some(_), Some(_)) => bail!("use either --command or --url, not both"),
        (Some(cmd), None) => {
            if cmd.trim().is_empty() {
                bail!("--command is empty");
            }
            if !a.headers.is_empty() {
                bail!("--header only applies to --url servers");
            }
            let mut args = a.args.clone();
            args.extend(a.trailing.iter().cloned());
            Transport::Stdio {
                command: cmd.trim().to_string(),
                args,
                env: pairs(&a.env)?,
                cwd: a.cwd.clone().filter(|c| !c.trim().is_empty()),
            }
        }
        (None, Some(url)) => {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                bail!("--url must start with http:// or https://");
            }
            if !a.args.is_empty() || !a.env.is_empty() || !a.trailing.is_empty() {
                bail!("--arg/--env only apply to --command servers");
            }
            Transport::Http {
                url: url.clone(),
                headers: pairs(&a.headers)?,
            }
        }
        (None, None) => bail!("give --command or --url"),
    };
    Ok(ServerConfig {
        name: a
            .name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| id.clone()),
        id,
        transport,
        enabled: !a.disabled,
        mode: match a.mode {
            ModeArg::Shared => InstanceMode::Shared,
            ModeArg::Session => InstanceMode::Session,
        },
        pinned_tools: vec![],
        harnesses: a.harnesses.clone(),
        start_timeout_secs: None,
        idle_timeout_mins: None,
        origin: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("mcpm").chain(args.iter().copied()))
    }

    #[test]
    fn no_args_means_gui() {
        assert!(parse(&[]).unwrap().command.is_none());
    }

    #[test]
    fn daemon_flags() {
        let Some(Cmd::Daemon {
            port,
            no_tray,
            wait_pid,
        }) = parse(&["daemon", "--no-tray", "--port", "40000", "--wait-pid", "7"])
            .unwrap()
            .command
        else {
            panic!()
        };
        assert_eq!((port, no_tray, wait_pid), (Some(40000), true, Some(7)));
        let Some(Cmd::Daemon { no_tray, .. }) = parse(&["daemon"]).unwrap().command else {
            panic!()
        };
        assert!(!no_tray, "the tray is on by default");
    }

    #[test]
    fn update_flags() {
        let Some(Cmd::Update { check }) = parse(&["update", "--check"]).unwrap().command else {
            panic!()
        };
        assert!(check);
        let Some(Cmd::Update { check }) = parse(&["update"]).unwrap().command else {
            panic!()
        };
        assert!(!check);
    }

    #[test]
    fn json_flag_is_global() {
        assert!(parse(&["--json", "status"]).unwrap().json);
        assert!(parse(&["status", "--json"]).unwrap().json);
        assert!(!parse(&["status"]).unwrap().json);
    }

    #[test]
    fn takeover_needs_exactly_one_target() {
        assert!(parse(&["takeover"]).is_err());
        assert!(parse(&["takeover", "cursor", "--all"]).is_err());
        let Some(Cmd::Takeover {
            harness,
            all,
            dry_run,
            ..
        }) = parse(&["takeover", "cursor", "--dry-run"]).unwrap().command
        else {
            panic!()
        };
        assert_eq!(
            (harness.as_deref(), all, dry_run),
            (Some("cursor"), false, true)
        );
        assert!(parse(&["takeover", "--all"]).is_ok());
        assert!(
            parse(&["takeover", "--all", "--text"]).is_err(),
            "--text needs --dry-run"
        );
    }

    #[test]
    fn restore_targets() {
        assert!(parse(&["restore"]).is_err());
        assert!(parse(&["restore", "--all", "--keep"]).is_ok());
        assert!(parse(&["restore", "--snapshot", "1-ab"]).is_ok());
        assert!(parse(&["restore", "claude", "--snapshot", "1-ab"]).is_err());
    }

    #[test]
    fn servers_default_is_list_and_autostart_default_is_status() {
        let Some(Cmd::Servers { action }) = parse(&["servers"]).unwrap().command else {
            panic!()
        };
        assert!(action.is_none());
        let Some(Cmd::Autostart { state }) = parse(&["autostart"]).unwrap().command else {
            panic!()
        };
        assert_eq!(state, OnOff::Status);
        assert!(parse(&["autostart", "maybe"]).is_err());
    }

    #[test]
    fn add_stdio_with_hyphen_args_and_trailing() {
        let cli = parse(&[
            "servers",
            "add",
            "fs",
            "--command",
            "npx",
            "--arg",
            "-y",
            "--env",
            "K=a=b",
            "--harness",
            "claude",
            "--",
            "@x/server",
            "--flag",
        ])
        .unwrap();
        let Some(Cmd::Servers {
            action: Some(ServersCmd::Add(a)),
        }) = cli.command
        else {
            panic!()
        };
        let c = build_server(&a).unwrap();
        let Transport::Stdio {
            command, args, env, ..
        } = &c.transport
        else {
            panic!()
        };
        assert_eq!(command, "npx");
        assert_eq!(args, &["-y", "@x/server", "--flag"]);
        assert_eq!(env.get("K").map(String::as_str), Some("a=b"));
        assert_eq!(
            (c.name.as_str(), c.enabled, c.harnesses.clone()),
            ("fs", true, vec!["claude".to_string()])
        );
    }

    #[test]
    fn add_http_and_errors() {
        let a = AddArgs {
            id: "web".into(),
            url: Some("https://h/mcp".into()),
            headers: vec!["Authorization=Bearer x".into()],
            disabled: true,
            mode: ModeArg::Session,
            ..Default::default()
        };
        let c = build_server(&a).unwrap();
        assert!(!c.enabled);
        assert_eq!(c.mode, InstanceMode::Session);
        assert!(matches!(c.transport, Transport::Http { .. }));

        assert!(
            build_server(&AddArgs {
                id: "Bad Id".into(),
                command: Some("x".into()),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            build_server(&AddArgs {
                id: "a".into(),
                url: Some("ftp://x".into()),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            build_server(&AddArgs {
                id: "a".into(),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            build_server(&AddArgs {
                id: "a".into(),
                command: Some("x".into()),
                env: vec!["novalue".into()],
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            parse(&["servers", "add", "a"]).is_err(),
            "needs --command or --url"
        );
    }
}
