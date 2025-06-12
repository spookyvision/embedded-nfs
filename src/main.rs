// https://github.com/jasta/esp32-tokio-demo/issues/2
// tldr is to create a tokio tcp stream from a std::net::TcpStream and set it to nonblocking

// memory debug:
// https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-guides/performance/ram-usage.html#reducing-stack-sizes
// obtain task handle for debug:
// https://github.com/esp-rs/esp-idf-hal/blob/master/src/task.rs#L119
use std::{sync::Mutex, thread::JoinHandle, time::Duration};

use demo_fs::DemoFS;
use embedded_svc::wifi::{ClientConfiguration, Configuration};
use esp_idf_hal::prelude::Peripherals;
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    log::EspLogger,
    nvs::EspDefaultNvsPartition,
    timer::EspTaskTimerService,
    wifi::{AsyncWifi, EspWifi},
};
use esp_idf_sys as _;
use esp_idf_sys::{esp, esp_app_desc, EspError};
use log::{info, warn};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

mod demo_fs;
// Edit these or provide your own way of provisioning...
const WIFI_SSID: Option<&str> = option_env!("WIFI_SSID");
const WIFI_PASSWORD: Option<&str> = option_env!("WIFI_PASSWORD");
const HOSTPORT: u32 = 11111;

// To test, run `cargo run`, then when the server is up, use `nc -v espressif 12345` from
// a machine on the same Wi-Fi network.
const TCP_LISTENING_PORT: u16 = 12345;

esp_app_desc!();

fn main() -> anyhow::Result<()> {
    // It is necessary to call this function once. Otherwise some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();

    std::env::set_var("RUST_LOG", "debug");
    // Bind the log crate to the ESP Logging facilities
    esp_idf_svc::log::EspLogger::initialize_default();
    tracing::info!("yo tracing");

    info!("Size of nfs_server: {}", size_of_future_0(nfs_server));
    // eventfd is needed by our mio poll implementation.  Note you should set max_fds
    // higher if you have other code that may need eventfd.
    info!("Setting up eventfd...");
    let config = esp_idf_sys::esp_vfs_eventfd_config_t {
        max_fds: 1,
        ..Default::default()
    };
    esp! { unsafe { esp_idf_sys::esp_vfs_eventfd_register(&config) } }?;

    info!("Setting up board...");
    let peripherals = Peripherals::take().unwrap();
    let sysloop = EspSystemEventLoop::take()?;
    let timer = EspTaskTimerService::new()?;
    let nvs = EspDefaultNvsPartition::take()?;

    info!("Initializing Wi-Fi...");
    let wifi = AsyncWifi::wrap(
        EspWifi::new(peripherals.modem, sysloop.clone(), Some(nvs))?,
        sysloop,
        timer.clone(),
    )?;

    info!("Starting async run loop");

    // spawn a thread for tokio with explicit stack size, nicer than sdkconfig *and* it seems to work more reliably

    let tokio_thread: JoinHandle<anyhow::Result<()>> = std::thread::Builder::new()
        .stack_size(1024 * 20)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(async move {
                    let mut wifi_loop = WifiLoop { wifi };
                    wifi_loop.configure().await?;
                    wifi_loop.initial_connect().await?;

                    // info!("Spawning echo server...");
                    // tokio::spawn(echo_server());

                    info!("Spawning NFS server…");
                    tokio::spawn(nfs_server());

                    info!("Entering main Wi-Fi run loop...");
                    wifi_loop.stay_connected().await
                })?;
            Ok(())
        })
        .expect("spawn thread failed");

    tokio_thread.join().expect("join thread failed");

    Ok(())
}

pub struct WifiLoop<'a> {
    wifi: AsyncWifi<EspWifi<'a>>,
}

