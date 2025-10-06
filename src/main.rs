use anyhow::{Result, anyhow};

use clap::Parser;
use epicars::{
    ServerBuilder,
    providers::{IntercomProvider, intercom::ConverterRecvError},
};
use futures_util::stream::StreamExt;
use jungfrau_ctrl_ioc::{AsyncTelnet, Pinger, TelnetPromptDecoder};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::{select, sync::mpsc};
use tokio_util::codec::FramedRead;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

/// Retrieve the power state e.g. are the front-end boards powered
async fn query_power_state(
    reader: &mut FramedRead<AsyncTelnet, TelnetPromptDecoder>,
) -> Result<bool> {
    reader
        .get_ref()
        .send("cat /var/log/state\n".as_bytes())
        .await?;
    let data = reader
        .next()
        .await
        .ok_or(anyhow!("Got closed connection"))??;
    Ok(str::from_utf8(&data)?.trim().parse::<i16>()? == 1i16)
}

/// Toggle the FEB power to a desired state
async fn switch_power(
    reader: &mut FramedRead<AsyncTelnet, TelnetPromptDecoder>,
    to_value: bool,
) -> Result<()> {
    let command = if to_value {
        "/power_control/on\n"
    } else {
        "/power_control/off\n"
    };
    debug!("Issuing '{}'", command.trim());
    reader.get_ref().send(command.as_bytes()).await?;
    let _ = reader
        .next()
        .await
        .ok_or(anyhow!("Got closed connection"))??;
    Ok(())
}

