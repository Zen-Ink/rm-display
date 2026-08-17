use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rand::{rngs::OsRng, RngCore};
use rm_display_core::{MockPanel, PanelBackend, RefreshPolicyConfig, RefreshProfile};
use rm_display_receiver::{
    ReceiverConfig, ReceiverLimits, ReceiverServer, ReservedZeroToken, SecurityMode,
};
use rm_display_transport::{Psk, PskLoadError};

fn main() {
    if let Err(error) = run() {
        eprintln!("rm-display-receiver: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let arguments = Arguments::parse(env::args().skip(1))?;
    let mut refresh_policy = RefreshPolicyConfig::for_profile(arguments.epaper_profile);
    if let Some(interval) = arguments.full_refresh_interval {
        refresh_policy.cleanup_after_updates = interval;
    }
    refresh_policy.damage_tile = arguments.damage_tile;
    let state_directory = default_state_directory()?;
    let (security, transport_name, managed_psk_path) =
        match (arguments.plaintext, arguments.psk_file) {
            (false, Some(psk_file)) => {
                let psk = Psk::load(&psk_file).map_err(|error| {
                    format!("cannot load PSK file {}: {error}", psk_file.display())
                })?;
                (
                    SecurityMode::Psk(psk),
                    "psk-aes-128-gcm; TLS 1.3 external PSK",
                    Some(psk_file),
                )
            }
            (true, None) => (
                SecurityMode::Plaintext,
                "plaintext; unauthenticated and unencrypted",
                None,
            ),
            (false, None) => {
                let path = state_directory.join("pairing.psk");
                let psk = load_or_create_psk(&path)?;
                (
                    SecurityMode::Psk(psk),
                    "receiver-managed PSK; TLS 1.3 AES-128-GCM",
                    Some(path),
                )
            }
            (true, Some(_)) => unreachable!("conflicting security arguments were validated"),
        };

    let server_id = match arguments.server_id {
        Some(server_id) => server_id,
        None => load_or_create_server_id(&state_directory.join("server-id"))?,
    };
    let config = ReceiverConfig {
        listen: arguments.listen,
        security,
        token_verifier: Arc::new(ReservedZeroToken),
        server_id,
        name: "rm-display".into(),
        limits: ReceiverLimits::default(),
        refresh_policy,
        input_device: arguments.input_device,
    };
    let panel = create_panel(arguments.mock_geometry)?;
    let mut server = ReceiverServer::bind(config, panel).map_err(|error| error.to_string())?;
    server.set_managed_psk_path(managed_psk_path);
    eprintln!("rm-display-receiver: {}", server.input_status());
    eprintln!(
        "rm-display-receiver: e-paper profile={}, full-refresh-interval={}, damage-tile={}",
        refresh_policy.profile.as_str(),
        refresh_policy.cleanup_after_updates,
        refresh_policy.damage_tile,
    );
    eprintln!(
        "rm-display-receiver: listening on {} ({transport_name})",
        server.local_addr().map_err(|error| error.to_string())?,
    );
    if arguments.pairing_qr {
        server
            .show_pairing_qr()
            .map_err(|error| format!("cannot display pairing QR: {error}"))?;
        eprintln!("rm-display-receiver: pairing QR displayed");
    }
    server.run().map_err(|error| error.to_string())
}

fn create_panel(mock_geometry: Option<(u32, u32)>) -> Result<Box<dyn PanelBackend>, String> {
    if let Some((width, height)) = mock_geometry {
        return Ok(Box::new(MockPanel::new(width, height)));
    }

    #[cfg(all(feature = "quill", target_os = "linux", target_arch = "aarch64"))]
    {
        return rm_display_receiver::quill::QuillPanel::open()
            .map(|panel| Box::new(panel) as Box<dyn PanelBackend>)
            .map_err(|error| error.to_string());
    }

    #[cfg(not(all(feature = "quill", target_os = "linux", target_arch = "aarch64")))]
    Err("this build has no Quill backend; pass --mock WIDTHxHEIGHT for host development".into())
}

#[derive(Debug)]
struct Arguments {
    listen: SocketAddr,
    psk_file: Option<PathBuf>,
    plaintext: bool,
    server_id: Option<[u8; 16]>,
    mock_geometry: Option<(u32, u32)>,
    input_device: Option<PathBuf>,
    epaper_profile: RefreshProfile,
    full_refresh_interval: Option<u32>,
    damage_tile: u32,
    pairing_qr: bool,
}

impl Arguments {
    fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut parsed = Self {
            listen: "0.0.0.0:7420".parse().expect("static socket address"),
            psk_file: None,
            plaintext: false,
            server_id: None,
            mock_geometry: None,
            input_device: None,
            epaper_profile: RefreshProfile::Balanced,
            full_refresh_interval: None,
            damage_tile: RefreshPolicyConfig::default().damage_tile,
            pairing_qr: true,
        };
        for argument in arguments {
            if let Some(value) = argument.strip_prefix("--listen=") {
                parsed.listen = value
                    .parse()
                    .map_err(|_| format!("invalid --listen address: {value}"))?;
            } else if let Some(value) = argument.strip_prefix("--psk-file=") {
                if value.is_empty() {
                    return Err("--psk-file requires a path".into());
                }
                if parsed.psk_file.replace(PathBuf::from(value)).is_some() {
                    return Err("--psk-file may be specified only once".into());
                }
            } else if argument == "--plaintext" {
                parsed.plaintext = true;
            } else if let Some(value) = argument.strip_prefix("--server-id=") {
                parsed.server_id = Some(parse_hex::<16>(value)?);
            } else if let Some(value) = argument.strip_prefix("--mock=") {
                parsed.mock_geometry = Some(parse_geometry(value)?);
            } else if let Some(value) = argument.strip_prefix("--input=") {
                parsed.input_device = Some(PathBuf::from(value));
            } else if let Some(value) = argument.strip_prefix("--epaper-profile=") {
                parsed.epaper_profile = parse_epaper_profile(value)?;
            } else if let Some(value) = argument.strip_prefix("--full-refresh-interval=") {
                parsed.full_refresh_interval = Some(
                    value
                        .parse()
                        .map_err(|_| "--full-refresh-interval must be a non-negative integer")?,
                );
            } else if let Some(value) = argument.strip_prefix("--damage-tile=") {
                parsed.damage_tile = value
                    .parse()
                    .map_err(|_| "--damage-tile must be a positive integer")?;
                if parsed.damage_tile == 0 {
                    return Err("--damage-tile must be a positive integer".into());
                }
            } else if argument == "--no-pairing-qr" {
                parsed.pairing_qr = false;
            } else if argument == "--help" || argument == "-h" {
                return Err(usage().into());
            } else {
                return Err(format!("unknown argument: {argument}\n{}", usage()));
            }
        }
        if parsed.plaintext && parsed.psk_file.is_some() {
            return Err("--plaintext and --psk-file are mutually exclusive".into());
        }
        if !parsed.pairing_qr && !parsed.plaintext && parsed.psk_file.is_none() {
            return Err(
                "--no-pairing-qr requires --plaintext or --psk-file because a generated PSK would be unreachable"
                    .into(),
            );
        }
        Ok(parsed)
    }
}

fn parse_epaper_profile(value: &str) -> Result<RefreshProfile, String> {
    match value {
        "realtime" => Ok(RefreshProfile::Realtime),
        "animate" => Ok(RefreshProfile::Animate),
        "balanced" => Ok(RefreshProfile::Balanced),
        "reading" => Ok(RefreshProfile::Reading),
        "quality" => Ok(RefreshProfile::Quality),
        _ => {
            Err("--epaper-profile must be realtime, animate, balanced, reading, or quality".into())
        }
    }
}

fn parse_geometry(value: &str) -> Result<(u32, u32), String> {
    let (width, height) = value
        .split_once('x')
        .ok_or("mock geometry must be WIDTHxHEIGHT")?;
    let width = width.parse().map_err(|_| "invalid mock width")?;
    let height = height.parse().map_err(|_| "invalid mock height")?;
    if width == 0 || height == 0 {
        return Err("mock dimensions must be nonzero".into());
    }
    Ok((width, height))
}

fn parse_hex<const N: usize>(value: &str) -> Result<[u8; N], String> {
    if value.len() != N * 2 || !value.is_ascii() {
        return Err(format!("expected {} hexadecimal characters", N * 2));
    }
    let mut output = [0_u8; N];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| "invalid hexadecimal value")?;
    }
    Ok(output)
}

