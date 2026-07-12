//! Host configuration: CLI args > config file > defaults. Persisted as TOML
//! in the data directory alongside the trust store and identity key.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Subset of CLI arguments the config loader needs (keeps the lib decoupled
/// from clap).
#[derive(Debug, Default, Clone)]
pub struct LoadArgs {
    pub name: Option<String>,
    pub data_dir: Option<std::path::PathBuf>,
    pub web_dir: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    /// Host display name.
    pub name: Option<String>,
    /// PIN length in digits.
    pub pin_digits: u32,
    /// PIN validity window in seconds (single-use regardless).
    pub pin_ttl_secs: u64,
    /// Max failed pairing attempts per IP before lockout.
    pub max_pin_attempts: u32,
    /// Lockout duration in seconds.
    pub lockout_secs: u64,
    /// Default max FPS cap applied on top of profiles.
    pub max_fps: u32,
    /// Refuse legacy (pre-PAKE) PIN pairing. Off by default so old viewers
    /// can still pair; turn on once every device runs a PAKE-capable viewer.
    pub require_pake: bool,
    /// Stream host audio (WASAPI loopback) to sessions that request it.
    /// Off by default — a visible privacy toggle, like input grants.
    pub audio: bool,
    /// Byte cap for a single clipboard sync payload (either direction).
    pub clipboard_max_bytes: usize,
    /// Byte cap for a single dropped file.
    pub file_max_bytes: u64,
    /// Where accepted file drops are stored. Default: `<data_dir>/downloads`.
    pub file_dir: Option<std::path::PathBuf>,
    /// Serve the viewer endpoint over HTTPS with a persisted self-signed
    /// certificate (printed fingerprint). Gives browsers a secure context
    /// (native WebCrypto/WebCodecs) at the cost of a one-time warning.
    pub https: bool,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            name: None,
            pin_digits: 6,
            pin_ttl_secs: 300,
            max_pin_attempts: 5,
            lockout_secs: 300,
            max_fps: 60,
            require_pake: false,
            audio: false,
            clipboard_max_bytes: 256 * 1024,
            file_max_bytes: 2 * 1024 * 1024 * 1024, // 2 GiB
            file_dir: None,
            https: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub data_dir: PathBuf,
    pub web_dir: Option<PathBuf>,
    pub file: FileConfig,
}

impl Config {
    pub fn load(args: &LoadArgs) -> anyhow::Result<Self> {
        let data_dir = match &args.data_dir {
            Some(d) => d.clone(),
            None => default_data_dir(),
        };
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("creating data dir {}", data_dir.display()))?;

        let cfg_path = data_dir.join("config.toml");
        let file: FileConfig = if cfg_path.exists() {
            let raw = std::fs::read_to_string(&cfg_path)?;
            toml::from_str(&raw).with_context(|| format!("parsing {}", cfg_path.display()))?
        } else {
            let d = FileConfig::default();
            // Write a commented default so users can discover the knobs.
            std::fs::write(&cfg_path, toml::to_string_pretty(&d)?)?;
            d
        };

        let name = args
            .name
            .clone()
            .or_else(|| file.name.clone())
            .or_else(hostname)
            .unwrap_or_else(|| "NebulaDisplay Host".into());

        let web_dir = args.web_dir.clone().or_else(find_web_dir);

        Ok(Self {
            name,
            data_dir,
            web_dir,
            file,
        })
    }
}

fn default_data_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(|p| PathBuf::from(p).join("NebulaDisplay"))
            .unwrap_or_else(|| PathBuf::from("nebuladisplay-data"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|p| p.join("nebuladisplay"))
            .unwrap_or_else(|| PathBuf::from("nebuladisplay-data"))
    }
}

fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

/// Look for the built web viewer near the executable or the repo layout so
/// `cargo run` works out of the box after `npm run build`.
fn find_web_dir() -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = [
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("web"))),
        Some(PathBuf::from("viewer/web/dist")),
        Some(PathBuf::from("../viewer/web/dist")),
        Some(PathBuf::from("../../viewer/web/dist")),
    ]
    .into_iter()
    .flatten()
    .collect();
    candidates.into_iter().find(|p| is_web_dir(p))
}

fn is_web_dir(p: &Path) -> bool {
    p.join("index.html").is_file()
}