/// Do the I2C query for the SHT33 values
async fn query_sht33_values(
    reader: &mut FramedRead<AsyncTelnet, TelnetPromptDecoder>,
) -> Result<(f32, f32)> {
    reader
        .get_ref()
        .send(
            "/power_control/i2c_sht33/i2ctransfer -y 0 w2@0x44 0xe0 0x00 r6; echo $?\n".as_bytes(),
        )
        .await?;
    let data = reader
        .next()
        .await
        .ok_or(anyhow!("Got closed connection"))??;
    let response = str::from_utf8(&data)?;
    let lines: Vec<_> = response.lines().collect();
    assert_eq!(lines.len(), 2);
    let status: i32 = lines[1].parse()?;
    if status != 0 {
        return Err(anyhow!("Got nonzero return status: {status}"));
    }
    // Convert the data to ints
    let data: Vec<u8> = lines[0]
        .split_whitespace()
        .map(|v| u8::from_str_radix(&v[2..], 16))
        .collect::<Result<Vec<_>, _>>()?;
    let temperature = 21.875 * (u16::from_be_bytes([data[0], data[1]]) as f32) / 8192.0 - 45.0;
    let humidity = 12.5 * (u16::from_be_bytes([data[3], data[4]]) as f32) / 8192.0;
    Ok((temperature, humidity))
}

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "BL24I-JUNGFRAU:CTRL:")]
    prefix: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let opts = Args::parse();
    let prefix = opts.prefix;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::default()
            .add_directive("warn".parse().unwrap())
            .add_directive(
                format!("{}=debug", env!("CARGO_CRATE_NAME"))
                    .parse()
                    .unwrap(),
            )
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();

    info!("Running with PV prefix {prefix}");

    let mut library = IntercomProvider::new();
    let pv_connected = library
        .build_pv(&format!("{prefix}CONNECTED"), false)
        .read_only(true)
        .build()
        .unwrap();
    let pv_temperature = library
        .build_pv(&format!("{prefix}TEMPERATURE"), 20f32)
        .read_only(true)
        .build()
        .unwrap();
    let pv_humidity = library
        .build_pv(&format!("{prefix}HUMIDITY"), 0f32)
        .read_only(true)
        .build()
        .unwrap();
    let pv_switch = library
        .build_pv(&format!("{prefix}SWITCH"), false)
        .rbv(true)
        .build()
        .unwrap();
    let pv_power = library
        .build_pv(&format!("{prefix}POWER"), false)
        .rbv(true)
        .build()
        .unwrap();
    let pv_state = library
        .build_pv(&format!("{prefix}STATUS"), String::new())
        .minimum_length(32)
        .read_only(true)
        .build()
        .unwrap();
    // Build PV for each submodule up
    let submodules: HashMap<_, _> = (0..18)
        .map(|i| {
            (
                format!("i24-jf9mb-{:02}.diamond.ac.uk", i),
                library
                    .build_pv(&format!("{prefix}BOARD{i:02}:STATE"), false)
                    .read_only(true)
                    .build()
                    .unwrap(),
            )
        })
        .collect();
    let mut switch_watch = pv_switch.subscribe();
    // Don't start the server until we have the first value
    let mut server = None;
    let (ping_tx, mut ping_rx) = mpsc::unbounded_channel();
    let pinger = Pinger::new(ping_tx).ok();
    if pinger.is_none() {
        warn!("Could not open datagram socket for ping: Will not monitor FEB state");
    }
    '_outer: loop {
        pv_state.store("Disconnected".to_string());
        let connection = match AsyncTelnet::connect("i24-jf9mb-ctrl:23").await {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to establish telnet connection: {e}. Waiting before new attempt...");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        pv_connected.store(true);
        pv_state.store("Connected".to_string());
        let mut reader = FramedRead::new(connection, TelnetPromptDecoder {});
        debug!("Discarding initial frame");
        let _ = reader.next().await.unwrap().unwrap();

        // Scrape info

        // The last time we queried an update
        let mut last_requested_update = Instant::now() - Duration::from_secs(60);
        let mut last_ping = last_requested_update;
        // The main loop, most time should be spent in here
        loop {
            pv_state.store("Ready".to_string());
            select! {
                _ = tokio::time::sleep_until((last_requested_update + Duration::from_secs(10)).into()) => {
                    pv_state.store("Reading state".to_string());
                    let Ok((temperature, humidity)) = query_sht33_values(&mut reader).await else {
                        continue;
                    };
                    pv_temperature.store(temperature);
                    pv_humidity.store(humidity);
                    let state = if let Ok(state) = query_power_state(&mut reader).await {
                        if state != pv_power.load() {
                        pv_power.store(state);
                        // Under assumption controlled elsewhere, copy the power state to switch
                            if state != pv_switch.load() {
                                warn!("Power state {state:?} does not match PV. Updating PV to match, as controlled from elsewhere");
                                pv_switch.store(pv_power.load());
                            }
                        }
                        format!("{state:?}")
                    } else {
                        warn!("Could not read power state");
                        "-----".to_string()
                    };

                    info!("SHT33 Temperature: {temperature:-.2}°C   Humidity: {humidity:2.1} %   Power: {state}",);

                    last_requested_update = Instant::now();
                    // Start the server, if we didn't yet, so we never expose pre-values
                    if server.is_none() {
                        server = Some(ServerBuilder::new(library.clone()).start().await.unwrap());
                    }
                },
                _ = tokio::time::sleep_until((last_ping + Duration::from_secs(5)).into()) => {
                    last_ping = Instant::now();
                    if let Some(ref pinger) = pinger {
                        pinger.ping();
                    }
                },
                Some((hostname, success)) = ping_rx.recv() => {
                    let Some(module) = submodules.get(&hostname) else {
                        warn!("Got ping result for unknown module {hostname}");
                        continue;
                    };
                    module.store(success);
                },
                _ = reader.get_ref().disconnected() => break,
                _ = tokio::signal::ctrl_c() => {
                    reader.into_inner().stop().await;
                    break '_outer;
                },
                v = switch_watch.recv() => match v {
                    // We've explicitly had the switch PV toggled
                    Ok(desired_state) => {
                        let current_state = pv_power.load();
                        match (current_state, desired_state) {
                            (true, false) => {
                                debug!("Switch switched to OFF, turning off detector ROB");
                                pv_state.store("Powering down".to_string());
                                let _ = switch_power(&mut reader, desired_state).await;
                            },
                            (false, true) => {
                                debug!("Switch switched to ON, turning on detector ROB");
                                pv_state.store("Powering up".to_string());
                                let _ = switch_power(&mut reader, desired_state).await;
                            },
                            _ => {}, // It already matches
                        };
                        let state = query_power_state(&mut reader).await.unwrap();
                        if state != pv_power.load() {
                            pv_power.store(state);
                            println!("Power on: {state:?}");
                        }
                    },
                    Err(ConverterRecvError::Closed) => {},
                    Err(ConverterRecvError::Lagged(n)) => debug!("Dropped {n} messages listening for changes"),
                    Err(ConverterRecvError::ConversionError) => warn!("Failed to convert subscribe data"),
                }
            }
        }
        // Just a general sleep before trying again
        pv_connected.store(false);
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
