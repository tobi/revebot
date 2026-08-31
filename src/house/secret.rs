//! Persist `AskUserForSecret` into `config.yml` and the host-side store.
//!
//! The guest never receives the value. Config stores a source reference and
//! host scope; paste lives under `.reve/secrets/` (gitignored).

use std::path::Path;

use crate::sandbox::Secret;

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

pub fn validate_env(name: &str) -> Result<(), String> {
    let n = name.trim();
    if n.is_empty() || n.len() > 64 {
        return Err("env name must be 1–64 characters".into());
    }
    let mut chars = n.chars();
    let first = chars.next().unwrap();
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
    let mut hosts: Vec<String> = decision
        .hosts
        .iter()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .collect();
    hosts.sort();
    hosts.dedup();
    if hosts.is_empty() {
        return Err(
            "at least one host is required; the VM must not hold an unscoped secret".into(),
        );
    }
    let env = decision.env.trim().to_string();
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
        header: decision
            .header
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        prefix: decision
            .prefix
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
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

pub fn upsert_config_yml(text: &str, secret: &Secret) -> Result<String, String> {
    let mut doc: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|e| format!("config.yml: {e}"))?;
    let root = doc.as_mapping_mut().ok_or("config.yml must be a mapping")?;
    let sandbox_key = serde_yaml::Value::String("sandbox".into());
    if !root.contains_key(&sandbox_key) {
        root.insert(
            sandbox_key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    let sandbox = root
        .get_mut(&sandbox_key)
        .and_then(serde_yaml::Value::as_mapping_mut)
        .ok_or("sandbox must be a mapping")?;
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
    let hosts = secret
        .hosts
        .iter()
        .cloned()
        .map(serde_yaml::Value::String)
        .collect();
    m.insert("hosts".into(), serde_yaml::Value::Sequence(hosts));
    if let Some(h) = &secret.header {
        m.insert("header".into(), h.clone().into());
    }
    if let Some(p) = &secret.prefix {
        m.insert("prefix".into(), p.clone().into());
    }
    serde_yaml::Value::Mapping(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_appends_a_secret_to_the_template() {
        let text = include_str!("../templates/config.yml");
        let secret = Secret {
            env: "ACME_TOKEN".into(),
            source: "file:.reve/secrets/ACME_TOKEN".into(),
            placeholder: Some("reve-acme-token".into()),
            hosts: vec!["api.acme.test".into()],
            header: Some("Authorization".into()),
            prefix: Some("Bearer".into()),
        };
        let out = upsert_config_yml(text, &secret).unwrap();
        assert!(out.contains("ACME_TOKEN"));
        assert!(out.contains("api.acme.test"));
        assert!(out.contains("file:.reve/secrets/ACME_TOKEN"));
        assert!(out.contains("GITHUB_TOKEN"), "existing secrets stay");
        let again = upsert_config_yml(&out, &secret).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&again).unwrap();
        let secrets = parsed["sandbox"]["secrets"].as_sequence().unwrap();
        let acme: Vec<_> = secrets
            .iter()
            .filter(|s| s["env"].as_str() == Some("ACME_TOKEN"))
            .collect();
        assert_eq!(acme.len(), 1, "upsert replaces the same env");
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
        let yaml = upsert_config_yml("sandbox: {}\n", &secret).unwrap();
        assert!(!yaml.contains("super-secret"));
        assert!(yaml.contains("file:.reve/secrets/DEMO_KEY"));
    }
}
