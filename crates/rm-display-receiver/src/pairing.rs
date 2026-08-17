use std::collections::HashSet;
#[cfg(target_os = "linux")]
use std::ffi::CStr;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use qrcode::{Color, EcLevel, QrCode};
use rm_display_core::GraySurface;
use thiserror::Error;

use crate::config::{ReceiverConfig, SecurityMode};

const QUIET_MODULES: usize = 4;
const MAX_ADVERTISED_HOSTS: usize = 4;

/// An IPv4 address together with the Linux interface that owns it.
///
/// Retaining the interface name is intentional: the pairing QR is normally
/// scanned while the device is reachable through Wi-Fi, and IP-address
/// heuristics cannot reliably distinguish that route from USB networking.
#[derive(Clone, Debug, Eq, PartialEq)]
struct InterfaceIpv4 {
    name: String,
    address: Ipv4Addr,
    is_up: bool,
}

#[derive(Debug, Error)]
pub enum PairingError {
    #[error("cannot encode pairing QR: {0}")]
    Encode(#[from] qrcode::types::QrError),
    #[error("display is too small for a scannable pairing QR")]
    DisplayTooSmall,
    #[error("cannot allocate pairing frame: {0}")]
    Surface(#[from] rm_display_core::SurfaceError),
}

pub fn pairing_uri(config: &ReceiverConfig, bound: SocketAddr) -> String {
    let hosts = advertised_hosts(bound);
    let mut uri = String::from("rm-display://pair/v2?");
    for (index, host) in hosts.iter().enumerate() {
        if index != 0 {
            uri.push('&');
        }
        uri.push_str("host=");
        uri.push_str(&host.to_string());
    }
    uri.push_str("&port=");
    uri.push_str(&bound.port().to_string());
    match &config.security {
        SecurityMode::Plaintext => uri.push_str("&security=plain"),
        SecurityMode::Psk(psk) => {
            uri.push_str("&security=psk&psk=");
            uri.push_str(&psk.pairing_hex());
        }
    }
    uri.push_str("&server=");
    for byte in config.server_id {
        use std::fmt::Write;
        let _ = write!(uri, "{byte:02x}");
    }
    uri
}

pub fn render_pairing_frame(
    width: u32,
    height: u32,
    uri: &str,
) -> Result<GraySurface, PairingError> {
    let code = QrCode::with_error_correction_level(uri.as_bytes(), EcLevel::Q)?;
    let modules = code.width();
    let total_modules = modules + QUIET_MODULES * 2;
    let available = width.min(height) as usize * 4 / 5;
    let scale = available / total_modules;
    if scale < 3 {
        return Err(PairingError::DisplayTooSmall);
    }
    let qr_pixels = total_modules * scale;
    let left = (width as usize - qr_pixels) / 2;
    let top = (height as usize - qr_pixels) / 2;
    let mut pixels = vec![255_u8; width as usize * height as usize];

    for module_y in 0..modules {
        for module_x in 0..modules {
            if code[(module_x, module_y)] != Color::Dark {
                continue;
            }
            let x = left + (module_x + QUIET_MODULES) * scale;
            let y = top + (module_y + QUIET_MODULES) * scale;
            for row in y..y + scale {
                let start = row * width as usize + x;
                pixels[start..start + scale].fill(0);
            }
        }
    }
    Ok(GraySurface::from_pixels(width, height, pixels)?)
}

fn advertised_hosts(bound: SocketAddr) -> Vec<IpAddr> {
    select_advertised_hosts(bound, interface_ipv4_addresses())
}

/// Select the addresses published in a pairing descriptor.
///
/// An explicit listen address is authoritative.  With a wildcard bind, rank
/// eligible interfaces by their transport role rather than by address range:
/// `wlan0`, then `usb0`, then every other active interface.  This is kept
/// separate from OS enumeration so the ordering contract is unit-testable.
fn select_advertised_hosts(bound: SocketAddr, addresses: Vec<InterfaceIpv4>) -> Vec<IpAddr> {
    if !bound.ip().is_unspecified() {
        return vec![bound.ip()];
    }
    let mut hosts = addresses
        .into_iter()
        .filter(|interface| {
            let address = interface.address;
            interface.is_up
                && !address.is_unspecified()
                && !address.is_loopback()
                && !address.is_link_local()
                && !address.is_multicast()
        })
        .collect::<Vec<_>>();
    hosts.sort_by_key(interface_host_rank);
    let mut seen = HashSet::new();
    hosts.retain(|interface| seen.insert(interface.address));
    hosts.truncate(MAX_ADVERTISED_HOSTS);
    let mut hosts = hosts
        .into_iter()
        .map(|interface| IpAddr::V4(interface.address))
        .collect::<Vec<_>>();
    if hosts.is_empty() {
        // Stable reMarkable USB gadget endpoint, useful while Wi-Fi is down.
        hosts.push(IpAddr::V4(Ipv4Addr::new(10, 11, 99, 1)));
    }
    hosts
}

fn interface_host_rank(interface: &InterfaceIpv4) -> (u8, u8, String, String) {
    let interface_rank = match interface.name.as_str() {
        "wlan0" => 0,
        "usb0" => 1,
        _ => 2,
    };
    // Keep a stable useful ordering within one interface class, but never let
    // an address range overrule the explicit wlan0 -> usb0 preference.
    let address_rank = match interface.address {
        ip if ip.is_private() && ip != Ipv4Addr::new(10, 11, 99, 1) => 0,
        ip if ip == Ipv4Addr::new(10, 11, 99, 1) => 1,
        _ => 2,
    };
    (
        interface_rank,
        address_rank,
        interface.name.clone(),
        interface.address.to_string(),
    )
}

#[cfg(target_os = "linux")]
fn interface_ipv4_addresses() -> Vec<InterfaceIpv4> {
    let mut head = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Vec::new();
    }
    let mut addresses = Vec::new();
    let mut current = head;
    while !current.is_null() {
        let interface = unsafe { &*current };
        if !interface.ifa_addr.is_null()
            && interface.ifa_flags & libc::IFF_UP as u32 != 0
            && unsafe { (*interface.ifa_addr).sa_family as i32 } == libc::AF_INET
        {
            let socket = unsafe { &*interface.ifa_addr.cast::<libc::sockaddr_in>() };
            if !interface.ifa_name.is_null() {
                let name = unsafe { CStr::from_ptr(interface.ifa_name) }
                    .to_string_lossy()
                    .into_owned();
                addresses.push(InterfaceIpv4 {
                    name,
                    address: Ipv4Addr::from(socket.sin_addr.s_addr.to_ne_bytes()),
                    is_up: interface.ifa_flags & (libc::IFF_UP as u32) != 0,
                });
            }
        }
        current = interface.ifa_next;
    }
    unsafe { libc::freeifaddrs(head) };
    addresses
}

#[cfg(not(target_os = "linux"))]
fn interface_ipv4_addresses() -> Vec<InterfaceIpv4> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use rm_display_core::RefreshPolicyConfig;

