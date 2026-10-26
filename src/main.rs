//! A terminal UI for the mihomo (Clash.Meta) external controller.
//!
//! The shape of the program: a single-threaded UI loop owns [`App`] and all
//! rendering, while a handful of background tasks own the network. They talk
//! over one channel in each direction — [`Command`] out, [`Event`] back — so
//! the UI never blocks on I/O and the network code never touches app state.

mod api;
mod app;
// Atomic file replacement shared by the persistence layers: unit tests pin
// the behavior, but nothing consumes it until the profile worker (task 11)
// wires the write path — drop the escape hatch once they do.
#[allow(dead_code)]
mod atomic;
// Foundation for the subscription feature: unit tests pin the API below, but
// no production code consumes it until the profile worker and the test pool
// land — drop the escape hatch once they do.
#[allow(dead_code)]
mod paths;
// Subscription persistence: unit tests pin the API, but nothing consumes it
// until the import flow (task 5) and the profile worker (task 11) land —
// drop the escape hatch once they do.
#[allow(dead_code)]
mod store;
// Subscription format sniffing: unit tests pin the API, but nothing consumes
// it until the import flow (task 5) and the merge worker (task 7) land —
// drop the escape hatch once they do.
#[allow(dead_code)]
mod subscription;
// Config block model, per-block diff and the merge engine: unit tests pin the
// API, but nothing consumes it until the runtime renderer (task 8), the
// confirm page (task 10) and the profile worker (task 11) land — drop the
// escape hatch once they do.
#[allow(dead_code)]
mod merge;
// Runtime config rendering: unit tests pin the API, but nothing consumes it
// until the confirm page (task 10) and the profile worker (task 11) land —
// drop the escape hatch once they do.
#[allow(dead_code)]
mod runtime;
mod ui;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use tokio::sync::mpsc;

use api::Client;
use app::{App, Command, Event, Snapshot, ToastKind};

const USAGE: &str = "\
mihomo-tui — a terminal client for the mihomo (Clash.Meta) API

USAGE:
    mihomo-tui [OPTIONS]

OPTIONS:
    --endpoint <URL>     Controller address          [default: http://127.0.0.1:9090]
    --secret <SECRET>    Bearer token, if the controller requires one
    --delay-url <URL>    URL used for latency tests
                                                    [default: http://www.gstatic.com/generate_204]
    --timeout <MS>       Per-test timeout in ms      [default: 5000]
    --refresh <MS>       State refresh interval in ms [default: 2000]
    --log-level <LEVEL>  Log stream level: debug|info|warning|error [default: info]
    --config-dir <DIR>   Directory for profiles, configs and TUI settings
    --concurrency <N>    Parallel delay tests        [default: 8]
    -h, --help           Print this help

KEYS:
    1-4 / tab            switch tab        q / ctrl-c   quit
    j/k, h/l             move              enter        select proxy
    t / T                test member/group r            refresh
    m / M                cycle mode        x / X        close connection(s)
";

#[derive(Debug, Clone)]
struct Options {
    endpoint: String,
    secret: Option<String>,
    delay_url: String,
    timeout_ms: u64,
    refresh_ms: u64,
    log_level: String,
    /// Relocates the whole mihomo-tui state tree; resolved by `Paths`.
    // Read together with the paths module once subscription management lands.
    #[allow(dead_code)]
    config_dir: Option<String>,
    /// Overrides the concurrency stored in `config.json` for delay tests.
    #[allow(dead_code)]
    concurrency: Option<usize>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            endpoint: api::DEFAULT_ENDPOINT.to_string(),
            secret: None,
            delay_url: api::DEFAULT_DELAY_URL.to_string(),
            timeout_ms: api::DEFAULT_TIMEOUT_MS,
            refresh_ms: 2000,
            log_level: api::DEFAULT_LOG_LEVEL.to_string(),
            config_dir: None,
            concurrency: None,
        }
    }
}

