use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cellshot::{driver, recording, render, session, shot as shot_engine};
use clap::{Args, Parser, Subcommand, ValueEnum};

const HELP: &str = "\
cellshot exports terminal shots for TUI inspection and agent workflows. A shot can observe a
command, piped command output, an ANSI/VT byte stream, or a running session, then write selected
visual and inspectable formats from the visible terminal frame.";

const ROOT_EXAMPLES: &str = "\
Examples:
  cellshot shot --out captures/app -- my-terminal-app
  cellshot shot --cols 100 --rows 32 --wait-for 'Commands' -s ctrl-p --out captures/menu -- my-terminal-app
  cellshot shot --pipe --format png --format txt --out captures/log -- my-log-command
  cellshot session start demo --host opentui -- opencode
  cellshot session status demo --json
  cellshot session wait demo '/connect' && cellshot session send demo text:/connect enter
  cellshot shot --session demo --format png --out captures/provider
  cellshot session stop demo
  printf '\\033[32msuccess\\033[0m\\n' | cellshot shot --input - --out captures/stdin

Use `shot` for one exported frame or `session` for multi-step terminal workflows.";

const SHOT_HELP: &str = "\
Shot sources:
  cellshot shot -- COMMAND...           Run a command in a PTY.
  cellshot shot --pipe -- COMMAND...    Render piped stdout/stderr; color defaults to always.
  cellshot shot --input FILE            Render ANSI/VT bytes from FILE, or use - for stdin.
  cellshot shot --session NAME          Export the current settled frame of a live session.

Command flow:
  1. Start COMMAND inside a PTY, or use --pipe for commands that need ordinary stdout/stderr.
  2. Optionally wait for --initial-delay-ms and visible --wait-for text.
  3. If --send input is queued, send the ordered events as one input burst.
  4. Freeze the visible frame once output is idle for --settle-ms or --deadline-ms expires.

Without --format, shots write PNG, SVG, text, JSON, and ANSI stream artifacts. Repeat --format
to select exactly the needed outputs, such as `--format png --format txt`. Use `--stdout` with
one text-based format, or with no format for visible text, when an agent needs direct inspection.

Use --wait-for whenever an interaction must occur only after a UI is mounted. If its text is not
visible before the command exits or deadline expires, the shot fails rather than exporting the
wrong screen. Send keys by name and text as `text:<value>`, for example `-s ctrl-p text:model
enter`. For multiple interaction steps on one live application, use `session start`, `session
wait`, `session send`, `shot --session`, and `session stop`.

Use `--host opentui` only for OpenTUI applications, including OpenCode, that query terminal
capabilities before painting their interface. Generic programs do not need a host profile.
Use `--pipe` for CLIs that skip output when stdout is a TTY; it captures stdout and stderr without
terminal input and forces color unless overridden with `--color`.

Examples:
  cellshot shot --host opentui --cols 100 --rows 32 --out captures/home -- opencode
  cellshot shot --host opentui --wait-for '/connect' -s text:/connect enter --out captures/provider -- opencode
  cellshot shot --pipe --format png --format txt --out captures/log -- my-command
  cellshot shot --input debug.ansi --format png --out captures/replay
  cellshot shot --session demo --stdout
  cellshot shot --session demo --format png --out captures/current";

const SESSION_HELP: &str = "\
Sessions keep one terminal application alive across interactions and shots. Start a session,
inspect its status, wait for visible state, send input, resize when needed, export shots with
`cellshot shot --session NAME`, inspect normal-screen scrollback with `session history`, then stop
the session when finished.";

const START_HELP: &str = "\
Start creates one background PTY session and returns once its local control socket is available.
The application stays alive until `cellshot session stop NAME`, so later commands interact with the
same screen and application state. Persistent sessions currently require macOS or Linux. Session
sockets are local control endpoints protected for the current user; recordings contain terminal
output plus client and automatic host input, so treat them as sensitive artifacts.

Example:
  cellshot session start demo --host opentui --cols 112 --rows 34 -- opencode
  cellshot session status demo --json
  cellshot session wait demo '/connect'
  cellshot session send demo text:/connect enter
  cellshot session resize demo --cols 132 --rows 38
  cellshot session wait demo 'Connect a provider'
  cellshot shot --session demo --out captures/provider
  cellshot session stop demo";

