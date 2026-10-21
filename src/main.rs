mod cable;
mod config;
mod ring;
mod stats;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use pipewire as pw;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use cable::Cable;
use config::{CableConfig, Config};

#[derive(Parser)]
#[command(
    version,
    about = "Virtual audio cable for PipeWire: what plays into \"<name> Input\" comes out of \"<name> Output\""
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Create the cables and keep them alive (default)
    Run {
        /// Config file (default: $XDG_CONFIG_HOME/nc-cable/config.toml)
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Ignore the config file and create a single cable with this name
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        rate: Option<u32>,
        #[arg(long)]
        channels: Option<u32>,
        /// Target latency in frames
        #[arg(long)]
        latency: Option<u32>,
        /// Ring buffer size in frames
        #[arg(long)]
        max_latency: Option<u32>,
    },
    /// Show counters and levels of running cables
    Status {
        /// Refresh continuously
        #[arg(short, long)]
        watch: bool,
    },
    /// Write an example config file
    InitConfig {
        #[arg(short, long)]
        config: Option<PathBuf>,
        #[arg(long)]
        force: bool,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command.unwrap_or(Command::Run {
        config: None,
        name: None,
        rate: None,
        channels: None,
        latency: None,
        max_latency: None,
    }) {
        Command::Run {
            config,
            name,
            rate,
            channels,
            latency,
            max_latency,
        } => {
            let path = config.unwrap_or_else(Config::default_path);
            let mut cfg = Config::load(&path)?;
            if name.is_some()
                || rate.is_some()
                || channels.is_some()
                || latency.is_some()
                || max_latency.is_some()
            {
                let base = CableConfig::default();
                let mut cable = CableConfig {
                    name: name.unwrap_or(base.name),
                    rate: rate.unwrap_or(base.rate),
                    channels: channels.unwrap_or(base.channels),
                    latency: latency.unwrap_or(base.latency),
                    max_latency: max_latency.unwrap_or(base.max_latency),
                };
                cable.validate()?;
                cfg.cables = vec![cable];
            }
            run(cfg)
        }
        Command::Status { watch } => status(watch),
        Command::InitConfig { config, force } => {
            let path = config.unwrap_or_else(Config::default_path);
            if path.exists() && !force {
                bail!(
                    "{} already exists (use --force to overwrite)",
                    path.display()
                );
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, config::EXAMPLE)
                .with_context(|| format!("writing {}", path.display()))?;
            println!("wrote {}", path.display());
            Ok(())
        }
    }
}

fn run(cfg: Config) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context
        .connect_rc(None)
        .context("cannot connect to PipeWire; is pipewire running for this user?")?;

    // Exit when the server goes away so systemd can restart us with it.
    let _core_listener = core
        .add_listener_local()
        .error({
            let mainloop = mainloop.clone();
            move |id, _seq, res, message| {
                eprintln!("pipewire error on object {id}: {message} ({res})");
                if id == pw::core::PW_ID_CORE {
                    mainloop.quit();
                }
            }
        })
        .register();

    let mut cables = Vec::new();
    for c in cfg.cables {
        println!(
            "{0} Input -> {0} Output: {1} Hz, {2} ch, latency {3} smp ({4:.1} ms), buffer {5} smp",
            c.name,
            c.rate,
            c.channels,
            c.latency,
            c.latency_ms(),
            c.max_latency
        );
        cables.push(Cable::new(&core, c)?);
    }
    let cables = Rc::new(cables);

    let timer = mainloop.loop_().add_timer({
        let cables = cables.clone();
        move |_| {
            for c in cables.iter() {
                let snap = stats::snapshot(
                    &c.cfg,
                    &c.ring,
                    c.input_connected.load(std::sync::atomic::Ordering::Relaxed),
                    c.output_connected
                        .load(std::sync::atomic::Ordering::Relaxed),
                );
                if let Err(e) = stats::publish(&snap, &c.cfg.slug()) {
                    eprintln!("cannot write stats: {e}");
                }
            }
        }
    });
    let interval = Duration::from_millis(250);
    timer
        .update_timer(Some(interval), Some(interval))
        .into_result()?;

    let quit = {
        let mainloop = mainloop.clone();
        move || mainloop.quit()
    };
    let _sigint = mainloop
        .loop_()
        .add_signal_local(pw::loop_::Signal::INT, quit.clone());
    let _sigterm = mainloop
        .loop_()
        .add_signal_local(pw::loop_::Signal::TERM, quit);

    mainloop.run();

    for c in cables.iter() {
        stats::remove(&c.cfg.slug());
    }
    Ok(())
}

fn status(watch: bool) -> Result<()> {
    loop {
        let snaps = stats::read_all()?;
        if watch {
            print!("\x1b[2J\x1b[H");
        }
        if snaps.is_empty() {
            println!(
                "no running cable (start one with `nc-cable run` or `systemctl --user start nc-cable`)"
            );
        }
        for s in &snaps {
            stats::print(s);
        }
        if !watch {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}