/// Parse `--flag value` and `--flag=value` pairs. Anything else is an error.
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Option<Options>> {
    let mut options = Options::default();
    let mut args = args.into_iter().peekable();

    while let Some(arg) = args.next() {
        // Split `--flag=value` into its two halves.
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag.to_string(), Some(value.to_string())),
            None => (arg.clone(), None),
        };

        let mut value = |flag: &str| -> Result<String> {
            if let Some(value) = inline.clone() {
                return Ok(value);
            }
            args.next()
                .ok_or_else(|| anyhow!("{flag} needs a value\n\n{USAGE}"))
        };

        match flag.as_str() {
            "-h" | "--help" => return Ok(None),
            "--endpoint" | "--api" => options.endpoint = value(&flag)?,
            "--secret" => options.secret = Some(value(&flag)?),
            "--delay-url" => options.delay_url = value(&flag)?,
            "--timeout" => {
                let raw = value(&flag)?;
                options.timeout_ms = raw.parse().map_err(|_| {
                    anyhow!("--timeout expects a number of milliseconds, got `{raw}`")
                })?;
            }
            "--refresh" => {
                let raw = value(&flag)?;
                options.refresh_ms = raw.parse().map_err(|_| {
                    anyhow!("--refresh expects a number of milliseconds, got `{raw}`")
                })?;
            }
            "--log-level" => options.log_level = value(&flag)?,
            "--config-dir" => options.config_dir = Some(value(&flag)?),
            "--concurrency" => {
                let raw = value(&flag)?;
                options.concurrency = Some(
                    raw.parse::<usize>()
                        .ok()
                        .filter(|n| *n >= 1)
                        .ok_or_else(|| {
                            anyhow!("--concurrency expects a positive integer, got `{raw}`")
                        })?,
                );
            }
            other => return Err(anyhow!("unknown option `{other}`\n\n{USAGE}")),
        }
    }

    Ok(Some(options))
}

/// Restores the terminal however we leave the loop — normal exit or panic.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = ratatui::try_restore();
    }
}

fn main() -> ExitCode {
    let options = match parse_args(std::env::args().skip(1)) {
        Ok(Some(options)) => options,
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    // A multi-thread runtime: the UI task must stay responsive while delay
    // tests (which can take seconds each) are in flight.
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("error: could not start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(options)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(options: Options) -> Result<()> {
    let client = Arc::new(Client::new(&options.endpoint, options.secret.clone())?);

    let mut terminal = ratatui::try_init()?;
    let _guard = TerminalGuard;

    let mut app = App::new(
        // Show the normalised endpoint the client actually dials, so a bare
        // `127.0.0.1:9090` is displayed with its scheme.
        client.endpoint().trim_end_matches('/').to_string(),
        options.delay_url.clone(),
        options.timeout_ms,
    );
    app.set_toast(ToastKind::Info, format!("connecting to {}", app.endpoint));

    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();

    spawn_key_reader(tx.clone());
    spawn_ticker(tx.clone());
    spawn_refresh(client.clone(), tx.clone(), options.refresh_ms);
    spawn_streams(client.clone(), tx.clone(), &options);

    terminal.draw(|frame| ui::draw(frame, &app, Instant::now()))?;

    while let Some(first) = rx.recv().await {
        // Coalesce whatever else has arrived so a burst of events costs one redraw.
        let mut events = vec![first];
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        for event in events {
            if let Some(command) = app.on_event(event) {
                spawn_command(command, client.clone(), tx.clone());
            }
        }

        terminal.draw(|frame| ui::draw(frame, &app, Instant::now()))?;

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

/// Read keys on a dedicated thread — `crossterm::event::read` blocks, and
/// blocking a runtime worker would stall every in-flight request.
fn spawn_key_reader(tx: mpsc::UnboundedSender<Event>) {
    std::thread::spawn(move || {
        loop {
            match crossterm::event::read() {
                Ok(crossterm::event::Event::Key(key)) => {
                    // Key releases are noise; only react to presses.
                    if key.kind == crossterm::event::KeyEventKind::Release {
                        continue;
                    }
                    if tx.send(Event::Key(key)).is_err() {
                        break; // the UI loop is gone
                    }
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
    });
}

/// Drives redraws and toast expiry independently of network activity.
fn spawn_ticker(tx: mpsc::UnboundedSender<Event>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(250));
        loop {
            interval.tick().await;
            if tx.send(Event::Tick).is_err() {
                break;
            }
        }
    });
}

/// Re-reads configs, groups and connections forever.
fn spawn_refresh(client: Arc<Client>, tx: mpsc::UnboundedSender<Event>, refresh_ms: u64) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(refresh_ms.max(500)));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;

            if tx.send(Event::Tick).is_err() {
                break;
            }

            let (version, configs, groups, connections) = tokio::join!(
                client.version(),
                client.configs(),
                client.groups(),
                client.connections(),
            );

            let fatal = configs
                .as_ref()
                .err()
                .or(groups.as_ref().err())
                .map(short_reason);
            if let Some(err) = fatal {
                // Either read failing means the controller is unreachable or
                // answering garbage — say so instead of retrying into the void
                // silently, and skip the snapshot so stale state is not masked.
                if tx.send(Event::Fatal(err)).is_err() {
                    break;
                }
                continue;
            }

            let snapshot = Snapshot {
                version: version.ok(),
                configs: configs.ok(),
                groups: groups.unwrap_or_default(),
                connections: connections.unwrap_or_default(),
            };
            if tx.send(Event::Snapshot(Box::new(snapshot))).is_err() {
                break;
            }
        }
    });
}

