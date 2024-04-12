use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};

const fn one() -> f64 { 1. }

pub fn make_example() -> Config {
    Config {
        sensor: SensorConfig {
            driver: SensorBackendConfig::IioSensorProxy,
            common: CommonSensorConfig {
                input: None,
                poll_hz: None,
                exponent: None,
            },
        },
        controls: [
            ("backlight".to_string(), ControlConfig {
                driver: ControlBackendConfig::Backlight(BacklightConfig {
                    device: "my_backlight_device".to_string(),
                    raw_max: None,
                }),
                common: CommonControlConfig::default(),
            }),
        ].into_iter().collect(),
    }
}

#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub sensor: SensorConfig,
    pub controls: BTreeMap<String, ControlConfig>,
}

#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
pub struct SensorConfig {
    pub driver: SensorBackendConfig,

    #[serde(flatten)]
    pub common: CommonSensorConfig,
}

#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub enum SensorBackendConfig {
    IioSensorProxy,
}

#[derive(Copy, Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
pub struct CommonSensorConfig {
    #[serde(default)]
    pub input: Option<OptRangeConfig>,
    #[serde(default)]
    pub poll_hz: Option<f64>,
    #[serde(default)]
    pub exponent: Option<f64>,
}

impl CommonSensorConfig {
    pub const DEFAULT_POLL_HZ: f64 = 0.2;
    pub const DEFAULT_EXPONENT: f64 = 3.;
}

#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
pub struct ControlConfig {
    pub driver: ControlBackendConfig,

    #[serde(flatten)]
    pub common: CommonControlConfig,
}

#[derive(Copy, Clone, Deserialize, Serialize, Debug, Default)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
pub struct CommonControlConfig {
    #[serde(default)]
    pub input: Option<RangeConfig>,
    #[serde(default)]
    pub output: Option<RangeConfig>,
    #[serde(default)]
    pub max_behavior: Option<MaxBehavior>,
    #[serde(default)]
    pub exponent: Option<f64>,
    #[serde(default)]
    pub adjust_slope: Option<f64>,
    #[serde(default)]
    pub update_rate: Option<f64>,
}

impl CommonControlConfig {
    pub const DEFAULT_ADJUST_SLOPE: f64 = 0.5;
    pub const DEFAULT_UPDATE_RATE: f64 = 60.;
    pub const DEFAULT_EXPONENT: f64 = 3.;
}

#[derive(Copy, Clone, Deserialize, Serialize, Debug, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MaxBehavior {
    Off,
    #[default]
    Saturate,
}

#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub enum ControlBackendConfig {
    Backlight(BacklightConfig),
    ThinkpadKeyboardBacklight,
}

#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct BacklightConfig {
    pub device: String,
    pub raw_max: Option<u32>,
}


#[derive(Copy, Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct RangeConfig {
    #[serde(default)]
    pub lo: f64,
    #[serde(default = "one")]
    pub hi: f64,
}

#[derive(Copy, Clone, Deserialize, Serialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct OptRangeConfig {
    #[serde(default)]
    pub lo: Option<f64>,
    #[serde(default)]
    pub hi: Option<f64>,
}

impl Default for RangeConfig {
    fn default() -> Self {
        Self { lo: 0., hi: 1. }
    }
}


