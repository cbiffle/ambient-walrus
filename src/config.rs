use std::collections::BTreeMap;
use serde::Deserialize;

const fn one() -> f64 { 1. }

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub sensor: SensorConfig,
    pub controls: BTreeMap<String, ControlConfig>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
pub struct SensorConfig {
    pub driver: SensorBackendConfig,

    #[serde(flatten)]
    pub common: CommonSensorConfig,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub enum SensorBackendConfig {
    IioSensorProxy,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
pub struct CommonSensorConfig {
    #[serde(default)]
    pub input: OptRangeConfig,
    #[serde(default)]
    pub poll_hz: Option<f64>,
    #[serde(default)]
    pub exponent: Option<f64>,
}

impl CommonSensorConfig {
    pub const DEFAULT_POLL_HZ: f64 = 0.2;
    pub const DEFAULT_EXPONENT: f64 = 3.;
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
pub struct ControlConfig {
    pub driver: ControlBackendConfig,

    #[serde(flatten)]
    pub common: CommonControlConfig,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
pub struct CommonControlConfig {
    #[serde(default)]
    pub input: RangeConfig,
    #[serde(default)]
    pub output: RangeConfig,
    #[serde(default)]
    pub max_behavior: MaxBehavior,
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

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MaxBehavior {
    Off,
    #[default]
    Saturate,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub enum ControlBackendConfig {
    Backlight(BacklightConfig),
    ThinkpadKeyboardBacklight,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct BacklightConfig {
    pub device: String,
    pub raw_max: Option<u32>,
}


#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct RangeConfig {
    #[serde(default)]
    pub lo: f64,
    #[serde(default = "one")]
    pub hi: f64,
}

#[derive(Deserialize, Debug, Default)]
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


