use futures::Stream;

use crate::config::{SensorConfig, SensorBackendConfig};

pub mod iio_sensors_proxy;

pub async fn run(
    config: SensorConfig,
) -> anyhow::Result<impl Stream<Item = f64>> {
    match config.driver {
        SensorBackendConfig::IioSensorsProxy => {
            self::iio_sensors_proxy::run(config.common).await
        }
    }
}
