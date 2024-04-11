// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{path::{PathBuf, Path}, collections::BTreeMap, time::Duration, pin::pin};

use anyhow::{Context, bail};
use clap::Parser;
use futures::Stream;
use logind_zbus::session::SessionProxyBlocking;
use serde::Deserialize;
use tokio::{sync::watch, task::JoinSet};
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
    #[serde(default = "one")]
    exponent: f64,
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
    #[serde(default = "one")]
    exponent: f64,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = AmbientWalrus::parse();

    let config_text = std::fs::read_to_string(&args.config_file)?;
    let config: Config = toml::from_str(&config_text)?;

    println!("{config:#?}");

    let (illum_send, _illum_rx) = watch::channel(0.);
    let mut join_set = JoinSet::default();

    for (_name, control) in config.controls {
        join_set.spawn(run_control(control.driver, control.common, illum_send.subscribe()));
    }

    join_set.spawn(async move {
        let mut sensor_stream = pin!(iio_sensor_proxy(
            config.sensor.common,
        ).await?);

        while let Some(sample) = sensor_stream.next().await {
            println!("sample = {sample}");
            if illum_send.send(sample).is_err() {
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
    illum: watch::Receiver<f64>,
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

#[no_mangle] // stfu rustanalyzer
async fn linux_backlight(
    common: CommonControlConfig,
    cfg: BacklightConfig,
    mut illum: watch::Receiver<f64>,
) -> anyhow::Result<()> {
    let (bl, _) = brightr::use_specific_backlight(&cfg.device)?;

    let conn = Connection::system()?;
    let session = SessionProxyBlocking::builder(&conn)
        .path("/org/freedesktop/login1/session/auto")?
        .build()?;

    let max = f64::from(bl.max);

    // Commit point, we'll let the daemon stay up from here on.
    loop {
        let Ok(()) = illum.changed().await else {
            return Ok(());
        };
        let sample = {
            let mut sample = *illum.borrow_and_update();

            // Map input range to 0..1, applying our saturation behavior
            if sample > common.input.hi {
                match common.max_behavior {
                    MaxBehavior::Off => {
                        if let Err(e) = brightr::set_brightness(&session, &bl, 0) {
                            eprintln!("failed to turn off backlight: {e:?}");
                        }
                        continue;
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
        println!("backlight post-mapping = {output}");

        let raw_output = (output.powf(common.exponent) * max).round() as u32;
        println!("backlight raw = {raw_output}");
        // The max here is redundant but I'm sketchy about the floating point
        // math
        if let Err(e) = brightr::set_brightness(&session, &bl, u32::min(raw_output, bl.max)) {
            eprintln!("failed to set backlight: {e:?}");
        }
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

    Ok(async_stream::stream! {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let level = match p.light_level().await {
                Ok(x) => x,
                Err(e) => {
                    eprintln!("error reading light level: {e:?}");
                    continue;
                }
            };
            println!("raw sensor reading = {level} {unit}");

            yield ((level - min) / (max - min)).clamp(0., 1.);
        }
    })
}