fn random_server_id() -> [u8; 16] {
    let mut id = [0_u8; 16];
    while id == [0; 16] {
        OsRng.fill_bytes(&mut id);
    }
    id
}

fn default_state_directory() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("XDG_STATE_HOME").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path).join("rm-display"));
    }
    if let Some(path) = env::var_os("HOME").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path)
            .join(".local")
            .join("state")
            .join("rm-display"));
    }
    let executable = env::current_exe()
        .map_err(|error| format!("cannot locate receiver executable for state storage: {error}"))?;
    let parent = executable
        .parent()
        .ok_or("receiver executable has no parent directory")?;
    Ok(parent.join(".state"))
}

fn load_or_create_psk(path: &Path) -> Result<Psk, String> {
    match Psk::load(path) {
        Ok(psk) => Ok(psk),
        Err(PskLoadError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            let psk = Psk::generate();
            match psk.store_new(path) {
                Ok(()) => Ok(psk),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Psk::load(path)
                    .map_err(|error| {
                        format!(
                            "cannot load concurrently created PSK {}: {error}",
                            path.display()
                        )
                    }),
                Err(error) => Err(format!(
                    "cannot persist managed PSK at {}: {error}",
                    path.display()
                )),
            }
        }
        Err(error) => Err(format!(
            "cannot load managed PSK {}: {error}",
            path.display()
        )),
    }
}

