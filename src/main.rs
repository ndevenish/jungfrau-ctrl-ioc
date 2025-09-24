use anyhow::{Result, anyhow};

use epicars::{ServerBuilder, providers::IntercomProvider};
use futures_util::stream::StreamExt;
use regex::bytes::Regex;
use std::{
    io::{self},
    net::ToSocketAddrs,
    task::Poll,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use telnet::{Action, Event, Telnet, TelnetOption};
use tokio::{
    io::AsyncRead,
    runtime::Handle,
    select,
    sync::{mpsc, oneshot},
};
use tokio_util::{
    codec::{Decoder, FramedRead},
    sync::CancellationToken,
};
use tracing::{debug, trace, warn};

struct AsyncTelnet {
    thread: Option<JoinHandle<()>>,
    cancel: CancellationToken,
    data_rx: mpsc::Receiver<Vec<u8>>,
    req_tx: mpsc::Sender<(oneshot::Sender<io::Result<usize>>, Vec<u8>)>,
    buffer: Vec<u8>,
    /// Triggered on disconnection, so we know it has ended
    disconnected: CancellationToken,
}

impl AsyncTelnet {
    async fn connect(
        addr: impl ToSocketAddrs + Send + 'static + std::fmt::Debug,
    ) -> Result<AsyncTelnet> {
        let (data_tx, data_rx) = mpsc::channel(16);
        let (req_tx, req_rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let cancel_sub = cancel.clone();
        let (conn_tx, conn_rx) = oneshot::channel();
        let disconnected = CancellationToken::new();
        debug!("Spawning new thread to talk to telnet");
        // addr.to_socket_addrs().unwrap()
        let dc = disconnected.clone();
        let thread = thread::spawn(move || {
            AsyncTelnetInternal::start(addr, cancel_sub, data_tx, req_rx, conn_tx, dc)
        });
        // Wait for the connection to succeed or fail
        let _ = conn_rx.await?;
        Ok(AsyncTelnet {
            thread: Some(thread),
            cancel,
            data_rx,
            req_tx,
            buffer: Vec::new(),
            disconnected,
        })
    }

    async fn disconnected(&self) {
        self.disconnected.cancelled().await
    }
    #[allow(dead_code)]
    async fn stop(&mut self) {
        self.cancel.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(|| thread.join()).await;
        }
    }

    async fn send(&self, request: &[u8]) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.req_tx.send((tx, request.to_vec())).await?;
        rx.await??;
        Ok(())
    }
}

impl Drop for AsyncTelnet {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Internal parts of AsyncTelnet, that synchronously manage the connection
struct AsyncTelnetInternal {
    cancel: CancellationToken,
    data_tx: mpsc::Sender<Vec<u8>>,
    req_rx: mpsc::Receiver<(oneshot::Sender<io::Result<usize>>, Vec<u8>)>,
}
impl AsyncTelnetInternal {
    fn start(
        addr: impl ToSocketAddrs + std::fmt::Debug,
        cancel: CancellationToken,
        data_tx: mpsc::Sender<Vec<u8>>,
        req_rx: mpsc::Receiver<(oneshot::Sender<io::Result<usize>>, Vec<u8>)>,
        conn_tx: oneshot::Sender<Result<()>>,
        disconnected: CancellationToken,
    ) {
        let mut internal = Self {
            cancel: cancel.clone(),
            data_tx,
            req_rx,
        };

        let connection = match Self::connect(&addr) {
            Ok(conn) => conn,
            Err(e) => {
                let _ = conn_tx.send(Err(e));
                return;
            }
        };
        let _ = conn_tx.send(Ok(()));

        // Make a runtime to allow timeouts on the tokio channels
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_time()
            .build()
            .unwrap();
        let _ = internal.run_one_connection(connection, rt.handle());
        // Mark use as disconnected
        disconnected.cancel();
    }

    fn connect(addr: impl ToSocketAddrs + std::fmt::Debug) -> Result<Telnet> {
        let sa = addr
            .to_socket_addrs()?
            .next()
            .ok_or(anyhow!("Could not decode address: {addr:?}"))?;
        debug!("Trying {sa}...");
        let connection = Telnet::connect(sa, 256)?;
        debug!("Connected to {addr:?}");
        Ok(connection)
    }

