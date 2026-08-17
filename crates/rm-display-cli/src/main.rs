use std::fs::OpenOptions;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use rand::RngCore;
use rm_display_cli::pixels::{fit_raw_gray8, frame_len, load_image_gray8, FitMode};
use rm_display_cli::stats::StatsReporter;
use rm_display_cli::transport;
use rm_display_cli::ProducerClient;
use rm_display_protocol::{
    ContentClass, EpaperProfile, EpaperProfileConfiguration, EpaperWaveform, FrameIntent,
    SourceKind,
};
use rm_display_transport::Psk;
use serde_json::json;

#[derive(Debug, Parser)]
#[command(
    name = "rm-display-cli",
    version,
    about = "rm-display v2 Linux producer"
)]
struct Cli {
    /// Receiver host or IP address.
    #[arg(long, global = true, default_value = "10.11.99.1")]
    host: String,
    /// Receiver TCP port.
    #[arg(long, global = true, default_value_t = 7420)]
    port: u16,
    /// Enable TLS 1.3 AES-128-GCM using a mode-0600 file containing a 32-byte PSK as 64 hex digits.
    #[arg(long, global = true, value_name = "FILE")]
    psk_file: Option<PathBuf>,
    /// Socket read/write/connect timeout.
    #[arg(long, global = true, default_value_t = 15)]
    timeout_seconds: u64,
    /// Print per-frame timing and a p50/p95/max summary to stderr.
    #[arg(long, global = true)]
    stats: bool,
    /// Write machine-readable per-frame and summary metrics as JSONL; '-' means stdout.
    #[arg(long, global = true, value_name = "PATH")]
    stats_jsonl: Option<PathBuf>,
    /// Request a receiver-owned e-paper policy for this connection only.
    #[arg(long, global = true, value_enum)]
    epaper_profile: Option<EpaperProfileArg>,
    /// CUSTOM waveform for LATEST text/UI and mixed frames.
    #[arg(long, global = true, value_enum)]
    latest_text_waveform: Option<EpaperWaveformArg>,
    /// CUSTOM waveform for LATEST photo frames.
    #[arg(long, global = true, value_enum)]
    latest_photo_waveform: Option<EpaperWaveformArg>,
    /// CUSTOM waveform for LATEST video frames.
    #[arg(long, global = true, value_enum)]
    latest_video_waveform: Option<EpaperWaveformArg>,
    /// CUSTOM waveform for every SETTLED frame.
    #[arg(long, global = true, value_enum)]
    settled_waveform: Option<EpaperWaveformArg>,
    /// CUSTOM first-frame full cleanup policy.
    #[arg(
        long,
        global = true,
        value_name = "BOOL",
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    clean_first_frame: Option<bool>,
    /// CUSTOM damage comparison tile (power of two, 8 through 512).
    #[arg(long, global = true, value_parser = clap::value_parser!(u32).range(8..=512))]
    damage_tile: Option<u32>,
    /// Enable or disable receiver partial refreshes for this connection.
    #[arg(
        long,
        global = true,
        value_name = "BOOL",
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    partial_refresh_enabled: Option<bool>,
    /// Periodic cleanup interval in successful partial panel submissions; zero disables it.
    #[arg(long, global = true)]
    cleanup_after_updates: Option<u32>,
    /// Full-clean threshold as bounding damage percent; zero disables it.
    #[arg(long, global = true, value_parser = clap::value_parser!(u32).range(0..=100))]
    large_update_threshold_percent: Option<u32>,
    /// At SETTLED, clean once after this many fast updates; zero disables it.
    #[arg(long, global = true)]
    static_cleanup_after_fast_updates: Option<u32>,
    /// Immediately request one receiver-decided full-panel cleanup.
    #[arg(long, global = true)]
    cleanup_now: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Verify the selected transport and protocol Hello exchange.
    Probe,
    /// Print negotiated receiver/display capabilities as JSON.
    Info,
    /// Query the effective connection-scoped e-paper profile as JSON.
    Profile,
    /// Query effective connection-scoped e-paper refresh parameters as JSON.
    Refresh,
    /// Decode, scale and display one image as a SETTLED frame.
    Show(ShowArgs),
    /// Stream fixed-size raw Gray8 frames from stdin; EOF is repeated as SETTLED.
    Stream(StreamArgs),
    /// Exercise Hello, SurfaceOpen and one SETTLED keyframe.
    Doctor,
    /// Present a white surface and print generic input/actions as JSONL.
    Events(EventsArgs),
}

