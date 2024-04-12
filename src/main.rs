// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{path::PathBuf, collections::BTreeMap, time::Duration, pin::pin};

use anyhow::{Context, bail};
use clap::Parser;
use futures::Stream;
use log::{debug, trace, error};
use logind_zbus::session::SessionProxyBlocking;
use serde::Deserialize;
use tokio::{sync::watch, task::JoinSet, select, time::Instant};
use tokio_stream::StreamExt;
use zbus::{blocking::Connection, proxy};

const fn one() -> f64 { 1. }

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct Config {
    sensor: SensorConfig,
    controls: BTreeMap<String, ControlConfig>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
struct SensorConfig {
    driver: SensorBackendConfig,

    #[serde(flatten)]
    common: CommonSensorConfig,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
enum SensorBackendConfig {
    IioSensorProxy,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
struct CommonSensorConfig {
    #[serde(default)]
    input: OptRangeConfig,
    #[serde(default)]
    poll_hz: Option<f64>,
    #[serde(default)]
    exponent: Option<f64>,
}

impl CommonSensorConfig {
    const DEFAULT_POLL_HZ: f64 = 1.;
    const DEFAULT_EXPONENT: f64 = 3.;
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
struct ControlConfig {
    driver: ControlBackendConfig,

    #[serde(flatten)]
    common: CommonControlConfig,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
struct CommonControlConfig {
    #[serde(default)]
    input: RangeConfig,
    #[serde(default)]
    output: RangeConfig,
    #[serde(default)]
    max_behavior: MaxBehavior,
    #[serde(default)]
    exponent: Option<f64>,
    #[serde(default)]
    adjust_slope: Option<f64>,
    #[serde(default)]
    update_rate: Option<f64>,
}

impl CommonControlConfig {
    const DEFAULT_ADJUST_SLOPE: f64 = 0.5;
    const DEFAULT_UPDATE_RATE: f64 = 60.;
    const DEFAULT_EXPONENT: f64 = 3.;
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "kebab-case")]
enum MaxBehavior {
    Off,
    #[default]
    Saturate,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
enum ControlBackendConfig {
    Backlight(BacklightConfig),
    ThinkpadKeyboardBacklight,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
struct BacklightConfig {
    device: String,
    raw_max: Option<u32>,
}


#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct RangeConfig {
    #[serde(default)]
    lo: f64,
    #[serde(default = "one")]
    hi: f64,
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct OptRangeConfig {
    #[serde(default)]
    lo: Option<f64>,
    #[serde(default)]
    hi: Option<f64>,
}

impl Default for RangeConfig {
    fn default() -> Self {
        Self { lo: 0., hi: 1. }
    }
}

#[derive(Parser)]
struct AmbientWalrus {
    #[clap(short = 'f', long)]
    config_file: PathBuf,
}

// Use tokio for its convenient composition of state machines, but don't spin up
// craploads of threads.
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = AmbientWalrus::parse();

    env_logger::init();

    let config_text = std::fs::read_to_string(&args.config_file)?;
    let config: Config = toml::from_str(&config_text)?;

    debug!("{config:#?}");

    let (illum_send, _illum_rx) = watch::channel(None);
    let mut join_set = JoinSet::default();

    for (_name, control) in config.controls {
        join_set.spawn(run_control(control.driver, control.common, illum_send.subscribe()));
    }

    join_set.spawn(async move {
        let mut sensor_stream = pin!(run_sensor(config.sensor).await?);

        while let Some(sample) = sensor_stream.next().await {
            trace!("sensor sample = {sample}");
            if illum_send.send(Some(sample)).is_err() {
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

async fn backlight_seeker(
    common: &CommonControlConfig,
    mut current: f64,
    mut target_in: watch::Receiver<Option<f64>>,
    mut apply: impl FnMut(f64),
) {
    let mut interval = tokio::time::interval(Duration::from_secs_f64(1. / common.update_rate.unwrap_or(CommonControlConfig::DEFAULT_UPDATE_RATE)));

    struct Seek {
        target: f64,
        begin_time: Instant,
        begin_value: f64,
    }
    let mut seek: Option<Seek> = None;
    // How close to the target we have to get before stopping.
    let hyst = 0.1;
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
                        apply(current);
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

    let conn = Connection::system()?;
    let session = SessionProxyBlocking::builder(&conn)
        .path("/org/freedesktop/login1/session/auto")?
        .build()?;

    let max = f64::from(bl.max);
    let exponent = common.exponent.unwrap_or(CommonControlConfig::DEFAULT_EXPONENT);

    let start = (f64::from(raw_start) / max).clamp(0., 1.).powf(1. / exponent);
    let start_mapped = start - common.output.lo / (common.output.hi - common.output.lo);

    // Commit point, we'll let the daemon stay up from here on.
    backlight_seeker(
        &common,
        start_mapped,
        illum,
        |x| {
            let sample = {
                let mut sample = x;
                // Map input range to 0..1, applying our saturation behavior
                if sample > common.input.hi {
                    match common.max_behavior {
                        MaxBehavior::Off => {
                            if let Err(e) = brightr::set_brightness(&session, &bl, 0) {
                                error!("failed to turn off backlight: {e:?}");
                            }
                            return;
                        }
                        MaxBehavior::Saturate => {
                            sample = common.input.hi;
                        }
                    }
                }

                sample = (sample - common.input.lo) / (common.input.hi - common.input.lo);
                sample.clamp(0., 1.)
            };

            // Apply our output mapping.
            let output = sample * (common.output.hi - common.output.lo) + common.output.lo;
            trace!("backlight post-mapping = {output}");

            let raw_output = (output.powf(exponent) * max).round() as u32;
            trace!("backlight raw = {raw_output}");
            // The max here is redundant but I'm sketchy about the floating point
            // math
            if let Err(e) = brightr::set_brightness(&session, &bl, u32::min(raw_output, bl.max)) {
                error!("failed to set backlight: {e:?}");
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
    #[zbus(property)]
    fn has_ambient_light(&self) -> zbus::fdo::Result<bool>;
    #[zbus(property)]
    fn light_level_unit(&self) -> zbus::fdo::Result<String>;
    #[zbus(property(emits_changed_signal = "true"))]
    fn light_level(&self) -> zbus::fdo::Result<f64>;

    fn claim_light(&self) -> zbus::fdo::Result<()>;
    fn release_light(&self) -> zbus::fdo::Result<()>;
}

async fn iio_sensor_proxy(
    common: CommonSensorConfig,
) -> anyhow::Result<impl Stream<Item = f64>> {
    let conn = Connection::system()?;
    let p = IioSensorsProxy::new(&conn.into()).await?;

    if !p.has_ambient_light().await? {
        bail!("no ambient light sensor supported");
    }

    let unit = p.light_level_unit().await?;
    let max = common.input.hi.map_or_else(|| {
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
    let min = common.input.lo.unwrap_or(0.);

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
        loop {
            tokio::time::sleep(poll_interval).await;
            let level = match p.light_level().await {
                Ok(x) => x,
                Err(e) => {
                    error!("error reading light level: {e:?}");
                    continue;
                }
            };

            if last != Some(level) {
                trace!("raw sensor reading = {level} {unit}");
                last = Some(level);

                yield ((level - min) / (max - min)).clamp(0., 1.).powf(exponent);
            }
        }
    })
}
