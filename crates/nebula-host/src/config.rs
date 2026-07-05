//! Host configuration: a small TOML file in the platform config directory.
//!
//! Secure defaults: TLS on, audio off, clipboard off, input requires
//! per-device authorization, no telemetry (there is no telemetry code at all).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Human-readable host name shown to viewers and in discovery replies.
    pub host_name: String,
    /// Bind address for the HTTPS/WebSocket server.
    pub bind: String,
    /// TCP port for the HTTPS/WebSocket server.
    pub port: u16,
    /// TLS on by default. Turning this off is only sensible for loopback tests.
    pub tls: bool,
    /// UDP discovery responder on/off.
    pub discovery: bool,
    /// Frame source: "auto" (screen on Windows, test elsewhere), "screen", "test".
    pub frame_source: String,
    /// Directory with the built web UI. When unset, well-known relative
    /// locations are probed (`viewer/web/dist`, `./web`).
    pub web_dir: Option<String>,
    /// System audio streaming master switch. Off by default (privacy).
    pub audio_enabled: bool,
    /// Clipboard sync master switch. Off by default (privacy).
    pub clipboard_enabled: bool,
    /// Default performance profile for new sessions.
    pub default_profile: String,
    /// Maximum simultaneously connected streaming clients.
    pub max_clients: u32,
    /// Hard FPS ceiling applied on top of any profile (host protection).
    pub max_fps: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host_name: hostname_or_default(),
            bind: "0.0.0.0".into(),
            port: nebula_proto::DEFAULT_PORT,
            tls: true,
            discovery: true,
            frame_source: "auto".into(),
            web_dir: None,
            audio_enabled: false,
            clipboard_enabled: false,
            default_profile: "balanced".into(),
            max_clients: 8,
            max_fps: 120,
        }
    }
}

fn hostname_or_default() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "NebulaDisplay Host".into())
}

impl Config {
    /// Platform config file path, e.g.
    /// `%APPDATA%/NebulaDisplay/host.toml` or `~/.config/nebuladisplay/host.toml`.
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("nebuladisplay")
            .join("host.toml")
    }

    pub fn load_or_default(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(toml::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Directory that holds config, trust store, and TLS material.
    pub fn data_dir(config_path: &Path) -> PathBuf {
        config_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_private() {
        let c = Config::default();
        assert!(c.tls, "TLS must default to on");
        assert!(!c.audio_enabled, "audio must default to off");
        assert!(!c.clipboard_enabled, "clipboard must default to off");
    }

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("nebula-cfg-test-{}", std::process::id()));
        let path = dir.join("host.toml");
        let c = Config {
            port: 12345,
            host_name: "Test Host".into(),
            ..Config::default()
        };
        c.save(&path).unwrap();
        let l = Config::load_or_default(&path).unwrap();
        assert_eq!(l.port, 12345);
        assert_eq!(l.host_name, "Test Host");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn missing_file_gives_defaults() {
        let c = Config::load_or_default(Path::new("/definitely/not/here.toml")).unwrap();
        assert_eq!(c.port, nebula_proto::DEFAULT_PORT);
    }
}