#[derive(Debug, Args)]
struct ShowArgs {
    image: PathBuf,
    #[arg(long, value_enum, default_value_t = FitMode::Contain)]
    fit: FitMode,
    /// Write input/actions as JSONL to '-' (stdout) or a file while waiting.
    #[arg(long, value_name = "PATH")]
    events_jsonl: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct StreamArgs {
    /// Width of each raw stdin frame; defaults to the negotiated receiver surface width.
    #[arg(long, requires = "height", value_parser = clap::value_parser!(u32).range(1..))]
    width: Option<u32>,
    /// Height of each raw stdin frame; defaults to the negotiated receiver surface height.
    #[arg(long, requires = "width", value_parser = clap::value_parser!(u32).range(1..))]
    height: Option<u32>,
    #[arg(long, value_enum, default_value_t = FitMode::Contain)]
    fit: FitMode,
    /// Write receiver input/actions as JSONL to '-' (stdout) or a file.
    #[arg(long, value_name = "PATH")]
    events_jsonl: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct EventsArgs {
    /// JSONL destination; '-' means stdout.
    #[arg(long, default_value = "-")]
    output: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum EpaperProfileArg {
    Realtime,
    Animate,
    Balanced,
    Reading,
    Quality,
    Custom,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EpaperWaveformArg {
    Fastest,
    Fast,
    Quality,
}

impl From<EpaperWaveformArg> for EpaperWaveform {
    fn from(value: EpaperWaveformArg) -> Self {
        match value {
            EpaperWaveformArg::Fastest => Self::Fastest,
            EpaperWaveformArg::Fast => Self::Fast,
            EpaperWaveformArg::Quality => Self::Quality,
        }
    }
}

impl From<EpaperProfileArg> for EpaperProfile {
    fn from(value: EpaperProfileArg) -> Self {
        match value {
            EpaperProfileArg::Realtime => Self::Realtime,
            EpaperProfileArg::Animate => Self::Animate,
            EpaperProfileArg::Balanced => Self::Balanced,
            EpaperProfileArg::Reading => Self::Reading,
            EpaperProfileArg::Quality => Self::Quality,
            EpaperProfileArg::Custom => Self::Custom,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let custom_selected = cli.epaper_profile == Some(EpaperProfileArg::Custom);
    if !custom_selected
        && (cli.latest_text_waveform.is_some()
            || cli.latest_photo_waveform.is_some()
            || cli.latest_video_waveform.is_some()
            || cli.settled_waveform.is_some()
            || cli.clean_first_frame.is_some()
            || cli.damage_tile.is_some())
    {
        bail!("custom waveform, clean-first-frame, and damage-tile options require --epaper-profile custom");
    }
    if custom_selected && cli.damage_tile.is_some_and(|tile| !tile.is_power_of_two()) {
        bail!("--damage-tile must be a power of two");
    }
    let mut client = connect_client(&cli)?;
    let server = client.hello("rm-display-cli")?.clone();
    if let Some(profile) = cli.epaper_profile {
        let result = if custom_selected {
            client.request_custom_epaper_profile(EpaperProfileConfiguration {
                latest_text_waveform: EpaperWaveform::from(
                    cli.latest_text_waveform.unwrap_or(EpaperWaveformArg::Fast),
                ) as i32,
                latest_photo_waveform: EpaperWaveform::from(
                    cli.latest_photo_waveform
                        .unwrap_or(EpaperWaveformArg::Quality),
                ) as i32,
                latest_video_waveform: EpaperWaveform::from(
                    cli.latest_video_waveform
                        .unwrap_or(EpaperWaveformArg::Fastest),
                ) as i32,
                settled_waveform: EpaperWaveform::from(
                    cli.settled_waveform.unwrap_or(EpaperWaveformArg::Quality),
                ) as i32,
                partial_refresh_enabled: cli.partial_refresh_enabled.unwrap_or(true),
                cleanup_after_updates: cli.cleanup_after_updates.unwrap_or(90),
                clean_first_frame: cli.clean_first_frame.unwrap_or(true),
                large_update_threshold_percent: cli.large_update_threshold_percent.unwrap_or(0),
                static_cleanup_after_fast_updates: cli
                    .static_cleanup_after_fast_updates
                    .unwrap_or(6),
                damage_tile: cli.damage_tile.unwrap_or(64),
            })?
        } else {
            client.request_epaper_profile(profile.into())?
        };
        let active = result.active.as_ref().context("profile state missing")?;
        eprintln!(
            "e-paper profile={} cleanup_after_updates={} damage_tile={} cleanup_performed={} cleanup_pending={}",
            EpaperProfile::try_from(active.profile)
                .map(|profile| profile.as_str_name())
                .unwrap_or("EPAPER_PROFILE_UNSPECIFIED"),
            active.cleanup_after_updates,
            active.damage_tile,
            result.cleanup_performed,
            result.cleanup_pending,
        );
    }
    if !custom_selected
        && (cli.partial_refresh_enabled.is_some()
            || cli.cleanup_after_updates.is_some()
            || cli.large_update_threshold_percent.is_some()
            || cli.static_cleanup_after_fast_updates.is_some())
    {
        let result = client.update_epaper_refresh(
            cli.partial_refresh_enabled,
            cli.cleanup_after_updates,
            cli.large_update_threshold_percent,
            cli.static_cleanup_after_fast_updates,
        )?;
        eprintln!("e-paper refresh={}", epaper_refresh_json(&result));
    }
    if cli.cleanup_now {
        let result = client.request_epaper_cleanup()?;
        eprintln!("e-paper cleanup={}", epaper_refresh_json(&result));
    }
    let mut stats = StatsReporter::new(cli.stats, cli.stats_jsonl.as_deref())?;

    match cli.command {
        Command::Probe => {
            println!(
                "ok receiver_id={} receiver={}",
                server_id(&server),
                server.name
            );
            client.goodbye()?;
        }
        Command::Info => {
            println!("{}", serde_json::to_string_pretty(&server_json(&server))?);
            client.goodbye()?;
        }
        Command::Profile => {
            let result = client.query_epaper_profile()?;
            println!(
                "{}",
                serde_json::to_string_pretty(&epaper_profile_json(&result))?
            );
            client.goodbye()?;
        }
        Command::Refresh => {
            let result = client.query_epaper_refresh()?;
            println!(
                "{}",
                serde_json::to_string_pretty(&epaper_refresh_json(&result))?
            );
            client.goodbye()?;
        }
        Command::Show(args) => {
            install_event_output(&mut client, args.events_jsonl.as_deref())?;
            let surface = client.open_surface(
                0,
                0,
                SourceKind::Document,
                args.events_jsonl.is_some(),
                "Linux image",
            )?;
            let prepare_started = Instant::now();
            let pixels = load_image_gray8(&args.image, surface.width, surface.height, args.fit)?;
            let prepare_us = elapsed_us(prepare_started);
            let report = client.send_frame_report(
                &surface,
                &pixels,
                FrameIntent::Settled,
                ContentClass::Mixed,
            )?;
            stats.record("settled", 0, prepare_us, &report)?;
            stats.finish()?;
            eprintln!("presented frame {}", report.result.presented_frame_id);
            client.goodbye()?;
        }
        Command::Stream(args) => {
            install_event_output(&mut client, args.events_jsonl.as_deref())?;
            let surface = client.open_surface(
                0,
                0,
                SourceKind::LinuxStream,
                args.events_jsonl.is_some(),
                "Linux raw Gray8 stream",
            )?;
            stream_stdin(&mut client, &surface, &args, &mut stats)?;
            stats.finish()?;
            client.goodbye()?;
        }
        Command::Doctor => {
            let surface = client.open_surface(0, 0, SourceKind::TestPattern, false, "doctor")?;
            let pixels = vec![255; frame_len(surface.width, surface.height)?];
            let report = client.send_frame_report(
                &surface,
                &pixels,
                FrameIntent::Settled,
                ContentClass::TextUi,
            )?;
            stats.record("settled", 0, 0, &report)?;
            stats.finish()?;
            println!(
                "{}",
                json!({
                    "status": "ok",
                    "surface": {"width": surface.width, "height": surface.height},
                    "logical_frame_id": report.result.logical_frame_id,
                    "presented_frame_id": report.result.presented_frame_id,
                    "credits": report.result.credits,
                })
            );
            client.goodbye()?;
        }
        Command::Events(args) => {
            install_event_output(&mut client, Some(&args.output))?;
            let surface = client.open_surface(0, 0, SourceKind::TestPattern, true, "events")?;
            let pixels = vec![255; frame_len(surface.width, surface.height)?];
            let report = client.send_frame_report(
                &surface,
                &pixels,
                FrameIntent::Settled,
                ContentClass::TextUi,
            )?;
            stats.record("settled", 0, 0, &report)?;
            stats.finish()?;
            loop {
                client.pump_once()?;
            }
        }
    }
    Ok(())
}

fn connect_client(cli: &Cli) -> Result<ProducerClient> {
    let psk = cli
        .psk_file
        .as_deref()
        .map(Psk::load)
        .transpose()
        .context("cannot load PSK")?;
    let io = transport::connect(
        &cli.host,
        cli.port,
        psk,
        Duration::from_secs(cli.timeout_seconds),
        if matches!(&cli.command, Command::Events(_)) {
            None
        } else {
            Some(Duration::from_secs(cli.timeout_seconds))
        },
    )?;
    let mut client_id = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut client_id);
    Ok(ProducerClient::new(io, client_id))
}

fn install_event_output(client: &mut ProducerClient, path: Option<&Path>) -> Result<()> {
    let Some(path) = path else { return Ok(()) };
    if path == Path::new("-") {
        client.set_event_output(Box::new(io::stdout()));
    } else {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .with_context(|| format!("cannot open JSONL output {}", path.display()))?;
        client.set_event_output(Box::new(file));
    }
    Ok(())
}

fn stream_stdin(
    client: &mut ProducerClient,
    surface: &rm_display_cli::Surface,
    args: &StreamArgs,
    stats: &mut StatsReporter,
) -> Result<()> {
    let (source_width, source_height) = match (args.width, args.height) {
        (None, None) => (surface.width, surface.height),
        (Some(width), Some(height)) => (width, height),
        _ => bail!("--width and --height must be specified together"),
    };
    let source_len = frame_len(source_width, source_height)?;
    let mut input = io::stdin().lock();
    let mut source = vec![0u8; source_len];
    let mut latest = None;
    let mut count = 0u64;
    loop {
        let source_wait_started = Instant::now();
        let complete = read_frame(&mut input, &mut source)?;
        let source_wait_us = elapsed_us(source_wait_started);
        match complete {
            false => break,
            true => {
                let prepare_started = Instant::now();
                let pixels = fit_raw_gray8(
                    &source,
                    source_width,
                    source_height,
                    surface.width,
                    surface.height,
                    args.fit,
                )?;
                let prepare_us = elapsed_us(prepare_started);
                let report = client.send_frame_report(
                    surface,
                    &pixels,
                    FrameIntent::Latest,
                    ContentClass::Mixed,
                )?;
                stats.record("latest", source_wait_us, prepare_us, &report)?;
                latest = Some(pixels);
                count += 1;
            }
        }
    }
    let latest = latest.context("raw stdin contained no complete frames")?;
    let report =
        client.send_frame_report(surface, &latest, FrameIntent::Settled, ContentClass::Mixed)?;
    stats.record("settled", 0, 0, &report)?;
    eprintln!("streamed {count} source frames; EOF SETTLED presented");
    Ok(())
}

fn elapsed_us(started: Instant) -> u64 {
    started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64
}

fn read_frame(reader: &mut dyn Read, destination: &mut [u8]) -> Result<bool> {
    let mut offset = 0usize;
    while offset < destination.len() {
        match reader.read(&mut destination[offset..]) {
            Ok(0) if offset == 0 => return Ok(false),
            Ok(0) => bail!(
                "raw stdin ended with a partial frame: {} of {} bytes",
                offset,
                destination.len()
            ),
            Ok(amount) => offset += amount,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(true)
}

fn server_id(server: &rm_display_protocol::ServerHello) -> String {
    hex::encode(&server.server_id)
}

fn server_json(server: &rm_display_protocol::ServerHello) -> serde_json::Value {
    let display = server.display.as_ref();
    let limits = server.limits.as_ref();
    json!({
        "name": server.name,
        "server_id": server_id(server),
        "minor": server.selected_minor,
        "features": server.features,
        "display": display.map(|display| json!({
            "width": display.width,
            "height": display.height,
            "orientation": display.orientation,
            "pixel_formats": display.pixel_formats,
            "encodings": display.encodings,
            "input_capabilities": display.input_capabilities,
        })),
        "limits": limits.map(|limits| json!({
            "max_payload": limits.max_payload,
            "max_frame_bytes": limits.max_frame_bytes,
            "max_regions": limits.max_regions,
            "max_inflight": limits.max_inflight,
            "max_inflight_bytes": limits.max_inflight_bytes,
            "max_fps_x100": limits.max_fps_x100,
            "settled_deadline_ms": limits.settled_deadline_ms,
        })),
    })
}

fn epaper_refresh_json(result: &rm_display_protocol::EpaperRefreshResult) -> serde_json::Value {
    let active = result.active.as_ref();
    json!({
        "request_id": result.request_id,
        "operation": result.operation,
        "result": result.result,
        "cleanup_performed": result.cleanup_performed,
        "message": result.message,
        "active": active.map(|state| json!({
            "partial_refresh_enabled": state.partial_refresh_enabled,
            "cleanup_after_updates": state.cleanup_after_updates,
            "large_update_threshold_percent": state.large_update_threshold_percent,
            "presented_since_full_refresh": state.presented_since_full_refresh,
            "cleanup_pending": state.cleanup_pending,
            "static_cleanup_after_fast_updates": state.static_cleanup_after_fast_updates,
            "fast_updates_since_settled": state.fast_updates_since_settled,
        })),
    })
}

fn epaper_profile_json(result: &rm_display_protocol::EpaperProfileResult) -> serde_json::Value {
    let active = result.active.as_ref();
    json!({
        "request_id": result.request_id,
        "operation": result.operation,
        "requested_profile": result.requested_profile,
        "result": result.result,
        "cleanup_performed": result.cleanup_performed,
        "cleanup_pending": result.cleanup_pending,
        "message": result.message,
        "active": active.map(|state| json!({
            "profile": state.profile,
            "profile_name": EpaperProfile::try_from(state.profile)
                .map(|profile| profile.as_str_name())
                .unwrap_or("EPAPER_PROFILE_UNSPECIFIED"),
            "cleanup_after_updates": state.cleanup_after_updates,
            "large_update_threshold_percent": state.large_update_threshold_percent,
            "damage_tile": state.damage_tile,
            "clean_first_frame": state.clean_first_frame,
            "static_cleanup_after_fast_updates": state.static_cleanup_after_fast_updates,
            "effective": state.effective.as_ref().map(|effective| json!({
                "latest_text_waveform": effective.latest_text_waveform,
                "latest_text_waveform_name": epaper_waveform_name(effective.latest_text_waveform),
                "latest_photo_waveform": effective.latest_photo_waveform,
                "latest_photo_waveform_name": epaper_waveform_name(effective.latest_photo_waveform),
                "latest_video_waveform": effective.latest_video_waveform,
                "latest_video_waveform_name": epaper_waveform_name(effective.latest_video_waveform),
                "settled_waveform": effective.settled_waveform,
                "settled_waveform_name": epaper_waveform_name(effective.settled_waveform),
                "partial_refresh_enabled": effective.partial_refresh_enabled,
                "cleanup_after_updates": effective.cleanup_after_updates,
                "clean_first_frame": effective.clean_first_frame,
                "large_update_threshold_percent": effective.large_update_threshold_percent,
                "static_cleanup_after_fast_updates": effective.static_cleanup_after_fast_updates,
                "damage_tile": effective.damage_tile,
            })),
        })),
    })
}

fn epaper_waveform_name(value: i32) -> &'static str {
    EpaperWaveform::try_from(value)
        .map(|waveform| waveform.as_str_name())
        .unwrap_or("EPAPER_WAVEFORM_UNSPECIFIED")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_options_are_accepted_before_or_after_the_subcommand() {
        let before = Cli::try_parse_from([
            "rm-display-cli",
            "--host",
            "192.0.2.10",
            "--port",
            "17420",
            "info",
        ])
        .unwrap();
        assert_eq!(before.host, "192.0.2.10");
        assert_eq!(before.port, 17420);
        assert!(matches!(before.command, Command::Info));

        let after = Cli::try_parse_from([
            "rm-display-cli",
            "info",
            "--host",
            "192.0.2.11",
            "--port",
            "17421",
            "--timeout-seconds",
            "3",
        ])
        .unwrap();
        assert_eq!(after.host, "192.0.2.11");
        assert_eq!(after.port, 17421);
        assert_eq!(after.timeout_seconds, 3);
        assert!(matches!(after.command, Command::Info));
    }

    #[test]
    fn default_host_is_the_remarkable_usb_address() {
        let cli = Cli::try_parse_from(["rm-display-cli", "probe"]).unwrap();
        assert_eq!(cli.host, "10.11.99.1");
        assert_eq!(cli.port, 7420);
    }

    #[test]
    fn online_epaper_profile_is_a_global_connection_option() {
        let cli = Cli::try_parse_from(["rm-display-cli", "--epaper-profile", "quality", "doctor"])
            .unwrap();
        assert!(matches!(
            cli.epaper_profile,
            Some(EpaperProfileArg::Quality)
        ));

        let realtime =
            Cli::try_parse_from(["rm-display-cli", "--epaper-profile", "realtime", "profile"])
                .unwrap();
        assert!(matches!(
            realtime.epaper_profile,
            Some(EpaperProfileArg::Realtime)
        ));
        assert!(matches!(realtime.command, Command::Profile));

        let reading =
            Cli::try_parse_from(["rm-display-cli", "profile", "--epaper-profile", "reading"])
                .unwrap();
        assert!(matches!(
            reading.epaper_profile,
            Some(EpaperProfileArg::Reading)
        ));
    }

    #[test]
    fn profile_query_json_names_appended_profiles() {
        let value = epaper_profile_json(&rm_display_protocol::EpaperProfileResult {
            request_id: 7,
            operation: rm_display_protocol::EpaperProfileOperation::Query as i32,
            requested_profile: EpaperProfile::Unspecified as i32,
            result: rm_display_protocol::EpaperProfileResultCode::Unchanged as i32,
            active: Some(rm_display_protocol::EpaperProfileState {
                profile: EpaperProfile::Reading as i32,
                cleanup_after_updates: 45,
                large_update_threshold_percent: 50,
                damage_tile: 64,
                clean_first_frame: true,
                static_cleanup_after_fast_updates: 3,
                effective: None,
            }),
            cleanup_performed: false,
            cleanup_pending: false,
            message: "current".to_owned(),
        });

        assert_eq!(value["active"]["profile_name"], "EPAPER_PROFILE_READING");
        assert_eq!(value["active"]["cleanup_after_updates"], 45);
        assert_eq!(value["active"]["large_update_threshold_percent"], 50);
    }

    #[test]
    fn refresh_parameters_keep_false_and_zero_as_present_values() {
        let cli = Cli::try_parse_from([
            "rm-display-cli",
            "--partial-refresh-enabled",
            "false",
            "--cleanup-after-updates",
            "0",
            "--large-update-threshold-percent",
            "0",
            "--static-cleanup-after-fast-updates",
            "0",
            "--cleanup-now",
            "refresh",
        ])
        .unwrap();
        assert_eq!(cli.partial_refresh_enabled, Some(false));
        assert_eq!(cli.cleanup_after_updates, Some(0));
        assert_eq!(cli.large_update_threshold_percent, Some(0));
        assert_eq!(cli.static_cleanup_after_fast_updates, Some(0));
        assert!(cli.cleanup_now);
        assert!(matches!(cli.command, Command::Refresh));
    }

    #[test]
    fn stream_dimensions_default_to_receiver_and_explicit_dimensions_are_paired() {
        let automatic = Cli::try_parse_from(["rm-display-cli", "stream"]).unwrap();
        let Command::Stream(automatic) = automatic.command else {
            panic!("expected stream command");
        };
        assert_eq!(automatic.width, None);
        assert_eq!(automatic.height, None);

        let explicit = Cli::try_parse_from([
            "rm-display-cli",
            "stream",
            "--width",
            "720",
            "--height",
            "1280",
        ])
        .unwrap();
        let Command::Stream(explicit) = explicit.command else {
            panic!("expected stream command");
        };
        assert_eq!(explicit.width, Some(720));
        assert_eq!(explicit.height, Some(1280));

        assert!(Cli::try_parse_from(["rm-display-cli", "stream", "--width", "720"]).is_err());
        assert!(Cli::try_parse_from(["rm-display-cli", "stream", "--height", "1280"]).is_err());
    }
}