impl<'a> WifiLoop<'a> {
    pub async fn configure(&mut self) -> Result<(), EspError> {
        info!("Setting Wi-Fi credentials...");
        self.wifi
            .set_configuration(&Configuration::Client(ClientConfiguration {
                ssid: WIFI_SSID
                    .expect("need env var: WIFI_SSID")
                    .try_into()
                    .unwrap(),
                password: WIFI_PASSWORD
                    .expect("need env var: WIFI_PASS")
                    .try_into()
                    .unwrap(),
                ..Default::default()
            }))?;

        info!("Starting Wi-Fi driver...");
        self.wifi.start().await
    }

    pub async fn initial_connect(&mut self) -> Result<(), EspError> {
        self.do_connect_loop(true).await
    }

    pub async fn stay_connected(mut self) -> Result<(), EspError> {
        self.do_connect_loop(false).await
    }

    async fn do_connect_loop(&mut self, exit_after_first_connect: bool) -> Result<(), EspError> {
        let wifi = &mut self.wifi;
        loop {
            // Wait for disconnect before trying to connect again.  This loop ensures
            // we stay connected and is commonly missing from trivial examples as it's
            // way too difficult to showcase the core logic of an example and have
            // a proper Wi-Fi event loop without a robust async runtime.  Fortunately, we can do it
            // now!
            wifi.wifi_wait(|w| w.is_up(), None).await?;

            for attempt in 1..=3 {
                info!("Connecting to Wi-Fi ({attempt})...");
                if let Err(e) = wifi.connect().await {
                    warn!("b0rk: {e:?}");
                    // TODO esp_idf_sys::esp_restart
                } else {
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
            }

            info!("Waiting for association...");
            wifi.ip_wait_while(|w| w.is_up().map(|s| !s), None).await?;

            if exit_after_first_connect {
                return Ok(());
            }
        }
    }
}

async fn nfs_server() -> anyhow::Result<()> {
    info!("nfs: creating listener");
    let listener = NFSTcpListener::bind(&format!("0.0.0.0:{HOSTPORT}"), DemoFS::default()).await?;
    info!("nfs: handle_forever()");
    listener.handle_forever().await?;
    Ok(())
}
// Test with
// mount -t nfs -o nolocks,vers=3,tcp,port=12000,mountport=12000,soft 127.0.0.1:/ mnt/

async fn echo_server() -> anyhow::Result<()> {
    let addr = format!("0.0.0.0:{TCP_LISTENING_PORT}");

    info!("Binding to {addr}...");
    let listener = TcpListener::bind(&addr).await?;

    loop {
        info!("Waiting for new connection on socket: {listener:?}");
        let (socket, _) = listener.accept().await?;

        info!("Spawning handle for: {socket:?}...");
        let fut = async move {
            info!("Spawned handler!");
            let peer = socket.peer_addr();
            if let Err(e) = serve_client(socket).await {
                info!("Got error handling {peer:?}: {e:?}");
            }
        };

        tokio::spawn(fut);
    }
}

async fn serve_client(mut stream: TcpStream) -> anyhow::Result<()> {
    info!("Handling {stream:?}...");

    let mut buf = [0u8; 512];
    loop {
        info!("About to read...");
        let n = stream.read(&mut buf).await?;
        info!("Read {n} bytes...");

        if n == 0 {
            break;
        }

        stream.write_all(&buf[0..n]).await?;
        info!("Wrote {n} bytes back...");
    }

    Ok(())
}

// define a fn that takes a function and tell the size of its return type
// if your actual fn takes more than one argument just add additional generics for each argument
// besides the T generic
fn size_of_future_1<F, T, R>(_: F) -> usize
where
    F: FnOnce(T) -> R,
{
    std::mem::size_of::<R>()
}

// define a fn that takes a function and tell the size of its return type
// if your actual fn takes more than one argument just add additional generics for each argument
// besides the T generic
fn size_of_future_0<F, R>(_: F) -> usize
where
    F: FnOnce() -> R,
{
    std::mem::size_of::<R>()
}
