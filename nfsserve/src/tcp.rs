use std::{
    io,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use async_trait::async_trait;
use tokio::{io::AsyncWriteExt, net::TcpListener, sync::mpsc};
use tracing::{debug, error, info, warn};

use crate::{context::RPCContext, rpcwire::*, vfs::NFSFileSystem};

/// A NFS Tcp Connection Handler
pub struct NFSTcpListener<T: NFSFileSystem + Send + Sync + 'static> {
    listener: TcpListener,
    port: u16,
    arcfs: Arc<T>,
    mount_signal: Option<mpsc::Sender<bool>>,
}

pub fn generate_host_ip(hostnum: u16) -> String {
    format!(
        "127.88.{}.{}",
        ((hostnum >> 8) & 0xFF) as u8,
        (hostnum & 0xFF) as u8
    )
}

/// processes an established socket
async fn process_socket(
    mut socket: tokio::net::TcpStream,
    context: RPCContext,
) -> Result<(), anyhow::Error> {
    let (mut message_handler, mut socksend, mut msgrecvchan) = SocketMessageHandler::new(&context);
    let _ = socket.set_nodelay(true);

    if !false {
        tokio::spawn(async move {
            info!("entering message handler loop");
            loop {
                if let Err(e) = message_handler.read().await {
                    // debug!("Message loop broken due to {:?}", e);
                    debug!("Message loop broken");
                    break;
                }
            }
        });
    }

    info!("entering select loop");
    loop {
        info!("select loop");
        tokio::select! {
            _ = socket.readable() => {
                // TODO original code used 128000
                let mut buf = [0; 128];

                match socket.try_read(&mut buf) {
                    Ok(0) => {
                        return Ok(());
                    }
                    Ok(n) => {
                        info!("shoveling {n} bytes");
                        let _ = socksend.write_all(&buf[..n]).await;
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        continue;
                    }
                    Err(e) => {
                        // debug!("Message handling closed : {:?}", e);
                        warn!("Message handling closed");
                        return Err(e.into());
                    }
                }

            },
            reply = msgrecvchan.recv() => {
                // info!("exiting early (2)");
                // return Ok(());
                match reply {
                    Some(Err(e)) => {
                        //debug!("Message handling closed : {:?}", e);
                        warn!("Message handling closed");
                        return Err(e);
                    }
                    Some(Ok(msg)) => {
                        if let Err(e) = write_fragment(&mut socket, &msg).await {
                            error!("Write error {:?}", e);
                        }
                    }
                    None => {
                        return Err(anyhow::anyhow!("Unexpected socket context termination"));
                    }
                }
            }
        }
    }
}

#[async_trait]
pub trait NFSTcp: Send + Sync {
    /// Gets the true listening port. Useful if the bound port number is 0
    fn get_listen_port(&self) -> u16;

    /// Gets the true listening IP. Useful on windows when the IP may be random
    fn get_listen_ip(&self) -> IpAddr;

    /// Sets a mount listener. A "true" signal will be sent on a mount
    /// and a "false" will be sent on an unmount
    fn set_mount_listener(&mut self, signal: mpsc::Sender<bool>);

    /// Loops forever and never returns handling all incoming connections.
    async fn handle_forever(&self) -> io::Result<()>;
}

impl<T: NFSFileSystem + Send + Sync + 'static> NFSTcpListener<T> {
    /// Binds to a ipstr of the form [ip address]:port. For instance
    /// "127.0.0.1:12000". fs is an instance of an implementation
    /// of NFSFileSystem.
    pub async fn bind(ipstr: &str, fs: T) -> io::Result<NFSTcpListener<T>> {
        let (ip, port) = ipstr.split_once(':').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "IP Address must be of form ip:port",
            )
        })?;
        let port = port.parse::<u16>().map_err(|_| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "Port not in range 0..=65535",
            )
        })?;

        let arcfs: Arc<T> = Arc::new(fs);

        if ip == "auto" {
            let mut num_tries_left = 32;

            for try_ip in 1u16.. {
                let ip = generate_host_ip(try_ip);

                let result = NFSTcpListener::bind_internal(&ip, port, arcfs.clone()).await;

                match &result {
                    Err(_) => {
                        if num_tries_left == 0 {
                            return result;
                        } else {
                            num_tries_left -= 1;
                            continue;
                        }
                    }
                    Ok(_) => {
                        return result;
                    }
                }
            }
            unreachable!(); // Does not detect automatically that loop above never terminates.
        } else {
            // Otherwise, try this.
            NFSTcpListener::bind_internal(ip, port, arcfs).await
        }
    }

    async fn bind_internal(ip: &str, port: u16, arcfs: Arc<T>) -> io::Result<NFSTcpListener<T>> {
        let ipstr = format!("{ip}:{port}");
        let listener = TcpListener::bind(&ipstr).await?;
        info!("Listening on {:?}", &ipstr);

        let port = match listener.local_addr().unwrap() {
            SocketAddr::V4(s) => s.port(),
            SocketAddr::V6(s) => s.port(),
        };
        Ok(NFSTcpListener {
            listener,
            port,
            arcfs,
            mount_signal: None,
        })
    }
}

#[async_trait]
impl<T: NFSFileSystem + Send + Sync + 'static> NFSTcp for NFSTcpListener<T> {
    /// Gets the true listening port. Useful if the bound port number is 0
    fn get_listen_port(&self) -> u16 {
        let addr = self.listener.local_addr().unwrap();
        addr.port()
    }

    fn get_listen_ip(&self) -> IpAddr {
        let addr = self.listener.local_addr().unwrap();
        addr.ip()
    }

    /// Sets a mount listener. A "true" signal will be sent on a mount
    /// and a "false" will be sent on an unmount
    fn set_mount_listener(&mut self, signal: mpsc::Sender<bool>) {
        self.mount_signal = Some(signal);
    }

    /// Loops forever and never returns handling all incoming connections.
    async fn handle_forever(&self) -> io::Result<()> {
        info!("entering nfs loop");
        loop {
            let (socket, _) = self.listener.accept().await?;
            let context = RPCContext {
                local_port: self.port,
                client_addr: socket.peer_addr().unwrap().to_string(),
                auth: crate::rpc::auth_unix::default(),
                vfs: self.arcfs.clone(),
                mount_signal: self.mount_signal.clone(),
            };
            info!("Accepting connection from {}", context.client_addr);
            // info!(
            //     "Accepting socket {:?} - context size: {:?}",
            //     std::mem::size_of_val(&socket),
            //     std::mem::size_of_val(&context)
            // );

            tokio::spawn(async move {
                let _ = process_socket(socket, context).await;
            });
        }
    }
}