/// Long-lived `/traffic` and `/logs` connections, re-established if dropped
/// (mihomo closes them whenever the config is reloaded).
fn spawn_streams(client: Arc<Client>, tx: mpsc::UnboundedSender<Event>, options: &Options) {
    let traffic_client = client.clone();
    let traffic_tx = tx.clone();
    tokio::spawn(async move {
        loop {
            if let Ok(response) = traffic_client.traffic_stream().await {
                pump(response, &traffic_tx, |line| {
                    serde_json::from_str::<api::Traffic>(line)
                        .ok()
                        .map(|t| Event::Traffic {
                            up: t.up,
                            down: t.down,
                        })
                })
                .await;
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
            if traffic_tx.is_closed() {
                break;
            }
        }
    });

    let log_client = client;
    let log_tx = tx;
    let level = options.log_level.clone();
    tokio::spawn(async move {
        loop {
            if let Ok(response) = log_client.log_stream(&level).await {
                pump(response, &log_tx, |line| {
                    serde_json::from_str::<api::LogLine>(line)
                        .ok()
                        .map(Event::Log)
                })
                .await;
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
            if log_tx.is_closed() {
                break;
            }
        }
    });
}

/// Read a newline-delimited JSON stream and hand each line to `parse`.
async fn pump(
    mut response: reqwest::Response,
    tx: &mpsc::UnboundedSender<Event>,
    parse: impl Fn(&str) -> Option<Event>,
) {
    let mut buffer = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        buffer.extend_from_slice(&chunk);

        for line in drain_lines(&mut buffer) {
            if let Some(event) = parse(&line)
                && tx.send(event).is_err()
            {
                return;
            }
        }

        // Defensive: a stream that never newline-terminates should not grow forever.
        if buffer.len() > 1 << 20 {
            buffer.clear();
        }
    }
}

/// Split every complete line out of `buffer`, leaving any trailing partial
/// line in place for the next chunk. Blank lines are dropped.
fn drain_lines(buffer: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
        let raw: Vec<u8> = buffer.drain(..=newline).collect();
        let text = String::from_utf8_lossy(&raw[..raw.len() - 1]);
        let text = text.trim();
        if !text.is_empty() {
            lines.push(text.to_string());
        }
    }
    lines
}

/// Run one user action against the API and report the outcome.
fn spawn_command(command: Command, client: Arc<Client>, tx: mpsc::UnboundedSender<Event>) {
    tokio::spawn(async move {
        let result = execute(command, &client, &tx).await;
        if let Err(err) = result {
            let _ = tx.send(Event::Toast(ToastKind::Error, err.to_string()));
        }
    });
}