const SEND_HELP: &str = "\
Send ordered input to a live session. Text uses `text:<value>`; named keys include `enter`,
`escape`, arrows, `tab`, `shift-tab`, `backspace`, `delete`, `home`, `end`, `page-up`, and
`page-down`. Use `ctrl-a` through `ctrl-z` for control input such as `ctrl-c` cancellation.
Add `--pace-ms 35` when producing a human-readable recording so typed text appears character by
character in the terminal instead of as one immediate paste. Use `--stdin` to send exact bytes
from standard input as one burst.

Examples:
  cellshot session send demo ctrl-p text:model enter
  cellshot session send demo ctrl-c
  printf '%s' 'a multiline prompt' | cellshot session send demo --stdin
  cellshot session send demo --pace-ms 35 'text:Write a terminal haiku.' enter";

const VIDEO_HELP: &str = "\
Replay a recording produced by `session start --record` into a video artifact. Output is sampled at --fps and
begins at the first visible terminal content while preserving real timing afterward. Pass
--include-startup to include blank startup/negotiation frames or --max-idle-ms when you explicitly
want to shorten long quiet gaps for a condensed edit. The source `.cellshot` file retains observed
timing, terminal bytes, client input, and automatic host input until the session is closed.
Video export requires `ffmpeg` to be installed.

Example:
  cellshot session start demo --record captures/demo.cellshot -- opencode
  cellshot session send demo text:/connect enter
  cellshot session stop demo
  cellshot video captures/demo.cellshot --out captures/demo.mp4";

const DRIVER_HELP: &str = "\
Driver mode serves isolated embedded sessions as newline-delimited JSON over standard input and
standard output. It is intended for typed clients such as a future `@cellshot/test` package;
standard output contains protocol messages only.

Example:
  cellshot driver";

#[derive(Parser)]
#[command(
    name = "cellshot",
    version,
    about = "Export terminal shots and recorded sessions",
    long_about = HELP,
    after_help = ROOT_EXAMPLES
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Export one terminal frame from a command, stream, or live session.
    #[command(after_help = SHOT_HELP)]
    Shot(ShotArgs),
    /// Start and control a persistent terminal application.
    #[command(after_help = SESSION_HELP)]
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Export a video from a recorded persistent session.
    #[command(after_help = VIDEO_HELP)]
    Video(VideoArgs),
    /// Serve isolated sessions for external testing clients.
    #[command(after_help = DRIVER_HELP)]
    Driver,
    #[command(name = "__serve", hide = true)]
    Serve(ServeArgs),
}

#[derive(Args)]
struct RenderArgs {
    /// Cell width used for terminal geometry and rendering.
    #[arg(long, default_value_t = 9)]
    cell_width: u16,
    /// Cell height used for terminal geometry and rendering.
    #[arg(long, default_value_t = 18)]
    cell_height: u16,
    /// Outer padding around the rendered terminal in pixels.
    #[arg(long, default_value_t = 18.0)]
    padding: f32,
    /// Font family used in SVG/PNG output.
    #[arg(
        long,
        default_value = "JetBrains Mono, SFMono-Regular, Menlo, monospace"
    )]
    font_family: String,
    /// Scale PNG output for sharp HiDPI viewing; SVG output is unchanged.
    #[arg(long, default_value_t = 2.0)]
    pixel_ratio: f32,
    /// Output path stem; extensions are added automatically.
    #[arg(short, long, default_value = "shot")]
    out: PathBuf,
    /// Hide the terminal cursor in rendered output.
    #[arg(long)]
    hide_cursor: bool,
    /// Output artifact format; repeat to select multiple formats (default: all).
    #[arg(long = "format", value_enum)]
    formats: Vec<ShotFormat>,
    /// Write one text-based format to stdout instead of artifact files (default: txt).
    #[arg(long)]
    stdout: bool,
}

