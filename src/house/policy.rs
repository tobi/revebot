//! `AskUserSandboxPolicyChange`: the user's decision and its persistence
//! into `config.yml` (and, for pasted secrets, the host-side store).
//!
//! A request may allow egress hosts, add a host-scoped secret, or both. The
//! guest never receives a secret value: config stores a source reference and
//! host scope; paste lives under `.reve/secrets/` (gitignored).

use std::collections::BTreeMap;
use std::path::Path;

use crate::sandbox::{Secret, SecretHost};

/// What the user answered to one `AskUserSandboxPolicyChange` card.
#[derive(Debug, Clone)]
pub struct PolicyDecision {
    pub accept: bool,
    /// Hosts to allow egress to (validated by [`normalize_hosts`]).
    pub hosts: Vec<String>,
    /// A scoped secret to add, when the request asked for one.
    pub secret: Option<SecretDecision>,
}

impl PolicyDecision {
    pub fn declined() -> Self {
        Self {
            accept: false,
            hosts: Vec::new(),
            secret: None,
        }
    }
}

/// The secret half of a decision.
#[derive(Debug, Clone)]
pub struct SecretDecision {
    pub accept: bool,
    pub env: String,
    pub kind: SecretKind,
    pub source: String,
    pub value: String,
    pub hosts: Vec<String>,
    pub header: Option<String>,
    pub prefix: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKind {
    Paste,
    Env,
    Command,
    Http,
}

impl SecretKind {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "paste" => Ok(Self::Paste),
            "env" => Ok(Self::Env),
            "command" => Ok(Self::Command),
            "http" => Ok(Self::Http),
            other => Err(format!("unknown secret kind {other}")),
        }
    }
}

/// Validate and canonicalize a hostname the model or user typed: lowercase,
/// no scheme/path/port/trailing dot, DNS label rules, at least two labels.
/// Wildcards are rejected: the allow list names hosts, not patterns.
pub fn normalize_host(raw: &str) -> Result<String, String> {
    let mut host = raw.trim();
    if let Some((_, rest)) = host.split_once("://") {
        host = rest;
    }
    host = host.split(['/', '?', '#']).next().unwrap_or("");
    if let Some(stripped) = host.strip_prefix('[') {
        return Err(format!(
            "`{stripped}`: IP literals are not allowed; name the host"
        ));
    }
    if let Some((h, port)) = host.rsplit_once(':')
        && port.chars().all(|c| c.is_ascii_digit())
    {
        host = h;
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return Err("host name is empty".into());
    }
    if host.len() > 253 {
        return Err(format!("`{host}`: host name too long"));
    }
    if host.contains('*') {
        return Err(format!(
            "`{host}`: wildcards are not allowed; name each host"
        ));
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Err(format!(
            "`{host}`: IP addresses are not allowed; name the host"
        ));
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return Err(format!("`{host}`: needs a domain, e.g. api.example.com"));
    }
    for label in &labels {
        let ok = !label.is_empty()
            && label.len() <= 63
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !label.starts_with('-')
            && !label.ends_with('-');
        if !ok {
            return Err(format!("`{host}`: `{label}` is not a valid DNS label"));
        }
    }
    Ok(host)
}

/// [`normalize_host`] over a list: deduplicated, sorted, first error wins.
pub fn normalize_hosts<I, S>(hosts: I) -> Result<Vec<String>, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = Vec::new();
    for host in hosts {
        let host = host.as_ref();
        if host.trim().is_empty() {
            continue;
        }
        out.push(normalize_host(host)?);
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Add `hosts` to `sandbox.allow` in `config.yml`, keeping everything else.
/// Existing entries are kept as written; the result is deduplicated.
pub fn upsert_allow_config_yml(text: &str, hosts: &[String]) -> Result<String, String> {
    let mut doc: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|e| format!("config.yml: {e}"))?;
    let sandbox = sandbox_mapping(&mut doc)?;
    let allow_key = serde_yaml::Value::String("allow".into());
    if !sandbox.contains_key(&allow_key) {
        sandbox.insert(allow_key.clone(), serde_yaml::Value::Sequence(Vec::new()));
    }
    let list = sandbox
        .get_mut(&allow_key)
        .and_then(serde_yaml::Value::as_sequence_mut)
        .ok_or("sandbox.allow must be a list")?;
    for host in hosts {
        let present = list
            .iter()
            .any(|item| item.as_str().is_some_and(|s| s.eq_ignore_ascii_case(host)));
        if !present {
            list.push(serde_yaml::Value::String(host.clone()));
        }
    }
    serde_yaml::to_string(&doc).map_err(|e| e.to_string())
}

