use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use rm_display_transport::{Psk, PskClientConfig, PskConfigError, PskTransportError};
use thiserror::Error;

pub trait ReadWrite: Read + Write + Send {}
impl<T: Read + Write + Send> ReadWrite for T {}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("cannot resolve receiver address")]
    Resolve,
    #[error("TCP I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    PskConfig(#[from] PskConfigError),
    #[error(transparent)]
    Psk(#[from] PskTransportError),
}

/// Connects using plain TCP when `psk` is `None`, otherwise using the fixed
/// TLS 1.3 external-PSK profile. An encrypted handshake failure is never
/// retried as plaintext.
pub fn connect(
    host: &str,
    port: u16,
    psk: Option<Psk>,
    connect_timeout: Duration,
    read_timeout: Option<Duration>,
) -> Result<Box<dyn ReadWrite>, TransportError> {
    let addresses: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|_| TransportError::Resolve)?
        .collect();
    let address = addresses.first().ok_or(TransportError::Resolve)?;
    let tcp = TcpStream::connect_timeout(address, connect_timeout)?;
    tcp.set_read_timeout(Some(connect_timeout))?;
    tcp.set_write_timeout(Some(connect_timeout))?;

    let Some(psk) = psk else {
        tcp.set_read_timeout(read_timeout)?;
        return Ok(Box::new(tcp));
    };
    let config = PskClientConfig::new(psk)?;
    let stream = config.connect(tcp)?;
    stream.get_ref().set_read_timeout(read_timeout)?;
    Ok(Box::new(stream))
}