#[derive(Args)]
struct ShotArgs {
    #[command(flatten)]
    render: RenderArgs,
    /// Terminal width in cells for command or ANSI input shots (default: 80).
    #[arg(long)]
    cols: Option<u16>,
    /// Terminal height in cells for command or ANSI input shots (default: 24).
    #[arg(long)]
    rows: Option<u16>,
    /// Observe command stdout/stderr as pipes instead of launching it in a PTY.
    #[arg(long)]
    pipe: bool,
    /// Render ANSI/VT bytes from this file; use `-` for stdin.
    #[arg(long, value_name = "FILE")]
    input: Option<PathBuf>,
    /// Export the current settled frame of this running session.
    #[arg(long, value_name = "NAME")]
    session: Option<String>,
    /// Color environment policy for a command source (default: auto for PTY, always for pipe).
    #[arg(long, value_enum)]
    color: Option<ColorMode>,
    /// Capture after this many milliseconds without output (default: 250).
    #[arg(long)]
    settle_ms: Option<u64>,
    /// Capture or return after this deadline even if output continues (default: 5000).
    #[arg(long)]
    deadline_ms: Option<u64>,
    /// Wait this long before allowing the initial screen to settle.
    #[arg(long)]
    initial_delay_ms: Option<u64>,
    /// Wait until the visible terminal includes this text before interacting or capturing.
    #[arg(long)]
    wait_for: Option<String>,
    /// Fail if command or ANSI input exceeds this many terminal bytes (default: 16777216).
    #[arg(long)]
    max_bytes: Option<usize>,
    /// Working directory for the terminal command.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Terminal-host compatibility response profile.
    #[arg(long, value_enum)]
    host: Option<HostProfile>,
    /// Ordered input after readiness: key name or `text:<value>` (repeatable/groupable).
    #[arg(short = 's', long, value_name = "INPUT", num_args = 1..)]
    send: Vec<String>,
    /// Command and arguments to launch, following `--`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct StartArgs {
    /// Stable local name used by later session commands.
    name: String,
    /// Terminal width in cells.
    #[arg(long, default_value_t = 80)]
    cols: u16,
    /// Terminal height in cells.
    #[arg(long, default_value_t = 24)]
    rows: u16,
    /// Terminal cell width in pixels.
    #[arg(long, default_value_t = 9)]
    cell_width: u16,
    /// Terminal cell height in pixels.
    #[arg(long, default_value_t = 18)]
    cell_height: u16,
    /// Maximum raw terminal bytes retained by the live session.
    #[arg(long, default_value_t = 16 * 1024 * 1024)]
    max_bytes: usize,
    /// Working directory for the terminal command.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Write timestamped terminal output and client/host input to this private recording file.
    #[arg(long)]
    record: Option<PathBuf>,
    /// Color environment policy for the terminal command.
    #[arg(long, value_enum, default_value = "auto")]
    color: ColorMode,
    /// Terminal-host compatibility response profile.
    #[arg(long, value_enum)]
    host: Option<HostProfile>,
    /// Command and arguments to launch, following `--`.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Subcommand)]
enum SessionCommand {
    /// Start a named persistent terminal session.
    #[command(after_help = START_HELP)]
    Start(StartArgs),
    /// Wait until a named session includes visible text.
    Wait(WaitArgs),
    /// Send ordered input to a named session.
    #[command(after_help = SEND_HELP)]
    Send(SendArgs),
    /// Inspect whether a named session is running or exited.
    Status(StatusArgs),
    /// List named local sessions and their states.
    List(ListArgs),
    /// Resize a named live session.
    Resize(ResizeArgs),
    /// Print retained terminal scrollback or exact ANSI/VT stream bytes.
    History(HistoryArgs),
    /// Stop an existing named session if present, then start its replacement.
    #[command(after_help = START_HELP)]
    Restart(StartArgs),
    /// Terminate a named session.
    Stop(SessionArgs),
}

#[derive(Args)]
struct WaitArgs {
    /// Name of a running session.
    name: String,
    /// Visible text that must appear in the session screen.
    text: String,
    /// Maximum time to wait before returning an error.
    #[arg(long, default_value_t = 5000)]
    timeout_ms: u64,
}

#[derive(Args)]
struct SendArgs {
    /// Name of a running session.
    name: String,
    /// Delay between input atoms; text is split into characters when set.
    #[arg(long, default_value_t = 0)]
    pace_ms: u64,
    /// Send bytes read from stdin as one burst; cannot be paced or combined with INPUT.
    #[arg(long, conflicts_with = "input")]
    stdin: bool,
    /// Ordered input: key name or `text:<value>`.
    #[arg(value_name = "INPUT")]
    input: Vec<String>,
}

#[derive(Args)]
struct StatusArgs {
    /// Name of a running or inspectable exited session.
    name: String,
    /// Write structured JSON status.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ListArgs {
    /// Write structured JSON entries, including stale sockets.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ResizeArgs {
    /// Name of a running session.
    name: String,
    /// New terminal width in cells.
    #[arg(long)]
    cols: u16,
    /// New terminal height in cells.
    #[arg(long)]
    rows: u16,
    /// New terminal cell width in pixels; defaults to current geometry.
    #[arg(long)]
    cell_width: Option<u16>,
    /// New terminal cell height in pixels; defaults to current geometry.
    #[arg(long)]
    cell_height: Option<u16>,
}

#[derive(Args)]
struct HistoryArgs {
    /// Name of a running or inspectable exited session.
    name: String,
    /// Write exact retained ANSI/VT stream bytes instead of readable normal-screen history.
    #[arg(long)]
    ansi: bool,
}

#[derive(Args)]
struct SessionArgs {
    /// Name of a running session.
    name: String,
}

#[derive(Args)]
struct ServeArgs {
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    record: Option<PathBuf>,
    #[arg(long)]
    opentui_host: bool,
    #[arg(long, value_enum, default_value = "auto")]
    color: ColorMode,
    #[arg(long)]
    cols: u16,
    #[arg(long)]
    rows: u16,
    #[arg(long)]
    cell_width: u16,
    #[arg(long)]
    cell_height: u16,
    #[arg(long)]
    max_bytes: usize,
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct VideoArgs {
    /// Recording created by `session start --record`.
    input: PathBuf,
    /// Override the recorded terminal cell width in rendered pixels.
    #[arg(long)]
    cell_width: Option<u16>,
    /// Override the recorded terminal cell height in rendered pixels.
    #[arg(long)]
    cell_height: Option<u16>,
    /// Outer padding around the rendered terminal in pixels.
    #[arg(long, default_value_t = 18.0)]
    padding: f32,
    /// Font family used in video output.
    #[arg(
        long,
        default_value = "JetBrains Mono, SFMono-Regular, Menlo, monospace"
    )]
    font_family: String,
    /// Scale video frames for sharp HiDPI viewing.
    #[arg(long, default_value_t = 2.0)]
    pixel_ratio: f32,
    /// Output video file path.
    #[arg(short, long, default_value = "video.mp4")]
    out: PathBuf,
    /// Hide the terminal cursor in rendered output.
    #[arg(long)]
    hide_cursor: bool,
    /// Maximum sampled frames per second (1 to 1000).
    #[arg(long, default_value_t = 20)]
    fps: u32,
    /// Optionally collapse longer gaps between changed screens to this duration.
    #[arg(long)]
    max_idle_ms: Option<u64>,
    /// Hold the final frame for this duration.
    #[arg(long, default_value_t = 1000)]
    tail_ms: u64,
    /// Include leading contentless startup/terminal negotiation frames.
    #[arg(long)]
    include_startup: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum HostProfile {
    /// Respond to OpenTUI startup terminal capability queries.
    Opentui,
}

#[derive(Clone, Copy, ValueEnum)]
enum ColorMode {
    /// Preserve the current process color environment.
    Auto,
    /// Remove NO_COLOR and set common force-color environment variables.
    Always,
    /// Set common no-color environment variables.
    Never,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ShotFormat {
    /// PNG image.
    Png,
    /// SVG image.
    Svg,
    /// Visible plain text.
    Txt,
    /// Structured terminal cells.
    Json,
    /// Original ANSI/VT terminal stream.
    Ansi,
}

impl From<ColorMode> for shot_engine::ColorMode {
    fn from(value: ColorMode) -> Self {
        match value {
            ColorMode::Auto => shot_engine::ColorMode::Auto,
            ColorMode::Always => shot_engine::ColorMode::Always,
            ColorMode::Never => shot_engine::ColorMode::Never,
        }
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Shot(args) => shot(args)?,
        Command::Session { command } => session_command(command)?,
        Command::Video(args) => {
            recording::video(
                &args.input,
                &recording::VideoOptions {
                    out: args.out,
                    cell_width: args.cell_width,
                    cell_height: args.cell_height,
                    padding: args.padding,
                    font_family: args.font_family,
                    pixel_ratio: args.pixel_ratio,
                    hide_cursor: args.hide_cursor,
                    fps: args.fps,
                    max_idle: args.max_idle_ms.map(Duration::from_millis),
                    tail: Duration::from_millis(args.tail_ms),
                    include_startup: args.include_startup,
                },
            )?;
        }
        Command::Driver => {
            driver::serve(BufReader::new(io::stdin().lock()), io::stdout().lock())?;
        }
        Command::Serve(args) => {
            session::serve(
                args.socket,
                args.command,
                args.cwd,
                args.record,
                shot_engine::Options {
                    cols: args.cols,
                    rows: args.rows,
                    cell_width: args.cell_width,
                    cell_height: args.cell_height,
                    settle: Duration::ZERO,
                    deadline: Duration::ZERO,
                    input: Vec::new(),
                    initial_delay: Duration::ZERO,
                    wait_for: None,
                    max_bytes: args.max_bytes,
                    opentui_host: args.opentui_host,
                    color: args.color.into(),
                },
            )?;
        }
    }
    Ok(())
}

fn shot(args: ShotArgs) -> Result<()> {
    if args.render.stdout {
        stdout_format(&args.render)?;
    }
    let defaults = shot_engine::Options::default();
    let settle =
        Duration::from_millis(args.settle_ms.unwrap_or(defaults.settle.as_millis() as u64));
    let deadline = Duration::from_millis(
        args.deadline_ms
            .unwrap_or(defaults.deadline.as_millis() as u64),
    );
    if args.input.is_some() && (args.pipe || args.session.is_some() || !args.command.is_empty()) {
        bail!("--input cannot be combined with --pipe, --session, or a command");
    }
    if args.session.is_some() && (args.pipe || !args.command.is_empty()) {
        bail!("--session cannot be combined with --pipe or a command");
    }
    if let Some(name) = args.session.as_deref() {
        if args.cols.is_some()
            || args.rows.is_some()
            || args.color.is_some()
            || args.initial_delay_ms.is_some()
            || args.wait_for.is_some()
            || args.max_bytes.is_some()
            || args.cwd.is_some()
            || args.host.is_some()
            || !args.send.is_empty()
        {
            bail!(
                "--session shots support rendering, formats, --settle-ms, and --deadline-ms only"
            );
        }
        let captured = session::shot(name, settle, deadline)?;
        return write_outputs(&captured, &args.render);
    }
    let cols = args.cols.unwrap_or(defaults.cols);
    let rows = args.rows.unwrap_or(defaults.rows);
    validate_terminal_size(cols, rows)?;
    let max_bytes = args.max_bytes.unwrap_or(defaults.max_bytes);
    if let Some(path) = args.input.as_ref() {
        if args.color.is_some()
            || args.settle_ms.is_some()
            || args.deadline_ms.is_some()
            || args.initial_delay_ms.is_some()
            || args.wait_for.is_some()
            || args.cwd.is_some()
            || args.host.is_some()
            || !args.send.is_empty()
        {
            bail!("--input shots support dimensions, rendering, formats, and --max-bytes only");
        }
        let mut input = Vec::new();
        let limit = max_bytes.saturating_add(1) as u64;
        if path.as_os_str() == "-" {
            io::stdin()
                .take(limit)
                .read_to_end(&mut input)
                .context("read ANSI input")?;
        } else {
            fs::File::open(path)
                .with_context(|| format!("open {}", path.display()))?
                .take(limit)
                .read_to_end(&mut input)
                .with_context(|| format!("read {}", path.display()))?;
        }
        let captured = shot_engine::from_ansi(input, rows, cols, max_bytes)?;
        return write_outputs(&captured, &args.render);
    }
    if args.command.is_empty() {
        bail!("provide a command after --, --input FILE, or --session NAME");
    }
    if args.pipe
        && (!args.send.is_empty()
            || args.host.is_some()
            || args.initial_delay_ms.is_some()
            || args.settle_ms.is_some())
    {
        bail!("--pipe shots do not support --send, --host, --initial-delay-ms, or --settle-ms");
    }
    let color = args.color.unwrap_or(if args.pipe {
        ColorMode::Always
    } else {
        ColorMode::Auto
    });
    let options = shot_engine::Options {
        cols,
        rows,
        cell_width: args.render.cell_width,
        cell_height: args.render.cell_height,
        settle,
        deadline,
        input: input_bytes(&args.send)?,
        initial_delay: Duration::from_millis(args.initial_delay_ms.unwrap_or(0)),
        wait_for: args.wait_for,
        max_bytes,
        opentui_host: matches!(args.host, Some(HostProfile::Opentui)),
        color: color.into(),
    };
    let captured = if args.pipe {
        shot_engine::from_pipe_command(&args.command, args.cwd.as_deref(), &options)
    } else {
        shot_engine::from_command(&args.command, args.cwd.as_deref(), &options)
    }?;
    write_outputs(&captured, &args.render)
}

fn session_command(command: SessionCommand) -> Result<()> {
    match command {
        SessionCommand::Start(args) => {
            start_session(&args, false)?;
            println!("{}", args.name);
        }
        SessionCommand::Wait(args) => {
            session::wait(
                &args.name,
                args.text,
                Duration::from_millis(args.timeout_ms),
            )?;
        }
        SessionCommand::Send(args) => {
            if args.stdin && args.pace_ms > 0 {
                bail!("--stdin cannot be combined with --pace-ms");
            }
            let input = if args.stdin {
                let mut bytes = Vec::new();
                io::stdin()
                    .take(1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .context("read session input")?;
                if bytes.len() > 1024 * 1024 {
                    bail!("session input exceeds 1 MiB");
                }
                vec![bytes]
            } else {
                if args.input.is_empty() {
                    bail!("provide INPUT events or --stdin");
                }
                session_input(&args.input, args.pace_ms > 0)?
            };
            session::send(&args.name, input, Duration::from_millis(args.pace_ms))?;
        }
        SessionCommand::Status(args) => {
            let status = session::status(&args.name)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!(
                    "{}\t{}\t{}x{}\t{}",
                    args.name,
                    session_state(status.state),
                    status.cols,
                    status.rows,
                    if status.recording { "recording" } else { "-" }
                );
            }
        }
        SessionCommand::List(args) => {
            let sessions = session::list()?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else {
                for entry in sessions {
                    if let Some(status) = entry.status {
                        println!(
                            "{}\t{}\t{}x{}\t{}",
                            entry.name,
                            session_state(status.state),
                            status.cols,
                            status.rows,
                            if status.recording { "recording" } else { "-" }
                        );
                    } else {
                        println!("{}\tstale\t-\t-", entry.name);
                    }
                }
            }
        }
        SessionCommand::Resize(args) => {
            validate_terminal_size(args.cols, args.rows)?;
            session::resize(
                &args.name,
                args.cols,
                args.rows,
                args.cell_width,
                args.cell_height,
            )?;
        }
        SessionCommand::History(args) => {
            let bytes = session::history(&args.name, args.ansi)?;
            io::stdout()
                .write_all(&bytes)
                .context("write session history")?;
            if !args.ansi && !bytes.ends_with(b"\n") {
                io::stdout()
                    .write_all(b"\n")
                    .context("write session history newline")?;
            }
        }
        SessionCommand::Restart(args) => {
            start_session(&args, true)?;
            println!("{}", args.name);
        }
        SessionCommand::Stop(args) => session::stop(&args.name)?,
    }
    Ok(())
}

fn start_session(args: &StartArgs, restart: bool) -> Result<()> {
    validate_terminal_size(args.cols, args.rows)?;
    let options = shot_engine::Options {
        cols: args.cols,
        rows: args.rows,
        cell_width: args.cell_width,
        cell_height: args.cell_height,
        settle: Duration::ZERO,
        deadline: Duration::ZERO,
        input: Vec::new(),
        initial_delay: Duration::ZERO,
        wait_for: None,
        max_bytes: args.max_bytes,
        opentui_host: matches!(args.host, Some(HostProfile::Opentui)),
        color: args.color.into(),
    };
    if restart {
        session::restart(
            &args.name,
            &args.command,
            args.cwd.as_deref(),
            args.record.as_deref(),
            &options,
        )
    } else {
        session::start(
            &args.name,
            &args.command,
            args.cwd.as_deref(),
            args.record.as_deref(),
            &options,
        )
    }
}

fn session_state(state: session::SessionState) -> &'static str {
    match state {
        session::SessionState::Running => "running",
        session::SessionState::Exited => "exited",
    }
}

fn input_bytes(events: &[String]) -> Result<Vec<u8>> {
    let mut input = Vec::new();
    for event in events {
        input.extend(input_event(event)?);
    }
    Ok(input)
}

fn input_event(event: &str) -> Result<Vec<u8>> {
    if let Some(text) = event.strip_prefix("text:") {
        return Ok(text.as_bytes().to_vec());
    }
    if let Some(key) = event
        .strip_prefix("ctrl-")
        .or_else(|| event.strip_prefix("ctrl:"))
        && key.len() == 1
    {
        let key = key.as_bytes()[0].to_ascii_lowercase();
        if key.is_ascii_lowercase() {
            return Ok(vec![key - b'a' + 1]);
        }
    }
    Ok(match event {
        "enter" => b"\r".to_vec(),
        "escape" | "esc" => b"\x1b".to_vec(),
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "left" => b"\x1b[D".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "tab" => b"\t".to_vec(),
        "shift-tab" => b"\x1b[Z".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "delete" => b"\x1b[3~".to_vec(),
        "home" => b"\x1b[H".to_vec(),
        "end" => b"\x1b[F".to_vec(),
        "page-up" => b"\x1b[5~".to_vec(),
        "page-down" => b"\x1b[6~".to_vec(),
        _ => anyhow::bail!(
            "unsupported input event {event:?}; use text:<value>, ctrl-a through ctrl-z, enter, escape, arrows, tab, shift-tab, backspace, delete, home, end, page-up, or page-down"
        ),
    })
}

fn session_input(events: &[String], paced: bool) -> Result<Vec<Vec<u8>>> {
    if !paced {
        return Ok(vec![input_bytes(events)?]);
    }
    let mut input = Vec::new();
    for event in events {
        if let Some(text) = event.strip_prefix("text:") {
            input.extend(text.chars().map(|char| char.to_string().into_bytes()));
            continue;
        }
        input.push(input_event(event)?);
    }
    Ok(input)
}

fn validate_terminal_size(cols: u16, rows: u16) -> Result<()> {
    if cols == 0 || rows == 0 {
        bail!("terminal dimensions must be greater than zero");
    }
    Ok(())
}

fn write_outputs(captured: &shot_engine::Shot, args: &RenderArgs) -> Result<()> {
    if args.stdout {
        return write_stdout(captured, args);
    }
    if let Some(parent) = args
        .out
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let enabled = |format| args.formats.is_empty() || args.formats.contains(&format);
    let svg = (enabled(ShotFormat::Svg) || enabled(ShotFormat::Png))
        .then(|| rendered_svg(captured, args));
    if let Some(svg) = svg.as_ref().filter(|_| enabled(ShotFormat::Svg)) {
        let path = args.out.with_extension("svg");
        fs::write(&path, svg).with_context(|| format!("write {}", path.display()))?;
        println!("{}", path.display());
    }
    if let Some(svg) = svg.as_ref().filter(|_| enabled(ShotFormat::Png)) {
        let path = args.out.with_extension("png");
        render::png(svg, &path, args.pixel_ratio)?;
        println!("{}", path.display());
    }
    if enabled(ShotFormat::Json) {
        let path = args.out.with_extension("json");
        fs::write(&path, serde_json::to_vec_pretty(&captured.frame)?)
            .with_context(|| format!("write {}", path.display()))?;
        println!("{}", path.display());
    }
    if enabled(ShotFormat::Txt) {
        let path = args.out.with_extension("txt");
        fs::write(&path, captured.frame.text())
            .with_context(|| format!("write {}", path.display()))?;
        println!("{}", path.display());
    }
    if enabled(ShotFormat::Ansi) {
        let path = args.out.with_extension("ansi");
        fs::write(&path, &captured.ansi).with_context(|| format!("write {}", path.display()))?;
        println!("{}", path.display());
    }
    Ok(())
}

fn write_stdout(captured: &shot_engine::Shot, args: &RenderArgs) -> Result<()> {
    let format = stdout_format(args)?;
    let bytes = match format {
        ShotFormat::Txt => captured.frame.text().into_bytes(),
        ShotFormat::Json => serde_json::to_vec_pretty(&captured.frame)?,
        ShotFormat::Ansi => captured.ansi.clone(),
        ShotFormat::Svg => rendered_svg(captured, args).into_bytes(),
        ShotFormat::Png => unreachable!("stdout format validated before observation"),
    };
    io::stdout()
        .write_all(&bytes)
        .context("write shot output")?;
    if format != ShotFormat::Ansi && !bytes.ends_with(b"\n") {
        io::stdout()
            .write_all(b"\n")
            .context("write shot output newline")?;
    }
    Ok(())
}

fn rendered_svg(captured: &shot_engine::Shot, args: &RenderArgs) -> String {
    render::svg(
        &captured.frame,
        &render::Options {
            cell_width: f32::from(args.cell_width),
            cell_height: f32::from(args.cell_height),
            font_size: f32::from(args.cell_height) * 0.78,
            padding: args.padding,
            font_family: args.font_family.clone(),
            show_cursor: !args.hide_cursor,
        },
    )
}

fn stdout_format(args: &RenderArgs) -> Result<ShotFormat> {
    let format = match args.formats.as_slice() {
        [] => ShotFormat::Txt,
        [format] => *format,
        _ => bail!("--stdout supports exactly one --format"),
    };
    if format == ShotFormat::Png {
        bail!("--stdout does not support PNG output");
    }
    Ok(format)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_ordered_input_events() {
        assert_eq!(
            input_bytes(&[
                "ctrl-p".to_owned(),
                "text:model".to_owned(),
                "enter".to_owned()
            ])
            .unwrap(),
            b"\x10model\r"
        );
    }

    #[test]
    fn rejects_unsupported_input_events() {
        assert!(input_bytes(&["space".to_owned()]).is_err());
    }

    #[test]
    fn encodes_control_and_navigation_input_events() {
        assert_eq!(
            input_bytes(&[
                "ctrl-c".to_owned(),
                "shift-tab".to_owned(),
                "delete".to_owned()
            ])
            .unwrap(),
            b"\x03\x1b[Z\x1b[3~"
        );
    }

    #[test]
    fn parses_compact_ordered_input_sequence() {
        let cli = Cli::try_parse_from([
            "cellshot",
            "shot",
            "--wait-for",
            "ready",
            "-s",
            "ctrl-p",
            "text:model",
            "enter",
            "--",
            "app",
        ])
        .unwrap();
        let Command::Shot(args) = cli.command else {
            panic!("expected shot command");
        };
        assert_eq!(args.send, ["ctrl-p", "text:model", "enter"]);
    }

    #[test]
    fn parses_repeated_shot_formats_and_session_source() {
        let cli = Cli::try_parse_from([
            "cellshot",
            "shot",
            "--session",
            "demo",
            "--format",
            "png",
            "--format",
            "txt",
        ])
        .unwrap();
        let Command::Shot(args) = cli.command else {
            panic!("expected shot command");
        };
        assert_eq!(args.session.as_deref(), Some("demo"));
        assert_eq!(args.render.formats, [ShotFormat::Png, ShotFormat::Txt]);
    }

    #[test]
    fn parses_session_status_resize_and_stdin_send() {
        assert!(Cli::try_parse_from(["cellshot", "session", "status", "demo", "--json"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "cellshot", "session", "resize", "demo", "--cols", "120", "--rows", "40"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["cellshot", "session", "send", "demo", "--stdin"]).is_ok());
        assert!(Cli::try_parse_from(["cellshot", "session", "history", "demo", "--ansi"]).is_ok());
        assert!(
            Cli::try_parse_from(["cellshot", "session", "restart", "demo", "--", "app"]).is_ok()
        );
    }

    #[test]
    fn validates_stdout_format_before_starting_a_source() {
        let cli = Cli::try_parse_from([
            "cellshot", "shot", "--stdout", "--format", "png", "--", "app",
        ])
        .unwrap();
        let Command::Shot(args) = cli.command else {
            panic!("expected shot command");
        };

        assert_eq!(
            shot(args).unwrap_err().to_string(),
            "--stdout does not support PNG output"
        );
    }

    #[test]
    fn rejects_settling_options_for_pipe_shots() {
        let cli = Cli::try_parse_from([
            "cellshot",
            "shot",
            "--pipe",
            "--settle-ms",
            "100",
            "--",
            "true",
        ])
        .unwrap();
        let Command::Shot(args) = cli.command else {
            panic!("expected shot command");
        };

        assert!(shot(args).is_err());
    }

    #[test]
    fn rejects_zero_terminal_dimensions() {
        assert!(validate_terminal_size(0, 24).is_err());
        assert!(validate_terminal_size(80, 0).is_err());
    }

    #[test]
    fn paced_session_input_splits_text_without_splitting_keys() {
        assert_eq!(
            session_input(&["text:hi".to_owned(), "enter".to_owned()], true).unwrap(),
            vec![b"h".to_vec(), b"i".to_vec(), b"\r".to_vec()]
        );
    }
}
