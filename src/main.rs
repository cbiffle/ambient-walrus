// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod config;

use std::{path::PathBuf, time::Duration, pin::pin, future::Future};

use anyhow::{Context, bail};
use clap::Parser;
use config::{BacklightConfig, CommonSensorConfig, SensorConfig, SensorBackendConfig};
use futures::Stream;
use log::{debug, trace, error};
use logind_zbus::session::SessionProxy;
use tokio::{sync::watch, task::JoinSet, select, time::Instant};
use tokio_stream::StreamExt;
use zbus::{Connection, proxy, interface};

use crate::config::{Config, ControlBackendConfig, CommonControlConfig, MaxBehavior};

/// The Ambient Walrus lurks in the background, adjusting the lighting to suit
/// the mood.
///
/// This is a simple program for controlling the brightness of display
/// backlights and supplementary lighting based on the output of ambient light
/// sensors.
#[derive(Parser)]
struct AmbientWalrus {
    #[clap(short = 'f', long)]
    config_file: Option<PathBuf>,

    #[clap(subcommand)]
    cmd: Option<SubCmd>,
}

#[derive(Parser)]
enum SubCmd {
    /// Run the walrus (default if no command is given).
    Run,
    /// Generate an example config as a starting point. You will need to edit
    /// the results for your system by e.g. setting the right backlight device.
    Generate,
    /// Send commands to a running instance.
    Ipc {
        #[clap(subcommand)]
        ipc: IpcCmd,
    },
}

#[derive(Parser)]
enum IpcCmd {
    /// Check if we can find and talk to an instance.
    Ping,
    /// Bias brightness adjustments up by the given factor (0-1).
    Up { amount: f64 },
    /// Bias brightness adjustments down by the given factor (0-1).
    Down { amount: f64 },
    /// Reset brightness adjustments to default.
    ResetAdjustment,
    /// Asks the instance to exit.
    Quit,
}

// Use tokio for its convenient composition of state machines, but don't spin up
// craploads of threads.
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = AmbientWalrus::parse();
    env_logger::init();

    if let Some(cmd) = args.cmd {
        match cmd {
            SubCmd::Generate => return do_generate().await,
            SubCmd::Ipc { ipc } => return do_ipc(ipc).await,

            SubCmd::Run => (),
        }
    }

    let config_path = if let Some(path) = args.config_file {
        path
    } else {
        let bd = xdg::BaseDirectories::with_prefix("ambientwalrus")
            .context("can't find HOME and/or config directories")?;
        match bd.find_config_file("config.toml") {
            Some(path) => path,
            None => {
                eprintln!("Error: no configuration file found, and no path specified");
                eprintln!("FYI: searched the following locations without success:");
                let main_path = bd.get_config_home().join("config.toml");
                eprintln!("- {}", main_path.display());
                for d in bd.get_config_dirs() {
                    eprintln!("- {}", d.join("config.toml").display());
                }
                eprintln!("FYI: you can generate an example by running: ambientwalrus generate");
                eprintln!("FYI: e.g. ambientwalrus generate > {}", main_path.display());
                bail!("ensure config is present/readable or override with -f/--config-file");
            }
        }
    };

    let config_text = std::fs::read_to_string(&config_path)
        .with_context(|| format!("can't read config file at path: {}", config_path.display()))?;
    let config: Config = toml::from_str(&config_text)
        .with_context(|| format!("can't parse config file as TOML from path: {}", config_path.display()))?;

    debug!("--- begin configuration ---");
    debug!("{config:#?}");
    debug!("--- end configuration ---");

    let (adjust_send, adjust_recv) = tokio::sync::broadcast::channel(1);
    // The zbus builder APIs use Result for things that, IMO, ought to be
    // panics. In particular the `name` and `serve_at` operations can't fail for
    // any reason other than providing a bogus name/path string.
    let _session = zbus::connection::Builder::session().expect("canned session name can't be parsed!?")
        .name("com.cliffle.AmbientWalrus").expect("constant name can't be parsed!?")
        .serve_at("/com/cliffle/AmbientWalrus", Remote {
            sender: adjust_send,
            adjust: 0.,
        }).expect("constant path can't be parsed!?")
        .build()
        .await
        .context("can't start DBus server, is there already an instance running?\n\
                  To check, run:   ambientwalrus ipc ping
                  To shut it down: ambientwalrus ipc quit")?;

    let (illum_send, _illum_rx) = watch::channel(None);
    let mut join_set = JoinSet::default();

    for (_name, control) in config.controls {
        join_set.spawn(run_control(control.driver, control.common, illum_send.subscribe()));
    }

    join_set.spawn(async move {
        let sensor_stream = run_sensor(config.sensor).await?;
        let mut adjusted_stream = pin!(adjust_stream(sensor_stream, adjust_recv));

        while let Some(sample) = adjusted_stream.next().await {
            trace!("adjusted sample = {sample}");
            if illum_send.send(Some(sample.clamp(0., 1.))).is_err() {
                // All recipients have been closed
                break;
            }
        }
        Ok(())
    });

    while let Some(result) = join_set.join_next().await {
        result.context("task panicked")?
            .context("task failed")?;
    }

    Ok(())
}

