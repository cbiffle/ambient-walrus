// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod config;
mod ipc_client;
mod ipc_server;
mod sensor;
mod control;

use std::pin::pin;
use std::{path::PathBuf, time::Duration};

use anyhow::{Context, bail};
use clap::Parser;
use futures::Stream;
use log::{debug, trace};
use tokio::{sync::watch, task::JoinSet, select};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use zbus::Connection;

use crate::config::Config;
use crate::ipc_client::RemoteProxy;

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
    let (illum_send, _illum_rx) = watch::channel(None);
    let mut join_set = JoinSet::default();
    let cancel = CancellationToken::new();

    for (_name, control) in config.controls {
        join_set.spawn(control::run(control.driver, control.common, illum_send.subscribe(), cancel.child_token()));
    }

    {
        let cancel_child = cancel.child_token();
        join_set.spawn(async move {
            let sensor_stream = sensor::run(config.sensor).await?;
            let mut adjusted_stream = pin!(adjust_stream(sensor_stream, adjust_recv, cancel_child));

            while let Some(sample) = adjusted_stream.next().await {
                trace!("adjusted sample = {sample}");
                if illum_send.send(Some(sample.clamp(0., 1.))).is_err() {
                    // All recipients have been closed
                    break;
                }
            }
            Ok(())
        });
    }

    // The zbus builder APIs use Result for things that, IMO, ought to be
    // panics. In particular the `name` and `serve_at` operations can't fail for
    // any reason other than providing a bogus name/path string.
    let session = zbus::connection::Builder::session().expect("canned session name can't be parsed!?")
        .name("com.cliffle.AmbientWalrus").expect("constant name can't be parsed!?")
        .serve_at("/com/cliffle/AmbientWalrus", ipc_server::Remote::new(
            adjust_send,
            cancel,
        )).expect("constant path can't be parsed!?")
        .build()
        .await
        .context("can't start DBus server, is there already an instance running?\n\
                  To check, run:   ambientwalrus ipc ping\n\
                  To shut it down: ambientwalrus ipc quit")?;

    while let Some(result) = join_set.join_next().await {
        result.context("task panicked")?
            .context("task failed")?;
    }

    // TODO: I can't figure out a good way to let the quit IPC finish responding
    // before shutting everything down; session.close() does not do it.
    tokio::time::sleep(Duration::from_millis(500)).await;

    session.close().await
        .context("error while closing DBus session")?;

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

fn adjust_stream(
    stream: impl Stream<Item = f64>,
    mut adjust: tokio::sync::broadcast::Receiver<f64>,
    cancel: CancellationToken,
) -> impl Stream<Item = f64> {
    async_stream::stream! {
        let mut stream = pin!(stream);
        let mut adjustment: f64 = 0.;
        let mut last_value: Option<f64> = None;
        loop {
            select! {
                _ = cancel.cancelled() => {
                    break;
                }
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
