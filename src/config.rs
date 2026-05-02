use ipnet::IpNet;
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

pub const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_CONCURRENT_TASKS: usize = 4;
pub const DEFAULT_READ_TIMEOUT_SECS: u64 = 30;
pub const CODEX_SESSIONS_EXTENSION_URI: &str = "urn:codex-a2a:extensions:codex-sessions:v1";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    Cli,
    Native,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Native => "native",
        }
    }
}

#[derive(Clone)]
pub struct Config {
    pub backend: BackendKind,
    pub host: String,
    pub port: u16,
    pub public_url: String,
    pub sessions_dir: PathBuf,
    pub default_working_directory: String,
    pub codex_bin: String,
    pub auth_token_env: Option<String>,
    pub auth_token_file: Option<PathBuf>,
    pub auth_token_keychain_service: Option<String>,
    pub write_roots: Vec<PathBuf>,
    pub write_mode_sandbox_permissions: Vec<String>,
    pub allowed_sources: Vec<IpNet>,
    pub max_body_bytes: usize,
    pub max_concurrent_tasks: usize,
    pub read_timeout_secs: u64,
    pub expose_local_sessions: bool,
}

#[derive(Deserialize)]
struct RawConfig {
    backend: Option<BackendKind>,
    host: Option<String>,
    port: Option<u16>,
    public_url: String,
    sessions_dir: String,
    default_working_directory: String,
    codex_bin: Option<String>,
    auth_token_env: Option<String>,
    auth_token_file: Option<String>,
    auth_token_keychain_service: Option<String>,
    write_roots: Option<Vec<String>>,
    write_mode_sandbox_permissions: Option<Vec<String>>,
    allowed_sources: Option<Vec<String>>,
    max_body_bytes: Option<usize>,
    max_concurrent_tasks: Option<usize>,
    read_timeout_secs: Option<u64>,
    expose_local_sessions: Option<bool>,
}

pub fn load_config() -> Result<Config, String> {
    let path = env::var("CODEX_A2A_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
    let content = fs::read_to_string(&path).map_err(|err| format!("{path}: {err}"))?;
    let raw: RawConfig = toml::from_str(&content).map_err(|err| format!("{path}: {err}"))?;

    let mut allowed_sources = Vec::new();
    for source in raw.allowed_sources.unwrap_or_default() {
        allowed_sources.push(
            IpNet::from_str(&source)
                .map_err(|err| format!("invalid allowed_sources entry {source}: {err}"))?,
        );
    }

    Ok(Config {
        backend: raw.backend.unwrap_or(BackendKind::Cli),
        host: raw.host.unwrap_or_else(|| "0.0.0.0".to_string()),
        port: raw.port.unwrap_or(18081),
        public_url: raw.public_url,
        sessions_dir: PathBuf::from(raw.sessions_dir),
        default_working_directory: raw.default_working_directory,
        codex_bin: raw.codex_bin.unwrap_or_else(|| "codex".to_string()),
        auth_token_env: raw
            .auth_token_env
            .or_else(|| Some("A2A_AUTH_TOKEN".to_string())),
        auth_token_file: raw.auth_token_file.map(PathBuf::from),
        auth_token_keychain_service: raw
            .auth_token_keychain_service
            .or_else(|| Some("A2A_AUTH_TOKEN".to_string())),
        write_roots: raw
            .write_roots
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        write_mode_sandbox_permissions: raw.write_mode_sandbox_permissions.unwrap_or_default(),
        allowed_sources,
        max_body_bytes: raw.max_body_bytes.unwrap_or(DEFAULT_MAX_BODY_BYTES),
        max_concurrent_tasks: raw
            .max_concurrent_tasks
            .unwrap_or(DEFAULT_MAX_CONCURRENT_TASKS),
        read_timeout_secs: raw.read_timeout_secs.unwrap_or(DEFAULT_READ_TIMEOUT_SECS),
        expose_local_sessions: raw.expose_local_sessions.unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::{BackendKind, RawConfig};

    fn test_path(name: &str) -> String {
        std::env::temp_dir()
            .join(name)
            .to_string_lossy()
            .replace('\\', "/")
    }

    #[test]
    fn raw_config_defaults_backend_to_cli_when_missing() {
        let sessions_dir = test_path("codex-a2a-test-sessions");
        let workspace = test_path("codex-a2a-test-workspace");
        let raw: RawConfig = toml::from_str(&format!(
            r#"
public_url = "http://127.0.0.1:18081"
sessions_dir = "{sessions_dir}"
default_working_directory = "{workspace}"
"#,
        ))
        .expect("parse raw config");

        assert_eq!(raw.backend.unwrap_or(BackendKind::Cli), BackendKind::Cli);
    }

    #[test]
    fn raw_config_parses_native_backend() {
        let sessions_dir = test_path("codex-a2a-test-sessions");
        let workspace = test_path("codex-a2a-test-workspace");
        let raw: RawConfig = toml::from_str(&format!(
            r#"
backend = "native"
public_url = "http://127.0.0.1:18081"
sessions_dir = "{sessions_dir}"
default_working_directory = "{workspace}"
"#,
        ))
        .expect("parse raw config");

        assert_eq!(raw.backend, Some(BackendKind::Native));
    }

    #[test]
    fn raw_config_parses_write_mode_sandbox_permissions() {
        let sessions_dir = test_path("codex-a2a-test-sessions");
        let workspace = test_path("codex-a2a-test-workspace");
        let raw: RawConfig = toml::from_str(&format!(
            r#"
public_url = "http://127.0.0.1:18081"
sessions_dir = "{sessions_dir}"
default_working_directory = "{workspace}"
write_mode_sandbox_permissions = ["disk-full-read-access"]
"#,
        ))
        .expect("parse raw config");

        assert_eq!(
            raw.write_mode_sandbox_permissions,
            Some(vec!["disk-full-read-access".to_string()])
        );
    }

    #[test]
    fn raw_config_parses_public_hardening_options() {
        let sessions_dir = test_path("codex-a2a-test-sessions");
        let workspace = test_path("codex-a2a-test-workspace");
        let token_file = test_path("codex-a2a-test-token");
        let raw: RawConfig = toml::from_str(&format!(
            r#"
public_url = "http://127.0.0.1:18081"
sessions_dir = "{sessions_dir}"
default_working_directory = "{workspace}"
auth_token_env = "A2A_AUTH_TOKEN"
auth_token_file = "{token_file}"
auth_token_keychain_service = "A2A_AUTH_TOKEN"
max_concurrent_tasks = 2
read_timeout_secs = 10
expose_local_sessions = true
"#,
        ))
        .expect("parse raw config");

        assert_eq!(raw.auth_token_env, Some("A2A_AUTH_TOKEN".to_string()));
        assert_eq!(raw.auth_token_file, Some(token_file));
        assert_eq!(
            raw.auth_token_keychain_service,
            Some("A2A_AUTH_TOKEN".to_string())
        );
        assert_eq!(raw.max_concurrent_tasks, Some(2));
        assert_eq!(raw.read_timeout_secs, Some(10));
        assert_eq!(raw.expose_local_sessions, Some(true));
    }
}