async fn do_generate() -> anyhow::Result<()> {
    let config = config::make_example();
    let text = toml::to_string_pretty(&config)
        .expect("could not format canned config as TOML?!");
    println!("# Example config generated by `ambientwalrus generate`");
    println!("{text}");
    Ok(())
}

async fn run_control(
    driver: ControlBackendConfig,
    common: CommonControlConfig,
    illum: watch::Receiver<Option<f64>>,
) -> anyhow::Result<()> {
    match driver {
        ControlBackendConfig::Backlight(c) => linux_backlight(
            common,
            c,
            illum,
        ).await,
        ControlBackendConfig::ThinkpadKeyboardBacklight => todo!(),
    }
}

async fn backlight_seeker<F>(
    common: &CommonControlConfig,
    mut current: f64,
    mut target_in: watch::Receiver<Option<f64>>,
    mut apply: impl FnMut(f64) -> F,
)
    where F: Future<Output = ()>,
{
    let mut interval = tokio::time::interval(Duration::from_secs_f64(1. / common.update_rate.unwrap_or(CommonControlConfig::DEFAULT_UPDATE_RATE)));

    struct Seek {
        target: f64,
        begin_time: Instant,
        begin_value: f64,
    }
    let mut seek: Option<Seek> = None;
    // How close to the target we have to get before stopping.
    let hyst = 0.05;
    // What fraction of the full range per second we'll move.
    let slope: f64 = common.adjust_slope.unwrap_or(CommonControlConfig::DEFAULT_ADJUST_SLOPE);
    loop {
        select! {
            _ = interval.tick() => {
                if let Some(in_progress) = &seek {
                    // How far off are we?
                    let error: f64 = current - in_progress.target;
                    if error.abs() < hyst {
                        // Close enough. Park it here.
                        seek = None;
                        // Stop getting ticks.
                        interval.reset_after(Duration::from_secs(60));
                        debug!("backlight sleeping (|{error}| < {hyst})");
                    } else {
                        let t = in_progress.begin_time.elapsed().as_secs_f64();
                        // Move toward the target at a constant rate.
                        if error > 0. {
                            current = (in_progress.begin_value - t * slope).max(in_progress.target);
                        } else {
                            current = (in_progress.begin_value + t * slope).min(in_progress.target);
                        }
                        debug!("backlight = {current}");
                        apply(current).await;
                    }
                } else {
                    debug!("resetting timer into future again");
                    interval.reset_after(Duration::from_secs(60));
                }
            }
            _ = target_in.changed() => {
                if let Some(v) = *target_in.borrow_and_update() {
                    // If we're not currently seeking, this may be our first
                    // wake in a while. We'd like to keep it that way! So we'll
                    // duplicate the hysteresis checking logic to avoid relying
                    // on a timer tick.
                    if seek.is_none() {
                        let error = current - v;
                        if error.abs() < hyst {
                            // ignore this.
                            debug!("new target {v} uninteresting: e={error}");
                            continue;
                        } else {
                            // We're about to _begin_ seeking, which means we
                            // need to restart the interval timer.
                            interval.reset_immediately();
                        }
                    }

                    debug!("new backlight target = {v}");
                    seek = Some(Seek {
                        target: v,
                        begin_time: Instant::now(),
                        begin_value: current,
                    });
                } else {
                    // Null reading from the sensor, disable the timer, but only
                    // if we're not already seeking.
                    if seek.is_none() {
                        interval.reset_after(Duration::from_secs(60));
                    }
                }
            }
        }
    }
}