    use crate::{ReceiverLimits, ReservedZeroToken};

    fn config(security: SecurityMode) -> ReceiverConfig {
        ReceiverConfig {
            listen: "0.0.0.0:7420".parse().unwrap(),
            security,
            token_verifier: Arc::new(ReservedZeroToken),
            server_id: [0xab; 16],
            name: "rm-display".into(),
            limits: ReceiverLimits::default(),
            refresh_policy: RefreshPolicyConfig::default(),
            input_device: None,
        }
    }

    #[test]
    fn plaintext_descriptor_contains_no_secret() {
        let uri = pairing_uri(
            &config(SecurityMode::Plaintext),
            "10.11.99.1:7420".parse().unwrap(),
        );
        assert_eq!(
            uri,
            "rm-display://pair/v2?host=10.11.99.1&port=7420&security=plain&server=abababababababababababababababab"
        );
        assert!(!uri.contains("psk="));
    }

    #[test]
    fn psk_descriptor_carries_receiver_generated_credential() {
        let uri = pairing_uri(
            &config(SecurityMode::Psk(rm_display_transport::Psk::from_bytes(
                [0x5a; 32],
            ))),
            "10.11.99.1:7420".parse().unwrap(),
        );
        assert_eq!(
            uri,
            format!(
                "rm-display://pair/v2?host=10.11.99.1&port=7420&security=psk&psk={}&server=abababababababababababababababab",
                "5a".repeat(32)
            )
        );
    }

