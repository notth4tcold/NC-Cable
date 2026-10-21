//! Cable configuration.
//!
//! The VB-CABLE driver reads these from `HKLM\Software\VB-Audio\Cable` at load time:
//! `VBAudioCableWDM` (max latency, >= 1024, default 7168 frames),
//! `VBAudioCableWDM_Latency` (>= 48, clamped to max), `VBAudioCableWDM_SR`
//! (internal rate, one of a fixed list). Here they live in a TOML file instead.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::ring::MAX_CHANNELS;

pub const RATES: [u32; 7] = [32000, 44100, 48000, 88200, 96000, 176400, 192000];
pub const MIN_LATENCY: u32 = 48;
pub const MIN_MAX_LATENCY: u32 = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CableConfig {
    /// Base name; the nodes are shown as "<name> Input" (playback) and "<name> Output" (recording).
    pub name: String,
    /// Internal sample rate. PipeWire resamples clients that use a different one.
    pub rate: u32,
    pub channels: u32,
    /// Target buffering between input and output, in frames.
    pub latency: u32,
    /// Ring buffer capacity, in frames.
    pub max_latency: u32,
}

impl Default for CableConfig {
    fn default() -> Self {
        CableConfig {
            name: "NC-Cable".into(),
            rate: 48000,
            channels: 2,
            latency: 1536,
            max_latency: 7168,
        }
    }
}

impl CableConfig {
    pub fn validate(&mut self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("cable name must not be empty");
        }
        if !RATES.contains(&self.rate) {
            bail!(
                "{}: rate {} not supported (use one of {:?})",
                self.name,
                self.rate,
                RATES
            );
        }
        if !(1..=MAX_CHANNELS as u32).contains(&self.channels) {
            bail!(
                "{}: channels must be between 1 and {MAX_CHANNELS}",
                self.name
            );
        }
        self.max_latency = self.max_latency.max(MIN_MAX_LATENCY);
        self.latency = self.latency.clamp(MIN_LATENCY, self.max_latency);
        Ok(())
    }

    /// Identifier used for node names and the stats file, e.g. "NC-Cable A" -> "nc_cable_a".
    pub fn slug(&self) -> String {
        slugify(&self.name)
    }

    pub fn latency_ms(&self) -> f64 {
        self.latency as f64 * 1000.0 / self.rate as f64
    }
}

pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('_') {
            slug.push('_');
        }
    }
    slug.trim_matches('_').to_string()
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, rename = "cable")]
    pub cables: Vec<CableConfig>,
}

impl Config {
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("nc-cable").join("config.toml")
    }

    /// Loads the file if it exists; a missing file means one default cable.
    pub fn load(path: &PathBuf) -> Result<Self> {
        let mut config = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<Config>(&text)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        if config.cables.is_empty() {
            config.cables.push(CableConfig::default());
        }
        let mut slugs = std::collections::HashSet::new();
        for cable in &mut config.cables {
            cable.validate()?;
            if !slugs.insert(cable.slug()) {
                bail!("duplicate cable name {:?}", cable.name);
            }
        }
        Ok(config)
    }
}

pub const EXAMPLE: &str = r#"# NC-Cable configuration
# Each [[cable]] creates a playback device "<name> Input" and a recording
# device "<name> Output". Everything played into the input comes out of the output.

[[cable]]
name = "NC-Cable"
rate = 48000        # internal rate: 32000 44100 48000 88200 96000 176400 192000
channels = 2        # 1..16
latency = 1536      # frames buffered between input and output (min 48)
max_latency = 7168  # ring buffer size in frames (min 1024)

# A second, independent cable:
# [[cable]]
# name = "NC-Cable B"
# channels = 8
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses() {
        let config: Config = toml::from_str(EXAMPLE).unwrap();
        assert_eq!(config.cables.len(), 1);
        assert_eq!(config.cables[0].latency, 1536);
    }

    #[test]
    fn validate_clamps_like_the_driver() {
        let mut c = CableConfig {
            latency: 10,
            max_latency: 100,
            ..Default::default()
        };
        c.validate().unwrap();
        assert_eq!((c.latency, c.max_latency), (48, 1024));
        let mut c = CableConfig {
            latency: 9000,
            ..Default::default()
        };
        c.validate().unwrap();
        assert_eq!(c.latency, 7168);
        assert!(
            CableConfig {
                rate: 22050,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            CableConfig {
                channels: 17,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("NC-Cable"), "nc_cable");
        assert_eq!(slugify("  My Cable #2 "), "my_cable_2");
    }
}
