pub mod linux_backlight;

use std::time::Duration;

use futures::Future;
use log::{debug, trace};
use tokio::{sync::watch, time::Instant, select};
use tokio_util::sync::CancellationToken;

use crate::config::{CommonControlConfig, ControlBackendConfig};

pub async fn run(
    driver: ControlBackendConfig,
    common: CommonControlConfig,
    illum: watch::Receiver<Option<f64>>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    match driver {
        ControlBackendConfig::Backlight(c) => self::linux_backlight::run(
            common,
            c,
            illum,
            cancel,
        ).await,
        ControlBackendConfig::ThinkpadKeyboardBacklight => todo!(),
    }
}



async fn backlight_seeker<F>(
    common: &CommonControlConfig,
    mut current: f64,
    mut target_in: watch::Receiver<Option<f64>>,
    cancel: CancellationToken,
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
    let hyst = 0.01;
    // What fraction of the full range per second we'll move.
    let slope: f64 = common.adjust_slope.unwrap_or(CommonControlConfig::DEFAULT_ADJUST_SLOPE);
    loop {
        select! {
            _ = cancel.cancelled() => {
                break;
            }
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
                        trace!("backlight = {current}");
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