async fn execute(
    command: Command,
    client: &Client,
    tx: &mpsc::UnboundedSender<Event>,
) -> Result<()> {
    match command {
        Command::SetMode(mode) => {
            client.set_mode(&mode).await?;
            let _ = tx.send(Event::Toast(ToastKind::Success, format!("mode → {mode}")));
        }
        Command::Select { group, member } => {
            client.select(&group, &member).await?;
            let _ = tx.send(Event::Toast(
                ToastKind::Success,
                format!("{group} → {member}"),
            ));
        }
        Command::TestMember {
            name,
            url,
            timeout_ms,
        } => {
            let result = client.proxy_delay(&name, &url, timeout_ms).await;
            let _ = tx.send(Event::MemberDelay {
                name,
                result: result.map_err(|err| short_reason(&err)),
            });
        }
        Command::TestGroup {
            name,
            url,
            timeout_ms,
        } => {
            let delays = client.group_delay(&name, &url, timeout_ms).await?;
            let _ = tx.send(Event::GroupDelay(delays));
        }
        Command::CloseConnection(id) => {
            client.close_connection(&id).await?;
            let _ = tx.send(Event::Toast(ToastKind::Success, "connection closed".into()));
        }
        Command::CloseAllConnections => {
            client.close_all_connections().await?;
            let _ = tx.send(Event::Toast(
                ToastKind::Success,
                "all connections closed".into(),
            ));
        }
        Command::Refresh => {}
        // The profile worker takes these over in the next task. Until it
        // exists they must still compile and must not pretend to have
        // changed anything.
        Command::SwitchProfile(_)
        | Command::ConfirmSwitch { .. }
        | Command::AddProfile { .. }
        | Command::UpdateProfile(_)
        | Command::DeleteProfile(_) => {
            let _ = tx.send(Event::Toast(
                ToastKind::Info,
                "subscription actions are not wired up yet".into(),
            ));
        }
    }

    // Every action changes state, so pull a fresh snapshot right away rather
    // than waiting for the next scheduled refresh.
    let (configs, groups, connections) =
        tokio::join!(client.configs(), client.groups(), client.connections());
    let snapshot = Snapshot {
        version: None,
        configs: configs.ok(),
        groups: groups.unwrap_or_default(),
        connections: connections.unwrap_or_default(),
    };
    let _ = tx.send(Event::Snapshot(Box::new(snapshot)));

    Ok(())
}

