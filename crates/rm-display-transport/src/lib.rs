//! Shared TCP transport security for rm-display.
//!
//! Plain TCP is provided by callers; this crate implements the TLS 1.3
//! external-PSK mode used by the reference receiver's default deployment.
//! That mode is pinned to `TLS_AES_128_GCM_SHA256` and the pure
//! `psk_ke` key exchange: no certificate, public key, or (EC)DHE key share is
//! involved.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::ptr;

use foreign_types::ForeignTypeRef;
use openssl::error::ErrorStack;
use openssl::ssl::{
    HandshakeError, Ssl, SslContext, SslContextBuilder, SslMethod, SslOptions, SslStream,
    SslVersion,
};
use rand::{rngs::OsRng, RngCore};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

/// The single external-PSK identity defined by protocol v2.
pub const PSK_IDENTITY: &[u8] = b"rm-display-v2";
pub const PSK_LEN: usize = 32;
pub const TLS13_CIPHERSUITE: &str = "TLS_AES_128_GCM_SHA256";

// OpenSSL 1.1.1 through 3.4 define this option, but openssl-rs does not expose
// it.  Keeping the constant local also avoids depending on OpenSSL 3.5's newer
// SSL_OP_PREFER_NO_DHE_KEX option for correctness.
const SSL_OP_ALLOW_NO_DHE_KEX: u64 = 1 << 10;
// OpenSSL 3.5+ uses this preference to avoid generating/selecting an otherwise
// available key share. Older OpenSSL versions ignore the unknown option bit;
// the mandatory post-handshake negotiated-group check remains the safety
// boundary there.
const SSL_OP_PREFER_NO_DHE_KEX: u64 = 1 << 35;
const SSL_CTRL_GET_NEGOTIATED_GROUP: i32 = 134;

/// A 256-bit, high-entropy pre-shared key.
///
/// Debug output is intentionally redacted.
#[derive(Clone)]
pub struct Psk([u8; PSK_LEN]);

impl Psk {
    /// Generates a fresh receiver-managed 256-bit pairing key.
    pub fn generate() -> Self {
        let mut bytes = [0_u8; PSK_LEN];
        OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    pub fn from_bytes(bytes: [u8; PSK_LEN]) -> Self {
        Self(bytes)
    }

    /// Loads exactly 64 hexadecimal digits from a regular file.
    ///
    /// On Unix, group/other permission bits must all be clear (mode 0600 or
    /// stricter). A single trailing LF or CRLF is accepted.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, PskLoadError> {
        let path = path.as_ref();
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(PskLoadError::NotAFile);
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(PskLoadError::InsecurePermissions);
            }
        }

        let mut data = Zeroizing::new(Vec::new());
        file.read_to_end(&mut data)?;
        let text = std::str::from_utf8(&data).map_err(|_| PskLoadError::InvalidEncoding)?;
        let text = text
            .strip_suffix("\r\n")
            .or_else(|| text.strip_suffix('\n'))
            .unwrap_or(text);
        if text.len() != PSK_LEN * 2 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(PskLoadError::InvalidEncoding);
        }
        let mut bytes = [0; PSK_LEN];
        hex::decode_to_slice(text, &mut bytes).map_err(|_| PskLoadError::InvalidEncoding)?;
        Ok(Self(bytes))
    }

    /// Atomically writes the key as a mode-0600 hex file next to its final
    /// destination. No system temporary directory is used.
    pub fn store_atomic(&self, path: impl AsRef<Path>) -> Result<(), std::io::Error> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            create_private_directory(parent)?;
        }
        let temporary = temporary_path(path);
        let result = (|| {
            write_new_psk_file(self, &temporary)?;
            fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    /// Creates a new mode-0600 key file without replacing an existing pairing.
    pub fn store_new(&self, path: impl AsRef<Path>) -> Result<(), std::io::Error> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            create_private_directory(parent)?;
        }
        let result = write_new_psk_file(self, path);
        if result
            .as_ref()
            .is_err_and(|error| error.kind() != std::io::ErrorKind::AlreadyExists)
        {
            let _ = fs::remove_file(path);
        }
        result
    }

    /// Encodes the key for an explicitly displayed pairing offer.
    ///
    /// Callers must treat the returned text as a credential and must not log
    /// or persist it outside the pairing UI.
    pub fn pairing_hex(&self) -> String {
        hex::encode(self.expose())
    }

    fn expose(&self) -> &[u8; PSK_LEN] {
        &self.0
    }
}

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn write_new_psk_file(psk: &Psk, path: &Path) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let encoded = Zeroizing::new(hex::encode(psk.expose()));
    file.write_all(encoded.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut nonce = [0_u8; 8];
    OsRng.fill_bytes(&mut nonce);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("pairing.psk");
    path.with_file_name(format!(
        ".{file_name}.new-{}-{}",
        std::process::id(),
        hex::encode(nonce)
    ))
}