    /// Do connection negotiation to the point real data starts coming through
    ///
    /// Return the first packet of real data
    fn negotiate_connection(connection: &mut Telnet) -> Result<Vec<u8>> {
        // Our initial burst of expectations
        for (action, option) in [
            (Action::Do, TelnetOption::SuppressGoAhead),
            (Action::Will, TelnetOption::TTYPE),
            (Action::Will, TelnetOption::NAWS),
            (Action::Wont, TelnetOption::NewEnvironment),
            (Action::Wont, TelnetOption::XDISPLOC),
        ] {
            debug!("CTRL: Sending {action:?} {option:?}");
            connection.negotiate(&action, option)?;
        }
        // Do the negotiation until we get the first data
        loop {
            let event = connection.read()?;
            if !matches!(event, Event::Data(_)) {
                debug!("CTRL: {event:?}");
            }

            match event {
                Event::Negotiation(Action::Do, option) => match option {
                    TelnetOption::NAWS => {
                        debug!("CTRL:   └ Sending NAWS");
                        connection.subnegotiate(TelnetOption::NAWS, &[0x00, 0x78, 0x00, 0x28])?;
                    }
                    TelnetOption::Echo => {
                        let response = Action::Wont;
                        debug!("CTRL:   └ Responding {response:?}");
                        connection.negotiate(&response, option)?;
                    }
                    TelnetOption::TSPEED => {
                        let response = Action::Will;
                        debug!("CTRL:   └ Responding {response:?}");
                        connection.negotiate(&response, option)?;
                    }
                    TelnetOption::LFLOW => {
                        let response = Action::Wont;
                        debug!("CTRL:   └ Responding {response:?}");
                        connection.negotiate(&response, option)?;
                    }
                    _ => (),
                },
                Event::Negotiation(Action::Will, option) => {
                    let response = match option {
                        TelnetOption::Echo => Action::Dont,
                        TelnetOption::Status => Action::Dont,
                        _ => continue,
                    };
                    debug!("CTRL:   └ Responding {response:?}");
                    connection.negotiate(&response, option)?;
                }
                // Event::Negotiation(Action::Wont)
                Event::Subnegotiation(TelnetOption::TTYPE, data) if data.first() == Some(&1) => {
                    debug!("CTRL:   └ Responding TTYPE = xterm");
                    connection.subnegotiate(TelnetOption::TTYPE, "xterm".as_bytes())?;
                }
                Event::Subnegotiation(TelnetOption::TSPEED, data) if data.first() == Some(&1) => {
                    debug!("CTRL:   └ Responding TSPEED = 9600,9600");
                    connection.subnegotiate(TelnetOption::TSPEED, "38400,38400".as_bytes())?;
                }
                Event::Data(d) => {
                    debug!("Received first data, opening negotiation complete");
                    // First data means we have finished the opening negotiations
                    break Ok(d.into_vec());
                }
                _ => continue,
            }
        }
    }