/// anyhow chains read poorly in a one-line toast; keep just the root cause.
fn short_reason(err: &anyhow::Error) -> String {
    err.chain()
        .last()
        .map(|cause| cause.to_string())
        .unwrap_or_else(|| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Options>> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_point_at_a_local_controller() {
        let options = parse(&[]).unwrap().unwrap();
        assert_eq!(options.endpoint, "http://127.0.0.1:9090");
        assert_eq!(options.secret, None);
        assert_eq!(options.timeout_ms, 5000);
    }

    #[test]
    fn flags_accept_both_separated_and_inline_values() {
        let separated = parse(&["--endpoint", "http://10.0.0.1:9090", "--secret", "hunter2"])
            .unwrap()
            .unwrap();
        assert_eq!(separated.endpoint, "http://10.0.0.1:9090");
        assert_eq!(separated.secret.as_deref(), Some("hunter2"));

        let inline = parse(&["--endpoint=http://10.0.0.1:9090", "--timeout=1500"])
            .unwrap()
            .unwrap();
        assert_eq!(inline.endpoint, "http://10.0.0.1:9090");
        assert_eq!(inline.timeout_ms, 1500);
    }

    #[test]
    fn parse_every_supported_flag() {
        let options = parse(&[
            "--endpoint",
            "http://x:1",
            "--secret",
            "s",
            "--delay-url",
            "http://d/204",
            "--timeout",
            "1234",
            "--refresh",
            "500",
            "--log-level",
            "debug",
        ])
        .unwrap()
        .unwrap();

        assert_eq!(options.delay_url, "http://d/204");
        assert_eq!(options.timeout_ms, 1234);
        assert_eq!(options.refresh_ms, 500);
        assert_eq!(options.log_level, "debug");
    }

    #[test]
    fn help_is_reported_as_no_work_to_do() {
        assert!(parse(&["--help"]).unwrap().is_none());
        assert!(parse(&["-h"]).unwrap().is_none());
    }

    #[test]
    fn bad_input_is_an_error_not_a_panic() {
        assert!(parse(&["--nope"]).is_err(), "unknown flag");
        assert!(parse(&["--endpoint"]).is_err(), "missing value");
        assert!(
            parse(&["--timeout", "soon"]).is_err(),
            "non-numeric timeout"
        );
        assert!(parse(&["--refresh", "x"]).is_err(), "non-numeric refresh");
    }

    #[test]
    fn parses_config_dir_and_concurrency() {
        let options = parse(&["--config-dir", "/x", "--concurrency", "16"])
            .unwrap()
            .unwrap();
        assert_eq!(options.config_dir.as_deref(), Some("/x"));
        assert_eq!(options.concurrency, Some(16));

        let inline = parse(&["--concurrency=4", "--config-dir=/y"])
            .unwrap()
            .unwrap();
        assert_eq!(inline.concurrency, Some(4));
        assert_eq!(inline.config_dir.as_deref(), Some("/y"));

        assert!(
            parse(&["--concurrency", "abc"]).is_err(),
            "non-numeric concurrency"
        );
        assert!(
            parse(&["--concurrency", "0"]).is_err(),
            "concurrency must be at least 1"
        );
    }

    #[test]
    fn the_endpoint_is_tolerated_without_a_scheme() {
        let options = parse(&["--endpoint", "127.0.0.1:9090"]).unwrap().unwrap();
        let client = Client::new(&options.endpoint, None).unwrap();
        assert_eq!(client.endpoint(), "http://127.0.0.1:9090/");
    }

    #[test]
    fn drain_lines_splits_a_complete_line() {
        let mut buffer = b"{\"up\":1}\n".to_vec();
        assert_eq!(drain_lines(&mut buffer), vec!["{\"up\":1}"]);
        assert!(buffer.is_empty(), "the consumed bytes are gone");
    }

    #[test]
    fn drain_lines_keeps_a_partial_line_for_the_next_chunk() {
        // This is the case that matters: a JSON object split across two reads.
        let mut buffer = b"{\"up\":1,\"dow".to_vec();
        assert!(drain_lines(&mut buffer).is_empty(), "nothing complete yet");
        assert_eq!(buffer, b"{\"up\":1,\"dow", "the partial line is preserved");

        buffer.extend_from_slice(b"n\":2}\n");
        assert_eq!(drain_lines(&mut buffer), vec!["{\"up\":1,\"down\":2}"]);
    }

    #[test]
    fn drain_lines_returns_several_lines_from_one_chunk() {
        let mut buffer = b"a\nb\nc\n".to_vec();
        assert_eq!(drain_lines(&mut buffer), vec!["a", "b", "c"]);
    }

    #[test]
    fn drain_lines_skips_blank_lines_and_trims_carriage_returns() {
        let mut buffer = b"\n  \n{\"up\":1}\r\n".to_vec();
        assert_eq!(drain_lines(&mut buffer), vec!["{\"up\":1}"]);
    }

    #[test]
    fn drain_lines_passes_through_a_line_that_is_not_json() {
        // Parsing is the caller's business; the splitter must not drop data.
        let mut buffer = b"not json at all\n".to_vec();
        assert_eq!(drain_lines(&mut buffer), vec!["not json at all"]);
    }

    #[test]
    fn drain_lines_on_an_empty_buffer_is_harmless() {
        let mut buffer = Vec::new();
        assert!(drain_lines(&mut buffer).is_empty());
    }
}