impl fmt::Debug for Psk {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Psk([REDACTED])")
    }
}

impl Drop for Psk {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Debug, Error)]
pub enum PskLoadError {
    #[error("cannot read PSK file: {0}")]
    Io(#[from] std::io::Error),
    #[error("PSK path is not a regular file")]
    NotAFile,
    #[error("PSK file must contain exactly 64 hexadecimal digits (32 bytes)")]
    InvalidEncoding,
    #[cfg(unix)]
    #[error("PSK file permissions are too broad; expected mode 0600 or stricter")]
    InsecurePermissions,
}

#[derive(Debug, Error)]
pub enum PskConfigError {
    #[error("cannot configure TLS 1.3 external-PSK transport: {0}")]
    OpenSsl(#[from] ErrorStack),
}

#[derive(Debug, Error)]
pub enum PskTransportError {
    #[error("cannot create TLS connection: {0}")]
    Setup(#[source] ErrorStack),
    #[error("TLS external-PSK handshake failed: {0}")]
    Handshake(String),
    #[error("TLS handshake violated the fixed rm-display PSK profile: {0}")]
    Profile(&'static str),
}

pub type PskStream<S> = SslStream<S>;

#[derive(Clone)]
pub struct PskClientConfig {
    context: SslContext,
}

impl PskClientConfig {
    pub fn new(psk: Psk) -> Result<Self, PskConfigError> {
        let mut builder = base_context()?;
        // Deliberately differ from the server group list. On OpenSSL before
        // 3.5 (which lacks PREFER_NO_DHE_KEX), this leaves no mutually
        // supported group and therefore forces the allowed psk_ke path.
        builder.set_groups_list("X25519")?;
        builder.set_psk_client_callback(move |_, _, identity, destination| {
            let identity_len = PSK_IDENTITY.len();
            if identity.len() <= identity_len || destination.len() < PSK_LEN {
                return Ok(0);
            }
            identity[..identity_len].copy_from_slice(PSK_IDENTITY);
            identity[identity_len] = 0;
            destination[..PSK_LEN].copy_from_slice(psk.expose());
            Ok(PSK_LEN)
        });
        Ok(Self {
            context: builder.build(),
        })
    }

    pub fn connect<S>(&self, stream: S) -> Result<PskStream<S>, PskTransportError>
    where
        S: Read + Write,
    {
        let ssl = Ssl::new(&self.context).map_err(PskTransportError::Setup)?;
        let stream = ssl
            .connect(stream)
            .map_err(|error| PskTransportError::Handshake(format_handshake_error(error)))?;
        validate_profile(&stream)?;
        Ok(stream)
    }
}

#[derive(Clone)]
pub struct PskServerConfig {
    context: SslContext,
}

impl PskServerConfig {
    pub fn new(psk: Psk) -> Result<Self, PskConfigError> {
        let mut builder = base_context()?;
        builder.set_groups_list("P-256")?;
        builder.set_psk_server_callback(move |_, identity, destination| {
            if identity != Some(PSK_IDENTITY) || destination.len() < PSK_LEN {
                return Ok(0);
            }
            destination[..PSK_LEN].copy_from_slice(psk.expose());
            Ok(PSK_LEN)
        });
        Ok(Self {
            context: builder.build(),
        })
    }

    pub fn accept<S>(&self, stream: S) -> Result<PskStream<S>, PskTransportError>
    where
        S: Read + Write,
    {
        let ssl = Ssl::new(&self.context).map_err(PskTransportError::Setup)?;
        let stream = ssl
            .accept(stream)
            .map_err(|error| PskTransportError::Handshake(format_handshake_error(error)))?;
        validate_profile(&stream)?;
        Ok(stream)
    }
}

fn base_context() -> Result<SslContextBuilder, PskConfigError> {
    let mut builder = SslContextBuilder::new(SslMethod::tls())?;
    builder.set_min_proto_version(Some(SslVersion::TLS1_3))?;
    builder.set_max_proto_version(Some(SslVersion::TLS1_3))?;
    builder.set_ciphersuites(TLS13_CIPHERSUITE)?;
    builder.set_max_early_data(0)?;
    builder.set_num_tickets(0)?;
    builder.set_options(
        SslOptions::NO_TICKET
            | SslOptions::NO_COMPRESSION
            | SslOptions::from_bits_retain(SSL_OP_ALLOW_NO_DHE_KEX | SSL_OP_PREFER_NO_DHE_KEX),
    );

    Ok(builder)
}

fn validate_profile<S>(stream: &SslStream<S>) -> Result<(), PskTransportError> {
    let ssl = stream.ssl();
    if ssl.version_str() != "TLSv1.3" {
        return Err(PskTransportError::Profile("protocol is not TLS 1.3"));
    }
    if ssl.current_cipher().map(|cipher| cipher.name()) != Some(TLS13_CIPHERSUITE) {
        return Err(PskTransportError::Profile(
            "cipher suite is not TLS_AES_128_GCM_SHA256",
        ));
    }
    if ssl.peer_certificate().is_some() || ssl.peer_cert_chain().is_some() {
        return Err(PskTransportError::Profile(
            "peer unexpectedly supplied a certificate",
        ));
    }
    if negotiated_group(ssl) != 0 {
        return Err(PskTransportError::Profile(
            "an (EC)DHE key-share group was negotiated",
        ));
    }
    Ok(())
}

/// Returns OpenSSL's negotiated TLS group identifier, or zero for pure psk_ke.
pub fn negotiated_group(ssl: &openssl::ssl::SslRef) -> i64 {
    unsafe {
        openssl_sys::SSL_ctrl(
            ssl.as_ptr(),
            SSL_CTRL_GET_NEGOTIATED_GROUP,
            0,
            ptr::null_mut(),
        ) as i64
    }
}

fn format_handshake_error<S>(error: HandshakeError<S>) -> String {
    match error {
        HandshakeError::SetupFailure(error) => error.to_string(),
        HandshakeError::Failure(stream) | HandshakeError::WouldBlock(stream) => {
            stream.error().to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    use super::*;

    const TEST_PSK: [u8; PSK_LEN] = [0x5a; PSK_LEN];

    #[test]
    fn psk_file_is_replaced_atomically_without_system_temp_storage() {
        let mut nonce = [0_u8; 8];
        OsRng.fill_bytes(&mut nonce);
        let directory = std::env::current_dir()
            .unwrap()
            .join(".cache")
            .join("rm-display-transport-tests")
            .join(hex::encode(nonce));
        let path = directory.join("pairing.psk");

        Psk::from_bytes([0x11; PSK_LEN])
            .store_atomic(&path)
            .unwrap();
        assert_eq!(
            Psk::load(&path).unwrap().pairing_hex(),
            "11".repeat(PSK_LEN)
        );
        Psk::from_bytes([0x22; PSK_LEN])
            .store_atomic(&path)
            .unwrap();
        assert_eq!(
            Psk::load(&path).unwrap().pairing_hex(),
            "22".repeat(PSK_LEN)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn tls13_external_psk_is_aes128_gcm_without_cert_or_group() {
        let server = PskServerConfig::new(Psk::from_bytes(TEST_PSK)).unwrap();
        let client = PskClientConfig::new(Psk::from_bytes(TEST_PSK)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();

        let server_thread = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut tls = server.accept(tcp).unwrap();
            assert_profile(&tls);
            let mut request = [0; 4];
            tls.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"ping");
            tls.write_all(b"pong").unwrap();
        });

        let tcp = TcpStream::connect(address).unwrap();
        let mut tls = client.connect(tcp).unwrap();
        assert_profile(&tls);
        tls.write_all(b"ping").unwrap();
        let mut response = [0; 4];
        tls.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"pong");
        server_thread.join().unwrap();
    }

    #[test]
    fn wrong_psk_cannot_complete_handshake() {
        let server = PskServerConfig::new(Psk::from_bytes(TEST_PSK)).unwrap();
        let client = PskClientConfig::new(Psk::from_bytes([0xa5; PSK_LEN])).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server_thread = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            assert!(server.accept(tcp).is_err());
        });
        assert!(client
            .connect(TcpStream::connect(address).unwrap())
            .is_err());
        server_thread.join().unwrap();
    }

    fn assert_profile<S>(stream: &PskStream<S>) {
        let ssl = stream.ssl();
        assert_eq!(ssl.version_str(), "TLSv1.3");
        assert_eq!(ssl.current_cipher().unwrap().name(), TLS13_CIPHERSUITE);
        assert!(
            ssl.session_reused(),
            "external PSK must select a PSK session"
        );
        assert!(ssl.peer_certificate().is_none());
        assert!(ssl.peer_cert_chain().is_none());
        assert_eq!(negotiated_group(ssl), 0, "psk_ke must not negotiate DHE");
    }
}
