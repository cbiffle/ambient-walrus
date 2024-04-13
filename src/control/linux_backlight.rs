use std::fs;

use log::{error, trace, warn};
use logind_zbus::session::SessionProxy;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use zbus::Connection;

use crate::config::{CommonControlConfig, BacklightConfig, MaxBehavior, ControlConfig, ControlBackendConfig};

pub async fn run(
    common: CommonControlConfig,
    cfg: BacklightConfig,
    illum: watch::Receiver<Option<f64>>,
    cancel: CancellationToken,
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
    super::backlight_seeker(
        &common,
        start_mapped,
        illum,
        cancel,
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

pub async fn try_generate() -> Vec<(String, ControlConfig)> {
    let mut controls = vec![];

    let Ok(dir) = fs::read_dir("/sys/class/backlight") else {
        warn!("could not read /sys/class/backlight/");
        return controls;
    };
    for dirent in dir {
        let Ok(dirent) = dirent else {
            warn!("error listing directory /sys/class/backlight");
            return controls;
        };

        let path = dirent.path();
        let Some(name) = path.file_name() else {
            warn!("can't get name for path: {}", path.display());
            continue;
        };
        let Some(name) = name.to_str() else {
            warn!("can't handle non-UTF8 device name: {name:?}");
            continue;
        };

        let key = format!("backlight_{name}");
        controls.push((key, ControlConfig {
            driver: ControlBackendConfig::Backlight(BacklightConfig {
                device: name.to_owned(),
                raw_max: None,
            }),
            common: CommonControlConfig::default(),
        }));
    }

    controls
}