async fn linux_backlight(
    common: CommonControlConfig,
    cfg: BacklightConfig,
    illum: watch::Receiver<Option<f64>>,
) -> anyhow::Result<()> {
    let (bl, raw_start) = {
        let (mut bl, current) = brightr::use_specific_backlight(&cfg.device)?;
        // Override driver-reported max setting if requested by the user.
        if let Some(m) = cfg.raw_max {
            bl.max = m;
        }
        (bl, current)
    };

    let conn = Connection::system().await?;
    let session = SessionProxy::builder(&conn)
        .path("/org/freedesktop/login1/session/auto")?
        .build().await?;

    let max = f64::from(bl.max);
    let exponent = common.exponent.unwrap_or(CommonControlConfig::DEFAULT_EXPONENT);

    let output = common.output.unwrap_or_default();
    let input = common.input.unwrap_or_default();
    let start = (f64::from(raw_start) / max).clamp(0., 1.).powf(1. / exponent);
    let start_mapped = start - output.lo / (output.hi - output.lo);

    // Commit point, we'll let the daemon stay up from here on.
    backlight_seeker(
        &common,
        start_mapped,
        illum,
        |x| {
            let session = &session;
            let bl = &bl;
            async move {
                let sample = {
                    let mut sample = x;
                    // Map input range to 0..1, applying our saturation behavior
                    if sample > input.hi {
                        match common.max_behavior.unwrap_or_default() {
                            MaxBehavior::Off => {
                                if let Err(e) = brightr::async_set_brightness(session, bl, 0).await {
                                    error!("failed to turn off backlight: {e:?}");
                                }
                                return;
                            }
                            MaxBehavior::Saturate => {
                                sample = input.hi;
                            }
                        }
                    }

                    sample = (sample - input.lo) / (input.hi - input.lo);
                    sample.clamp(0., 1.)
                };

                // Apply our output mapping.
                let output = sample * (output.hi - output.lo) + output.lo;
                trace!("backlight post-mapping = {output}");

                let raw_output = (output.powf(exponent) * max).round() as u32;
                trace!("backlight raw = {raw_output}");
                // The max here is redundant but I'm sketchy about the floating point
                // math
                if let Err(e) = brightr::async_set_brightness(session, bl, u32::min(raw_output, bl.max)).await {
                    error!("failed to set backlight: {e:?}");
                }
            }
        },
    ).await;
    Ok(())
}

async fn run_sensor(
    config: SensorConfig,
) -> anyhow::Result<impl Stream<Item = f64>> {
    match config.driver {
        SensorBackendConfig::IioSensorProxy => iio_sensor_proxy(config.common).await,
    }
}


#[proxy(
    interface = "net.hadess.SensorProxy",
    default_service = "net.hadess.SensorProxy",
    default_path = "/net/hadess/SensorProxy",
)]
trait IioSensors {
    #[zbus(property(emits_changed_signal = "false"))]
    fn has_ambient_light(&self) -> zbus::fdo::Result<bool>;
    #[zbus(property(emits_changed_signal = "false"))]
    fn light_level_unit(&self) -> zbus::fdo::Result<String>;
    #[zbus(property)]
    fn light_level(&self) -> zbus::fdo::Result<f64>;

    fn claim_light(&self) -> zbus::fdo::Result<()>;
    fn release_light(&self) -> zbus::fdo::Result<()>;
}

async fn iio_sensor_proxy(
    common: CommonSensorConfig,
) -> anyhow::Result<impl Stream<Item = f64>> {
    let conn = Connection::system().await?;
    let p = IioSensorsProxy::new(&conn).await?;

    if !p.has_ambient_light().await? {
        bail!("no ambient light sensor supported");
    }

    let input = common.input.unwrap_or_default();
    let unit = p.light_level_unit().await?;
    let max = input.hi.map_or_else(|| {
        match unit.as_str() {
            "vendor" => {
                // The proxy docs indicate that this means a scale from 0-255.
                Ok(255.0)
            }
            "lux" => {
                // The choice of default max here is much less obvious, and
                // kinda depends on how bright your backlight is.
                bail!("sensor with unit 'lux' requires sensor.input.hi to be specified");
            }
            _ => {
                bail!("unrecognized light sensor unit: {unit}");
            }
        }
    }, Ok)?;
    let min = input.lo.unwrap_or(0.);

    if max < 0. {
        bail!("sensor max must be greater than zero");
    }

    p.claim_light().await?;

    let exponent = 1. / common.exponent.unwrap_or(CommonSensorConfig::DEFAULT_EXPONENT);

    let poll_interval = Duration::from_secs_f64(
        1. / common.poll_hz.unwrap_or(CommonSensorConfig::DEFAULT_POLL_HZ)
    );

    let mut last = None;

    Ok(async_stream::stream! {
        let mut change_stream = p.receive_light_level_changed().await;
        loop {
            let level = select! {
                change = change_stream.next() => {
                    match change {
                        None => {
                            // Huh. End of stream.
                            error!("reached end of light level change stream");
                            continue;
                        }
                        Some(vchange) => match vchange.get().await {
                            Ok(value) => {
                                trace!("light level change signal: {value}");
                                value
                            },
                            Err(e) => {
                                error!("can't get changed property: {e:?}");
                                continue;
                            }
                        },
                    }
                }
                _ = tokio::time::sleep(poll_interval) => {
                    match p.light_level().await {
                        Ok(x) => x,
                        Err(e) => {
                            error!("error reading light level: {e:?}");
                            continue;
                        }
                    }
                }
            };

            if last != Some(level) {
                trace!("raw sensor reading = {level} {unit}");
                last = Some(level);

                let f = ((level - min) / (max - min)).clamp(0., 1.).powf(exponent);
                trace!("scaled sensor reading = {f}");
                yield f;
            }
        }
    })
}