    /// Do logging and passing of data to the user
    fn handle_data(&mut self, data: Vec<u8>) -> Result<(), mpsc::error::SendError<Vec<u8>>> {
        trace!(
            "Receieved:\n{}",
            String::from_utf8_lossy(&data)
                .lines()
                .map(|s| format!("> {s}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        self.data_tx.blocking_send(data)
    }

    fn run_one_connection(&mut self, mut connection: Telnet, handle: &Handle) -> Result<()> {
        self.handle_data(Self::negotiate_connection(&mut connection)?)?;

        // Now we are past negotiation, start the main loop
        while !self.cancel.is_cancelled() {
            // Consume everything waiting until we get NoData from the API
            loop {
                let event = connection.read_nonblocking()?;
                let data = match event {
                    Event::NoData => {
                        break;
                    }
                    Event::Data(d) => d.into_vec(),
                    w => {
                        debug!("CTRL: {w:?}");
                        continue;
                    }
                };
                self.handle_data(data)?;
            }
            // Now, check to see if we need to handle messages from tokio

            if let Ok(data) = handle.block_on(async {
                tokio::time::timeout(Duration::from_millis(100), self.req_rx.recv()).await
            }) {
                let Some((send, request)) = data else {
                    debug!("Communication receiver dropped; terminating telnet connection");
                    break;
                };
                send.send(connection.write(&request))
                    .map_err(|_| anyhow!("Could not send data to internal process"))?;
            }
        }
        debug!("Terminating single telnet connection");
        Ok(())
    }
}

impl AsyncRead for AsyncTelnet {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        // If we have data in the buffer, just return that
        if !self.buffer.is_empty() {
            let to_copy = std::cmp::min(self.buffer.len(), buf.remaining());
            buf.put_slice(&self.buffer[..to_copy]);
            self.buffer.drain(0..to_copy);
            return Poll::Ready(Ok(()));
        }
        assert!(self.buffer.is_empty());
        // Get any new data
        match self.data_rx.poll_recv(cx) {
            Poll::Ready(Some(mut data)) => {
                if data.is_empty() {
                    Poll::Pending
                } else {
                    let to_copy = std::cmp::min(data.len(), buf.remaining());
                    buf.put_slice(&data[..to_copy]);
                    data.drain(0..to_copy);
                    if !data.is_empty() {
                        self.buffer = data;
                    }
                    Poll::Ready(Ok(()))
                }
            }
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

struct TelnetPromptDecoder {}

impl Decoder for TelnetPromptDecoder {
    type Item = Vec<u8>;

    type Error = anyhow::Error;

    fn decode(
        &mut self,
        src: &mut tokio_util::bytes::BytesMut,
    ) -> std::result::Result<Option<Self::Item>, Self::Error> {
        let data_re = Regex::new(r"(root)?:/> ").unwrap();
        let (start, end) = match data_re.find(src) {
            Some(rematch) => (rematch.start(), rematch.end()),
            None => return Ok(None),
        };
        let mut data_out = src.split_to(end);
        data_out.truncate(start);
        Ok(Some(data_out.to_vec()))
    }
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
        .await
        .unwrap();
    let data = reader.next().await.unwrap().unwrap();
    let response = str::from_utf8(&data).unwrap();
    let lines: Vec<_> = response.lines().collect();
    assert_eq!(lines.len(), 2);
    let status: i32 = lines[1].parse().unwrap();
    if status != 0 {
        return Err(anyhow!("Got nonzero return status: {status}"));
    }
    // Convert the data to ints
    let data: Vec<u8> = lines[0]
        .split_whitespace()
        .map(|v| u8::from_str_radix(&v[2..], 16).unwrap())
        .collect::<Vec<_>>();
    let temperature = 21.875 * (u16::from_be_bytes([data[0], data[1]]) as f32) / 8192.0 - 45.0;
    let humidity = 12.5 * (u16::from_be_bytes([data[3], data[4]]) as f32) / 8192.0;
    Ok((temperature, humidity))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let mut library = IntercomProvider::new();
    let pv_connected = library
        .add_pv("BL24I-JUNGFRAU:CTRL:CONNECTED", 0i8)
        .unwrap();
    let pv_temperature = library
        .add_pv("BL24I-JUNGFRAU:CTRL:TEMPERATURE", 20f32)
        .unwrap();
    let pv_humidity = library
        .add_pv("BL24I-JUNGFRAU:CTRL:HUMIDITY", 0f32)
        .unwrap();
    let pv_switch = library.add_pv("BL24I-JUNGFRAU:CTRL:SWITCH", 0f32).unwrap();
    let pv_switch = library.add_pv("BL24I-JUNGFRAU:CTRL:POWER", 0f32).unwrap();

    // Don't start the server until we have the first value
    let mut server = None;

    loop {
        let connection = match AsyncTelnet::connect("i24-jf9mb-ctrl:23").await {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to establish telnet connection: {e}. Waiting before new attempt...");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        let mut reader = FramedRead::new(connection, TelnetPromptDecoder {});
        debug!("Discarding initial frame");
        let _ = reader.next().await.unwrap().unwrap();

        // The last time we queried an update
        let mut last_requested_update = Instant::now() - Duration::from_secs(60);
        // The main loop, most time should be spent in here
        loop {
            select! {
                _ = tokio::time::sleep_until((last_requested_update + Duration::from_secs(10)).into()) => {
                    let (temperature, humidity) = query_sht33_values(&mut reader).await.unwrap();
                    pv_temperature.store(&temperature);
                    pv_humidity.store(&humidity);
                    println!("SHT33: Temperature: {temperature:-.2}°C   Humidity: {humidity:.1} %",);
                    last_requested_update = Instant::now();
                    // Start the server, if we didn't yet
                    if server.is_none() {
                        server = Some(ServerBuilder::new(library.clone()).start().await.unwrap());
                    }
                },
                _ = reader.get_ref().disconnected() => break,
            }
        }
        // Just a general sleep before trying again
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
