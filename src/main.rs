use std::{path::{PathBuf, Path}, collections::BTreeMap};

use anyhow::Context;
use clap::Parser;
use logind_zbus::session::SessionProxyBlocking;
use serde::Deserialize;
use tokio::sync::watch;
use zbus::blocking::Connection;

const fn one() -> f64 { 1. }

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct Config {
    controls: BTreeMap<String, ControlConfig>,
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

        let raw_output = (output.powf(common.exponent) * max).round() as u32;

        // The max here is redundant but I'm sketchy about the floating point
        // math
        if let Err(e) = brightr::set_brightness(&session, &bl, u32::min(raw_output, bl.max)) {
            eprintln!("failed to set backlight: {e:?}");
        }
    }
}