struct Remote {
    sender: tokio::sync::broadcast::Sender<f64>,
    adjust: f64,
}

#[interface(name = "com.cliffle.AmbientWalrus1")]
impl Remote {
    async fn adjust_by(&mut self, amt: f64) {
        self.adjust += amt;
        trace!("adjustment = {}", self.adjust);
        self.sender.send(self.adjust).ok();
    }

    async fn set_adjustment(&mut self, amt: f64) {
        self.adjust = amt;
        trace!("adjustment = {}", self.adjust);
        self.sender.send(self.adjust).ok();
    }

    async fn quit(&mut self) {
        // TODO should be nicer than this
        std::process::exit(0);
    }
}

fn adjust_stream(
    stream: impl Stream<Item = f64>,
    mut adjust: tokio::sync::broadcast::Receiver<f64>,
) -> impl Stream<Item = f64> {
    async_stream::stream! {
        let mut stream = pin!(stream);
        let mut adjustment: f64 = 0.;
        let mut last_value: Option<f64> = None;
        loop {
            select! {
                v = stream.next() => {
                    if let Some(v) = v {
                        last_value = Some(v);
                        yield v + adjustment;
                    } else {
                        // Stream shutting down.
                        break;
                    }
                }
                a = adjust.recv() => {
                    match a {
                        Ok(new_adjust) => {
                            adjustment = new_adjust;
                            if let Some(last) = last_value {
                                trace!("last = {last}");
                                yield last + adjustment;
                            }
                        }
                        Err(_) => {
                            // Adjustment shutting down, we will too.
                            break;
                        }
                    }
                }
            }
        }
    }
}

async fn do_ipc(ipc: IpcCmd) -> anyhow::Result<()> {
    let conn = Connection::session().await
        .expect("could not parse canned session bus name?!");
    let proxy = RemoteProxy::new(&conn).await
        .context("can't connect to instance (is one running?)")?;

    match do_ipc_core(proxy, ipc).await {
        Ok(()) => debug!("IPC succeeded"),

        // Apply some special diagnostics for certain common error cases.
        Err(e) => {
            #[allow(clippy::single_match)] // not planning on keeping it single
            match &e {
                zbus::Error::MethodError(name, _text_, _msg) => {
                    match name.as_str() {
                        "org.freedesktop.DBus.Error.ServiceUnknown" => {
                            return Err(e).context("can't contact instance (is it running?)");
                        }
                        _ => (),
                    }
                }
                _ => (),
            }

            return Err(e).context("IPC operation failed");
        }
    }
    Ok(())
}

async fn do_ipc_core(proxy: RemoteProxy<'_>, ipc: IpcCmd) -> zbus::Result<()> {
    match ipc {
        IpcCmd::Ping => {
            proxy.adjust_by(0.).await?;
            println!("success!");
            Ok(())
        }
        IpcCmd::Up { amount } => {
            proxy.adjust_by(amount).await
        }
        IpcCmd::Down { amount } => {
            proxy.adjust_by(-amount).await
        }
        IpcCmd::ResetAdjustment => {
            proxy.set_adjustment(0.).await
        }
        IpcCmd::Quit => {
            proxy.quit().await
        }
    }
}

#[proxy(
    default_service = "com.cliffle.AmbientWalrus",
    default_path = "/com/cliffle/AmbientWalrus",
    interface = "com.cliffle.AmbientWalrus1",
)]
trait Remote {
    async fn adjust_by(&self, amt: f64) -> zbus::Result<()>;
    async fn set_adjustment(&self, amt: f64) -> zbus::Result<()>;
    async fn quit(&self) -> zbus::Result<()>;
}