    #[test]
    fn qr_frame_has_white_quiet_area_and_dark_modules() {
        let uri = "rm-display://pair/v2?host=10.11.99.1&port=7420&security=plain&server=abababababababababababababababab";
        let frame = render_pairing_frame(960, 1696, uri).unwrap();
        assert_eq!(frame.pixels()[0], 255);
        assert!(frame.pixels().contains(&0));
        assert!(frame.pixels().contains(&255));
    }

    fn interface(name: &str, address: [u8; 4]) -> InterfaceIpv4 {
        InterfaceIpv4 {
            name: name.into(),
            address: Ipv4Addr::from(address),
            is_up: true,
        }
    }

    #[test]
    fn wildcard_pairing_prefers_wlan0_then_usb0_then_other_interfaces() {
        let hosts = select_advertised_hosts(
            "0.0.0.0:7420".parse().unwrap(),
            vec![
                interface("eth0", [192, 168, 50, 2]),
                interface("usb0", [10, 11, 99, 1]),
                interface("wlan0", [192, 168, 1, 42]),
                interface("wlan0", [192, 168, 1, 43]),
            ],
        );
        assert_eq!(
            hosts,
            vec![
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 43)),
                IpAddr::V4(Ipv4Addr::new(10, 11, 99, 1)),
                IpAddr::V4(Ipv4Addr::new(192, 168, 50, 2)),
            ]
        );
    }

    #[test]
    fn wildcard_pairing_filters_ineligible_addresses_before_interface_ranking() {
        let hosts = select_advertised_hosts(
            "[::]:7420".parse().unwrap(),
            vec![
                interface("wlan0", [127, 0, 0, 1]),
                interface("wlan0", [169, 254, 1, 2]),
                interface("usb0", [10, 11, 99, 1]),
                interface("eth0", [192, 168, 50, 2]),
            ],
        );
        assert_eq!(
            hosts,
            vec![
                IpAddr::V4(Ipv4Addr::new(10, 11, 99, 1)),
                IpAddr::V4(Ipv4Addr::new(192, 168, 50, 2)),
            ]
        );
    }

    #[test]
    fn wildcard_pairing_ignores_addresses_on_down_interfaces() {
        let mut down_wlan = interface("wlan0", [192, 168, 1, 42]);
        down_wlan.is_up = false;
        let hosts = select_advertised_hosts(
            "0.0.0.0:7420".parse().unwrap(),
            vec![down_wlan, interface("usb0", [10, 11, 99, 1])],
        );
        assert_eq!(hosts, vec![IpAddr::V4(Ipv4Addr::new(10, 11, 99, 1))]);
    }

    #[test]
    fn wildcard_pairing_deduplicates_an_address_owned_by_multiple_interfaces() {
        let hosts = select_advertised_hosts(
            "0.0.0.0:7420".parse().unwrap(),
            vec![
                interface("usb0", [192, 168, 1, 42]),
                interface("wlan0", [192, 168, 1, 42]),
                interface("eth0", [192, 168, 1, 43]),
            ],
        );
        assert_eq!(
            hosts,
            vec![
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 43)),
            ]
        );
    }

    #[test]
    fn explicit_bind_address_is_not_reordered_or_replaced() {
        let hosts = select_advertised_hosts(
            "192.168.50.99:7420".parse().unwrap(),
            vec![
                interface("wlan0", [192, 168, 1, 42]),
                interface("usb0", [10, 11, 99, 1]),
            ],
        );
        assert_eq!(hosts, vec![IpAddr::V4(Ipv4Addr::new(192, 168, 50, 99))]);
    }

    #[test]
    fn wildcard_pairing_keeps_usb_fallback_when_no_eligible_interface_exists() {
        let hosts = select_advertised_hosts(
            "0.0.0.0:7420".parse().unwrap(),
            vec![interface("lo", [127, 0, 0, 1])],
        );
        assert_eq!(hosts, vec![IpAddr::V4(Ipv4Addr::new(10, 11, 99, 1))]);
    }
}
