//! Publishes cable counters to `$XDG_RUNTIME_DIR/nc-cable/<slug>.json` so that
//! `nc-cable status` can show them, like the VB-CABLE control panel does.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use crate::config::CableConfig;
use crate::ring::Ring;

#[derive(Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub name: String,
    pub pid: u32,
    pub rate: u32,
    pub channels: u32,
    pub latency: u32,
    pub max_latency: u32,
    pub fill: usize,
    pub input_connected: bool,
    pub output_connected: bool,
    pub push_calls: u64,
    pub pull_calls: u64,
    pub push_loss: u64,
    pub pull_loss: u64,
    pub overflows: u64,
    pub underruns: u64,
    pub resyncs: u64,
    pub quantum_hist: [u64; 4],
    pub latency_warnings: u64,
    pub last_quantum_in: u32,
    pub last_quantum_out: u32,
    pub peak_in: Vec<f32>,
    pub peak_out: Vec<f32>,
}

pub fn snapshot(
    cfg: &CableConfig,
    ring: &Ring,
    input_connected: bool,
    output_connected: bool,
) -> Snapshot {
    let s = &ring.stats;
    let load = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
    let take_peaks = |peaks: &[std::sync::atomic::AtomicU32]| {
        peaks[..ring.channels()]
            .iter()
            .map(|p| f32::from_bits(p.swap(0, Ordering::Relaxed)))
            .collect()
    };
    Snapshot {
        name: cfg.name.clone(),
        pid: std::process::id(),
        rate: cfg.rate,
        channels: cfg.channels,
        latency: cfg.latency,
        max_latency: cfg.max_latency,
        fill: ring.fill(),
        input_connected,
        output_connected,
        push_calls: load(&s.push_calls),
        pull_calls: load(&s.pull_calls),
        push_loss: load(&s.push_loss),
        pull_loss: load(&s.pull_loss),
        overflows: load(&s.overflows),
        underruns: load(&s.underruns),
        resyncs: load(&s.resyncs),
        quantum_hist: std::array::from_fn(|i| load(&s.quantum_hist[i])),
        latency_warnings: load(&s.latency_warnings),
        last_quantum_in: s.last_quantum_in.load(Ordering::Relaxed),
        last_quantum_out: s.last_quantum_out.load(Ordering::Relaxed),
        peak_in: take_peaks(&s.peak_in),
        peak_out: take_peaks(&s.peak_out),
    }
}

pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("nc-cable")
}

pub fn publish(snap: &Snapshot, slug: &str) -> Result<()> {
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".{slug}.json.tmp"));
    std::fs::write(&tmp, serde_json::to_vec(snap)?)?;
    std::fs::rename(&tmp, dir.join(format!("{slug}.json")))?;
    Ok(())
}

pub fn remove(slug: &str) {
    let _ = std::fs::remove_file(runtime_dir().join(format!("{slug}.json")));
}

fn pid_alive(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

pub fn read_all() -> Result<Vec<Snapshot>> {
    let dir = runtime_dir();
    let mut snaps = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(snaps),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(snap) = serde_json::from_slice::<Snapshot>(&bytes) else {
            continue;
        };
        if pid_alive(snap.pid) {
            snaps.push(snap);
        }
    }
    snaps.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(snaps)
}

fn meter(peak: f32) -> String {
    const WIDTH: usize = 24;
    let db = if peak > 0.0 {
        20.0 * peak.log10()
    } else {
        f32::NEG_INFINITY
    };
    // -60 dB .. 0 dB mapped onto the bar.
    let filled = (((db + 60.0) / 60.0).clamp(0.0, 1.0) * WIDTH as f32).round() as usize;
    let label = if db.is_finite() {
        format!("{db:6.1} dB")
    } else {
        "  -inf dB".into()
    };
    format!(
        "[{}{}] {label}",
        "#".repeat(filled),
        " ".repeat(WIDTH - filled)
    )
}

pub fn print(snap: &Snapshot) {
    let ms = |frames: f64| frames * 1000.0 / snap.rate as f64;
    let state = |on: bool| if on { "streaming" } else { "idle" };
    println!("== {} (pid {}) ==", snap.name, snap.pid);
    println!(
        "  Internal SR: {} Hz   Channels: {}   Latency: {} smp ({:.1} ms)   Max: {} smp",
        snap.rate,
        snap.channels,
        snap.latency,
        ms(snap.latency as f64),
        snap.max_latency
    );
    println!(
        "  Fill: {} smp ({:.1} ms)   Input: {}   Output: {}",
        snap.fill,
        ms(snap.fill as f64),
        state(snap.input_connected),
        state(snap.output_connected)
    );
    println!(
        "  Buffers in/out: {} / {} smp   Calls in/out: {} / {}",
        snap.last_quantum_in, snap.last_quantum_out, snap.push_calls, snap.pull_calls
    );
    println!(
        "  Buffer sizes: <256: {}  <512: {}  <1024: {}  >=1024: {}",
        snap.quantum_hist[0], snap.quantum_hist[1], snap.quantum_hist[2], snap.quantum_hist[3]
    );
    println!(
        "  Push loss: {} smp ({} overflows)   Pull loss: {} smp ({} underruns)   Resyncs: {}",
        snap.push_loss, snap.overflows, snap.pull_loss, snap.underruns, snap.resyncs
    );
    if snap.latency_warnings > 0 {
        println!(
            "  Warning: {} buffers larger than latency/3; raise `latency` to avoid dropouts",
            snap.latency_warnings
        );
    }
    println!("  Input levels:");
    for (i, p) in snap.peak_in.iter().enumerate() {
        println!("    {:>2} {}", i + 1, meter(*p));
    }
    println!("  Output levels:");
    for (i, p) in snap.peak_out.iter().enumerate() {
        println!("    {:>2} {}", i + 1, meter(*p));
    }
}