fn load_or_create_server_id(path: &Path) -> Result<[u8; 16], String> {
    match load_server_id(path) {
        Ok(server_id) => Ok(server_id),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let server_id = random_server_id();
            match store_server_id(path, server_id) {
                Ok(()) => Ok(server_id),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    load_server_id(path).map_err(|error| {
                        format!(
                            "cannot load concurrently created receiver identity {}: {error}",
                            path.display()
                        )
                    })
                }
                Err(error) => Err(format!(
                    "cannot persist receiver identity {}: {error}",
                    path.display()
                )),
            }
        }
        Err(error) => Err(format!(
            "cannot load receiver identity {}: {error}",
            path.display()
        )),
    }
}

fn load_server_id(path: &Path) -> std::io::Result<[u8; 16]> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "identity path is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "identity file permissions must be 0600 or stricter",
            ));
        }
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    parse_hex::<16>(text.trim_end())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn store_server_id(path: &Path, server_id: [u8; 16]) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    for byte in server_id {
        write!(file, "{byte:02x}")?;
    }
    file.write_all(b"\n")?;
    file.sync_all()
}

fn usage() -> &'static str {
    "usage: rm-display-receiver [--listen=IP:PORT] [--plaintext|--psk-file=FILE] [--server-id=32HEX] [--mock=WIDTHxHEIGHT] [--input=/dev/input/eventN] [--epaper-profile=realtime|animate|balanced|reading|quality] [--full-refresh-interval=N] [--damage-tile=PIXELS] [--no-pairing-qr]"
}

#[cfg(test)]
mod tests {
    use super::{load_or_create_psk, load_or_create_server_id, Arguments, RefreshProfile};
    use std::path::PathBuf;

    #[test]
    fn managed_pairing_state_is_reused_across_startups() {
        let directory = std::env::current_dir()
            .unwrap()
            .join(".cache")
            .join("rm-display-receiver-main-tests")
            .join(format!("state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let psk_path = directory.join("pairing.psk");
        let identity_path = directory.join("server-id");

        let first_psk = load_or_create_psk(&psk_path).unwrap().pairing_hex();
        let first_identity = load_or_create_server_id(&identity_path).unwrap();
        assert_eq!(
            load_or_create_psk(&psk_path).unwrap().pairing_hex(),
            first_psk
        );
        assert_eq!(
            load_or_create_server_id(&identity_path).unwrap(),
            first_identity
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn defaults_to_receiver_managed_psk_pairing() {
        let arguments = Arguments::parse(Vec::<String>::new()).unwrap();
        assert_eq!(arguments.listen, "0.0.0.0:7420".parse().unwrap());
        assert_eq!(arguments.psk_file, None);
        assert!(!arguments.plaintext);
        assert_eq!(arguments.epaper_profile, RefreshProfile::Balanced);
        assert_eq!(arguments.full_refresh_interval, None);
        assert_eq!(arguments.damage_tile, 64);
        assert!(arguments.pairing_qr);
    }

    #[test]
    fn psk_file_enables_psk_configuration() {
        let arguments = Arguments::parse(["--psk-file=/keys/display.psk".into()]).unwrap();
        assert_eq!(arguments.psk_file, Some(PathBuf::from("/keys/display.psk")));
    }

    #[test]
    fn plaintext_is_explicit_and_excludes_psk_file() {
        let arguments = Arguments::parse(["--plaintext".into()]).unwrap();
        assert!(arguments.plaintext);
        assert!(Arguments::parse(["--plaintext".into(), "--psk-file=key".into()]).is_err());
    }

    #[test]
    fn rejects_duplicate_psk_file() {
        let error =
            Arguments::parse(["--psk-file=first".into(), "--psk-file=second".into()]).unwrap_err();
        assert!(error.contains("only once"));
    }

    #[test]
    fn removed_tls_options_are_not_silently_accepted() {
        let error = Arguments::parse(["--tls-cert=old.pem".into()]).unwrap_err();
        assert!(error.contains("unknown argument"));
    }

    #[test]
    fn parses_epaper_policy_options() {
        let arguments = Arguments::parse([
            "--epaper-profile=quality".into(),
            "--full-refresh-interval=12".into(),
            "--damage-tile=32".into(),
        ])
        .unwrap();
        assert_eq!(arguments.epaper_profile, RefreshProfile::Quality);
        assert_eq!(arguments.full_refresh_interval, Some(12));
        assert_eq!(arguments.damage_tile, 32);

        assert_eq!(
            Arguments::parse(["--epaper-profile=realtime".into()])
                .unwrap()
                .epaper_profile,
            RefreshProfile::Realtime
        );
        assert_eq!(
            Arguments::parse(["--epaper-profile=reading".into()])
                .unwrap()
                .epaper_profile,
            RefreshProfile::Reading
        );
    }

    #[test]
    fn rejects_invalid_epaper_policy_options() {
        assert!(Arguments::parse(["--epaper-profile=fast".into()]).is_err());
        assert!(Arguments::parse(["--damage-tile=0".into()]).is_err());
        assert!(Arguments::parse(["--full-refresh-interval=-1".into()]).is_err());
    }

    #[test]
    fn startup_pairing_qr_can_be_disabled_explicitly() {
        assert!(
            !Arguments::parse(["--no-pairing-qr".into(), "--plaintext".into()])
                .unwrap()
                .pairing_qr
        );
        assert!(Arguments::parse(["--no-pairing-qr".into()]).is_err());
    }
}
