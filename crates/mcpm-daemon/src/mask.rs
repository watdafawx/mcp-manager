//! Secret masking for config views, meta-tool output and error text.

use std::collections::BTreeMap;

use mcpm_core::model::{ServerConfig, Transport};

pub const MASK: &str = "••••";

fn is_secret_key(k: &str) -> bool {
    let k = k.trim_start_matches('-').to_ascii_lowercase();
    [
        "token", "secret", "password", "passwd", "apikey", "api-key", "api_key", "auth", "bearer",
        "key",
    ]
    .iter()
    .any(|w| k.contains(w))
}

/// Masks `--token=abc`, `API_KEY=abc` and the value after a bare `--api-key` flag.
pub fn mask_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut mask_next = false;
    for a in args {
        if mask_next {
            out.push(MASK.to_string());
            mask_next = false;
        } else if let Some((k, _)) = a.split_once('=') {
            if is_secret_key(k) {
                out.push(format!("{k}={MASK}"))
            } else {
                out.push(a.clone())
            }
        } else {
            mask_next = a.starts_with('-') && is_secret_key(a);
            out.push(a.clone());
        }
    }
    out
}

/// Masks userinfo and query values: `https://u:p@h/p?key=abc` -> `https://••••@h/p?key=••••`.
pub fn mask_url(url: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    let mut base = base.to_string();
    if let Some(scheme_end) = base.find("://") {
        let rest_start = scheme_end + 3;
        let rest = &base[rest_start..];
        let authority_end = rest.find('/').unwrap_or(rest.len());
        if let Some(at) = rest[..authority_end].rfind('@') {
            base = format!("{}{}{}", &base[..rest_start], MASK, &rest[at..]);
        }
    }
    match query {
        None => base,
        Some(q) => {
            let masked: Vec<String> = q
                .split('&')
                .map(|kv| match kv.split_once('=') {
                    Some((k, _)) => format!("{k}={MASK}"),
                    None => kv.to_string(),
                })
                .collect();
            format!("{base}?{}", masked.join("&"))
        }
    }
}

fn mask_map(m: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    m.keys().map(|k| (k.clone(), MASK.to_string())).collect()
}

/// A copy of `cfg` safe to show: env/header values, URL secrets and secret-looking args hidden.
pub fn mask_config(cfg: &ServerConfig) -> ServerConfig {
    let mut c = cfg.clone();
    c.transport = match &cfg.transport {
        Transport::Stdio {
            command,
            args,
            env,
            cwd,
        } => Transport::Stdio {
            command: command.clone(),
            args: mask_args(args),
            env: mask_map(env),
            cwd: cwd.clone(),
        },
        Transport::Http { url, headers } => Transport::Http {
            url: mask_url(url),
            headers: mask_map(headers),
        },
    };
    c
}

/// Every secret string in `cfg` (used to scrub error text).
pub fn secrets(cfg: &ServerConfig) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        let s = s.trim();
        if s.len() >= 4 {
            out.push(s.to_string());
        }
    };
    match &cfg.transport {
        Transport::Stdio { args, env, .. } => {
            env.values().for_each(|v| push(v));
            let masked = mask_args(args);
            for (orig, m) in args.iter().zip(masked.iter()) {
                if orig != m {
                    push(orig.split_once('=').map_or(orig.as_str(), |(_, v)| v));
                }
            }
        }
        Transport::Http { url, headers } => {
            for v in headers.values() {
                push(v);
                if let Some((_, tok)) = v.split_once(' ') {
                    push(tok);
                }
            }
            if let Some((_, q)) = url.split_once('?') {
                q.split('&')
                    .filter_map(|kv| kv.split_once('='))
                    .for_each(|(_, v)| push(v));
            }
        }
    }
    // Longest first so a secret that contains another is replaced whole.
    out.sort_by_key(|s| std::cmp::Reverse(s.len()));
    out.dedup();
    out
}

pub fn mask_text(text: &str, secrets: &[String]) -> String {
    let mut t = text.to_string();
    for s in secrets {
        if t.contains(s.as_str()) {
            t = t.replace(s.as_str(), MASK);
        }
    }
    t
}

/// On update, a masked placeholder from a GUI round trip means "keep the stored value".
pub fn restore_masked(old: &ServerConfig, new: &mut ServerConfig) {
    match (&old.transport, &mut new.transport) {
        (
            Transport::Stdio {
                args: oa, env: oe, ..
            },
            Transport::Stdio {
                args: na, env: ne, ..
            },
        ) => {
            for (k, v) in ne.iter_mut() {
                if v == MASK
                    && let Some(ov) = oe.get(k)
                {
                    *v = ov.clone();
                }
            }
            if *na == mask_args(oa) {
                *na = oa.clone();
            }
        }
        (
            Transport::Http {
                url: ou,
                headers: oh,
            },
            Transport::Http {
                url: nu,
                headers: nh,
            },
        ) => {
            for (k, v) in nh.iter_mut() {
                if v == MASK
                    && let Some(ov) = oh.get(k)
                {
                    *v = ov.clone();
                }
            }
            if *nu == mask_url(ou) {
                *nu = ou.clone();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcpm_core::model::InstanceMode;

    fn stdio(env: &[(&str, &str)], args: &[&str]) -> ServerConfig {
        ServerConfig {
            id: "x".into(),
            name: "x".into(),
            transport: Transport::Stdio {
                command: "cmd".into(),
                args: args.iter().map(|s| s.to_string()).collect(),
                env: env
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                cwd: None,
            },
            enabled: true,
            mode: InstanceMode::Shared,
            pinned_tools: vec![],
            harnesses: vec![],
            start_timeout_secs: None,
            origin: None,
        }
    }

    #[test]
    fn masks_env_and_args() {
        let c = stdio(
            &[("TOKEN", "supersecret")],
            &["--api-key", "abc123", "--token=zzz999", "--verbose"],
        );
        let m = mask_config(&c);
        let Transport::Stdio { args, env, .. } = &m.transport else {
            panic!()
        };
        assert_eq!(env["TOKEN"], MASK);
        assert_eq!(
            args,
            &["--api-key", MASK, &format!("--token={MASK}"), "--verbose"]
        );
        let s = secrets(&c);
        assert!(s.contains(&"supersecret".to_string()) && s.contains(&"abc123".to_string()));
        assert_eq!(
            mask_text("boom supersecret abc123", &s),
            format!("boom {MASK} {MASK}")
        );
    }

    #[test]
    fn masks_url() {
        assert_eq!(
            mask_url("https://u:p@h.io/p?key=abc&x=1"),
            format!("https://{MASK}@h.io/p?key={MASK}&x={MASK}")
        );
        assert_eq!(mask_url("http://h/p"), "http://h/p");
    }

    #[test]
    fn restores_placeholders() {
        let old = stdio(&[("TOKEN", "supersecret")], &["--api-key", "abc123"]);
        let mut new = mask_config(&old);
        restore_masked(&old, &mut new);
        assert_eq!(new, old);
    }
}