fn sandbox_mapping(doc: &mut serde_yaml::Value) -> Result<&mut serde_yaml::Mapping, String> {
    if doc.is_null() {
        *doc = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }
    let root = doc.as_mapping_mut().ok_or("config.yml must be a mapping")?;
    let sandbox_key = serde_yaml::Value::String("sandbox".into());
    if !root.contains_key(&sandbox_key) {
        root.insert(
            sandbox_key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    root.get_mut(&sandbox_key)
        .and_then(serde_yaml::Value::as_mapping_mut)
        .ok_or_else(|| "sandbox must be a mapping".to_string())
}

pub fn validate_env(name: &str) -> Result<(), String> {
    let n = name.trim();
    if n.is_empty() || n.len() > 64 {
        return Err("env name must be 1–64 characters".into());
    }
    let Some(first) = n.chars().next() else {
        return Err("env name must be 1–64 characters".into());
    };
    if !first.is_ascii_alphabetic() && first != '_' {
        return Err("env must start with a letter or underscore".into());
    }
    if !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("env must be ASCII letters, digits, underscore".into());
    }
    Ok(())
}

pub fn to_secret(house_root: &Path, decision: &SecretDecision) -> Result<Secret, String> {
    validate_env(&decision.env)?;
    let env = decision.env.trim().to_string();
    let headers = host_headers(&env, decision.header.as_deref(), decision.prefix.as_deref());
    let mut hosts = BTreeMap::new();
    for host in &decision.hosts {
        let name = host.trim();
        if name.is_empty() {
            continue;
        }
        hosts.insert(
            name.to_string(),
            SecretHost {
                allow: true,
                headers: headers.clone(),
            },
        );
    }
    if hosts.is_empty() {
        return Err(
            "at least one host is required; the VM must not hold an unscoped secret".into(),
        );
    }
    let source = match decision.kind {
        SecretKind::Paste => {
            if decision.value.trim().is_empty() {
                return Err("paste a secret value".into());
            }
            write_store(house_root, &env, decision.value.trim())?;
            format!("file:.reve/secrets/{env}")
        }
        SecretKind::Env => {
            let src = decision.source.trim();
            if src.is_empty() {
                return Err("host environment variable name required".into());
            }
            src.to_string()
        }
        SecretKind::Command => {
            let cmd = decision.source.trim();
            let wrapped = if cmd.starts_with("$(") {
                cmd.to_string()
            } else {
                format!("$({cmd})")
            };
            crate::sandbox::command_secret_source(&wrapped).ok_or_else(|| {
                "command must be $(command) with no nested substitution".to_string()
            })?;
            wrapped
        }
        SecretKind::Http => {
            let url = decision.source.trim();
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err("http source must be an http(s) URL".into());
            }
            if url
                .chars()
                .any(|c| c.is_ascii_whitespace() || matches!(c, ';' | '|' | '&' | '`'))
            {
                return Err("http URL contains illegal characters".into());
            }
            url.to_string()
        }
    };
    let placeholder = Some(format!(
        "reve-{}",
        env.to_ascii_lowercase().replace('_', "-")
    ));
    Ok(Secret {
        env,
        source,
        placeholder,
        hosts,
    })
}

fn host_headers(env: &str, header: Option<&str>, prefix: Option<&str>) -> BTreeMap<String, String> {
    let Some(name) = header.map(str::trim).filter(|s| !s.is_empty()) else {
        return BTreeMap::new();
    };
    let value = match prefix.map(str::trim).filter(|s| !s.is_empty()) {
        Some(prefix) => format!("{prefix} ${env}"),
        None => format!("${env}"),
    };
    BTreeMap::from([(name.to_string(), value)])
}

fn write_store(house_root: &Path, env: &str, value: &str) -> Result<(), String> {
    let dir = house_root.join(".reve/secrets");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(env);
    std::fs::write(&path, value).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Upsert `secret` (by `env`) into `sandbox.secrets` in `config.yml`.
pub fn upsert_secret_config_yml(text: &str, secret: &Secret) -> Result<String, String> {
    let mut doc: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|e| format!("config.yml: {e}"))?;
    let sandbox = sandbox_mapping(&mut doc)?;
    let secrets_key = serde_yaml::Value::String("secrets".into());
    if !sandbox.contains_key(&secrets_key) {
        sandbox.insert(secrets_key.clone(), serde_yaml::Value::Sequence(Vec::new()));
    }
    let list = sandbox
        .get_mut(&secrets_key)
        .and_then(serde_yaml::Value::as_sequence_mut)
        .ok_or("sandbox.secrets must be a list")?;
    list.retain(|item| item.get("env").and_then(|v| v.as_str()) != Some(secret.env.as_str()));
    list.push(secret_yaml(secret));
    serde_yaml::to_string(&doc).map_err(|e| e.to_string())
}

