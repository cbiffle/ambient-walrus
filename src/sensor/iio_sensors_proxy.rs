use std::time::Duration;

use anyhow::bail;
use futures::Stream;
use log::{error, trace, warn};
use tokio::select;
use tokio_stream::StreamExt;
use zbus::{Connection, proxy};

use crate::config::{CommonSensorConfig, SensorConfig, SensorBackendConfig, OptRangeConfig};

pub async fn run(
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

pub async fn try_generate() -> Option<SensorConfig> {
    let conn = Connection::system().await.ok()?;
    let p = IioSensorsProxy::new(&conn).await.ok()?;

    let Ok(flag) = p.has_ambient_light().await else {
        warn!("iio-sensors-proxy not found");
        return None;
    };

    if !flag {
        warn!("iio-sensors-proxy does not have ambient light support");
        return None;
    }

    let mut config = SensorConfig {
        driver: SensorBackendConfig::IioSensorsProxy,
        common: CommonSensorConfig {
            poll_hz: Some(0.1),
            ..CommonSensorConfig::default()
        },
    };

    // Check if we need to provide an explicit input max.
    let Ok(unit) = p.light_level_unit().await else {
        warn!("iio-sensors-proxy not found");
        return None;
    };

    if unit == "lux" {
        config.common.input = Some(OptRangeConfig {
            lo: None,
            // Assume the backlight is equivalent to about 1 klux illumination,
            // somewhat arbitrarily.
            hi: Some(1_000.),
        });
    }

    Some(config)
}