fn secret_yaml(secret: &Secret) -> serde_yaml::Value {
    let mut m = serde_yaml::Mapping::new();
    m.insert("env".into(), secret.env.clone().into());
    m.insert("source".into(), secret.source.clone().into());
    if let Some(p) = &secret.placeholder {
        m.insert("placeholder".into(), p.clone().into());
    }
    let mut hosts = serde_yaml::Mapping::new();
    for (name, cfg) in &secret.hosts {
        let mut host = serde_yaml::Mapping::new();
        host.insert("allow".into(), cfg.allow.into());
        if !cfg.headers.is_empty() {
            let mut headers = serde_yaml::Mapping::new();
            for (header, value) in &cfg.headers {
                headers.insert(header.clone().into(), value.clone().into());
            }
            host.insert("headers".into(), serde_yaml::Value::Mapping(headers));
        }
        hosts.insert(name.clone().into(), serde_yaml::Value::Mapping(host));
    }
    m.insert("hosts".into(), serde_yaml::Value::Mapping(hosts));
    serde_yaml::Value::Mapping(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_appends_a_secret_to_the_template() {
        let text = include_str!("../templates/config.yml");
        let mut hosts = BTreeMap::new();
        hosts.insert(
            "api.acme.test".into(),
            SecretHost {
                allow: true,
                headers: BTreeMap::from([("Authorization".into(), "Bearer $ACME_TOKEN".into())]),
            },
        );
        let secret = Secret {
            env: "ACME_TOKEN".into(),
            source: "file:.reve/secrets/ACME_TOKEN".into(),
            placeholder: Some("reve-acme-token".into()),
            hosts,
        };
        let out = upsert_secret_config_yml(text, &secret).unwrap();
        assert!(out.contains("ACME_TOKEN"));
        assert!(out.contains("api.acme.test"));
        assert!(out.contains("file:.reve/secrets/ACME_TOKEN"));
        assert!(out.contains("GITHUB_TOKEN"), "existing secrets stay");
        assert!(out.contains("Bearer $ACME_TOKEN"));
        let again = upsert_secret_config_yml(&out, &secret).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&again).unwrap();
        let secrets = parsed["sandbox"]["secrets"].as_sequence().unwrap();
        let acme: Vec<_> = secrets
            .iter()
            .filter(|s| s["env"].as_str() == Some("ACME_TOKEN"))
            .collect();
        assert_eq!(acme.len(), 1, "upsert replaces the same env");
        assert!(acme[0].get("header").is_none());
        assert!(acme[0].get("prefix").is_none());
        assert_eq!(
            acme[0]["hosts"]["api.acme.test"]["allow"].as_bool(),
            Some(true)
        );
        assert_eq!(
            acme[0]["hosts"]["api.acme.test"]["headers"]["Authorization"].as_str(),
            Some("Bearer $ACME_TOKEN")
        );
    }

    #[test]
    fn hosts_are_canonicalized_and_junk_is_rejected() {
        assert_eq!(
            normalize_host(" API.Example.com. ").unwrap(),
            "api.example.com"
        );
        assert_eq!(
            normalize_host("https://pypi.org:443/simple/").unwrap(),
            "pypi.org"
        );
        for bad in [
            "",
            "localhost",
            "*.example.com",
            "10.0.0.1",
            "[::1]",
            "-a.example.com",
            "a b.com",
        ] {
            assert!(normalize_host(bad).is_err(), "{bad:?} must be rejected");
        }
        let hosts =
            normalize_hosts(["b.example.com", "A.example.com", "", "b.example.com"]).unwrap();
        assert_eq!(hosts, vec!["a.example.com", "b.example.com"]);
    }

    #[test]
    fn allow_upsert_extends_the_list_once_and_keeps_the_rest() {
        let text = "model: x\nsandbox:\n  open: false\n  allow:\n    - github.com\n";
        let hosts = vec!["pypi.org".to_string(), "GitHub.com".to_string()];
        let out = upsert_allow_config_yml(text, &hosts).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
        let allow: Vec<&str> = parsed["sandbox"]["allow"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            allow,
            vec!["github.com", "pypi.org"],
            "case-insensitive dedupe"
        );
        assert_eq!(
            parsed["sandbox"]["open"].as_bool(),
            Some(false),
            "other keys stay"
        );
        assert_eq!(parsed["model"].as_str(), Some("x"));
        let again = upsert_allow_config_yml(&out, &hosts).unwrap();
        assert_eq!(again, out, "idempotent");
        // A config without a sandbox section, or an empty file, gets one.
        let fresh = upsert_allow_config_yml("", &hosts).unwrap();
        assert!(fresh.contains("pypi.org"));
    }

    #[test]
    fn paste_writes_a_host_file_not_the_value_into_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let decision = SecretDecision {
            accept: true,
            env: "DEMO_KEY".into(),
            kind: SecretKind::Paste,
            source: String::new(),
            value: "super-secret".into(),
            hosts: vec!["example.com".into()],
            header: Some("Authorization".into()),
            prefix: Some("Bearer".into()),
        };
        let secret = to_secret(dir.path(), &decision).unwrap();
        assert_eq!(secret.source, "file:.reve/secrets/DEMO_KEY");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".reve/secrets/DEMO_KEY")).unwrap(),
            "super-secret"
        );
        let host = secret.hosts.get("example.com").unwrap();
        assert!(host.allow);
        assert_eq!(
            host.headers.get("Authorization").map(String::as_str),
            Some("Bearer $DEMO_KEY")
        );
        let yaml = upsert_secret_config_yml("sandbox: {}\n", &secret).unwrap();
        assert!(!yaml.contains("super-secret"));
        assert!(yaml.contains("file:.reve/secrets/DEMO_KEY"));
        assert!(yaml.contains("Bearer $DEMO_KEY"));
    }
}
