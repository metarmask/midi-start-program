#![feature(exit_status_error)]
#![feature(unix_send_signal)]

use clap::Parser;
use keepawake::KeepAwake;
use midi_msg::ChannelVoiceMsg::NoteOn;
use midi_msg::MidiMsg::ChannelVoice;
use midi_msg::{MidiMsg, ReceiverContext};
use midir::{MidiInput, MidiInputConnection, MidiOutput, SendError};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::flag;
use std::io;
use std::os::unix::process::ChildExt;
use std::process::{Child, Command, ExitStatusError};
use std::result::Result as StdResult;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Instant;
use std::{thread::sleep, time::Duration};
use thiserror::Error;

use crate::midi_out::MidiOut;
use crate::volume::Volume;

mod midi_out;
mod volume;

#[derive(Parser, Debug)]
#[command(
    name = "midi-start-program",
    version,
    about = "Start Pianoteq on MIDI activity"
)]
struct Args {
    #[arg(
        long,
        default_value = "Digital Piano",
        help = "Prefix of the MIDI port name to connect to"
    )]
    device_prefix: String,
    #[arg(long, help = "Path to the Pianoteq executable")]
    program: String,
    #[arg(
        long,
        default_value_t = 180,
        help = "Idle seconds before killing Pianoteq"
    )]
    idle: u64,
}

#[derive(Error, Debug)]
pub enum MidiStartPrgError {
    #[error("somethingy io-y {0}")]
    IoError(#[from] io::Error),
    #[error("midi parsing error {0}")]
    MidiParse(#[from] midi_msg::ParseError),
    #[error("midi init error {0}")]
    MidiInit(#[from] midir::InitError),
    #[error("midi init error while getting port info {0}")]
    MidiInitPort(#[from] midir::PortInfoError),
    #[error("midi connection error while getting port info {0}")]
    MidiConnect(#[from] midir::ConnectError<MidiInput>),
    #[error("midi output connection error {0}")]
    MidiOutputConnect(#[from] midir::ConnectError<MidiOutput>),
    #[error("process exited unsucessfully {0}")]
    ExitStatusError(#[from] ExitStatusError),
    #[error("tried to keep computer awake {0}")]
    KeepAwake(#[from] keepawake::Error),
    #[error("thread error {0}")]
    ThreadError(#[from] std::sync::mpsc::RecvError),
    #[error("parsing volume {0}")]
    ParsingVolume(String),
    #[error("midi send error {0}")]
    MidiSend(#[from] SendError),
    #[error("No matching MIDI port")]
    NoMatchingMidiPort,
}

type Result<T, E = MidiStartPrgError> = StdResult<T, E>;

fn pause_media() -> Result<(), MidiStartPrgError> {
    let output = Command::new("playerctl").args(["pause"]).output()?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("No players found") {
        return Ok(());
    }

    if !stderr.trim().is_empty() {
        eprintln!("playerctl pause failed: {}", stderr.trim());
    }

    output.status.exit_ok()?;
    Ok(())
}

fn systemctl_suspend() -> Result<(), MidiStartPrgError> {
    // TODO: This is to make sure messages reach the piano before, maybe make it less arbitrary
    sleep(Duration::from_millis(500));
    let status = Command::new("systemctl")
        .args(["suspend", "--check-inhibitors=no"])
        .status()?;
    if !status.success() {
        println!("systemctl suspend failed (ignored)");
    }
    Ok(())
}

struct PianoteqProcess {
    child: Child,
    has_exited: bool,
    _keep_awake: KeepAwake,
}

impl PianoteqProcess {
    fn new(executable: &str) -> Result<Self, MidiStartPrgError> {
        let child = Command::new(executable).spawn()?;
        Ok(PianoteqProcess {
            child,
            has_exited: false,
            _keep_awake: keepawake::Builder::default()
                .sleep(true)
                .reason("Playing piano")
                .app_name("Auto-Pianoteq")
                .app_reverse_domain("pianoteq.auto")
                .create()?,
        })
    }

    fn terminate(&mut self) -> Result<(), MidiStartPrgError> {
        self.child.send_signal(15).map_err(Into::into)
    }

    fn check_has_exited(&mut self) -> Result<bool, MidiStartPrgError> {
        if self.has_exited {
            return Ok(true);
        }
        if self.child.try_wait()?.is_some() {
            self.has_exited = true;
        }
        Ok(self.has_exited)
    }
}

impl Drop for PianoteqProcess {
    fn drop(&mut self) {
        if !self.has_exited {
            let result = self.child.kill();
            if !std::thread::panicking() {
                result.unwrap()
            } else if let Err(err) = result {
                eprintln!(
                    "killing in Drop for PianoteqProcess failed during panic: {:?}",
                    err
                )
            }
        }
    }
}

struct App {
    args: Args,
    shutdown: Arc<AtomicBool>,
    process: Option<PianoteqProcess>,
    last_note_instant: Instant,
    last_msg_instant: Instant,
    rightmost_count: u8,
    midi: Option<MidiConnection>,
    volume: Volume,
    idle_timeout: Duration,
}

struct MidiConnection {
    midi_out: Option<MidiOut>,
    /// Closes connection when dropped
    _midi_in: MidiInputConnection<()>,
    midi_in_rx: Receiver<Result<MidiMsg, midi_msg::ParseError>>,
}

impl App {
    fn tick_midi(&mut self, msg: MidiMsg) -> Result<(), MidiStartPrgError> {
        const RIGHTMOST_C: u8 = 108;
        const TRIGGER_NOTE: u8 = RIGHTMOST_C - 2;

        self.last_msg_instant = Instant::now();
        #[rustfmt::skip]
        let ChannelVoice { msg: NoteOn { note, velocity, .. }, .. } = msg else {
            return Ok(());
        };
        if velocity > 0 {
            if note == TRIGGER_NOTE && self.rightmost_count >= 7 {
                return self.suspend();
            } else if note == RIGHTMOST_C {
                self.rightmost_count = self.rightmost_count.saturating_add(1);
            } else {
                self.rightmost_count = 0;
            }
        }
        self.last_note_instant = Instant::now();
        self.start_process()
    }

    fn start_chime(&mut self) -> Result<(), MidiStartPrgError> {
        const START_NOTE: u8 = 60; // middle C
        const START_VELOCITY: u8 = 80;
        if let Some(Some(midi_out)) = self.midi.as_mut().map(|a| a.midi_out.as_mut()) {
            midi_out.set_local_control(false)?;
            midi_out.send_note(START_NOTE, START_VELOCITY, true)?;
            midi_out.send_note(START_NOTE, START_VELOCITY, false)?;
        }
        Ok(())
    }

    fn start_process(&mut self) -> Result<(), MidiStartPrgError> {
        if self.process.is_some() {
            return Ok(());
        }
        let start_result = (|| -> Result<(), MidiStartPrgError> {
            self.start_chime()?;
            if pause_media().is_ok() {
                self.volume.set_swapped(true)?;
            } else {
                println!("playerctl pause failed, not swapping volume");
            }
            self.process = Some(PianoteqProcess::new(&self.args.program)?);
            Ok(())
        })();
        if let Err(err) = start_result {
            self.do_process_cleanup()?;
            return Err(err);
        }
        Ok(())
    }

    fn maybe_cleanup_process(&mut self) -> Result<(), MidiStartPrgError> {
        if let Some(process) = &mut self.process
            && process.check_has_exited()?
        {
            self.process = None;
            self.do_process_cleanup()?;
        };
        Ok(())
    }

    fn do_process_cleanup(&mut self) -> Result<(), MidiStartPrgError> {
        let volume_swap = self.volume.set_swapped(false);
        let local_control_setting =
            if let Some(Some(midi_out)) = self.midi.as_mut().map(|a| a.midi_out.as_mut()) {
                midi_out.set_local_control(true).map_err(From::from)
            } else {
                Ok(())
            };
        volume_swap.and(local_control_setting)
    }

    fn tick_main(&mut self) -> Result<bool> {
        self.maybe_cleanup_process()?;

        let is_shutdown_requested = self.shutdown.load(Ordering::Relaxed);
        let is_idle = self.last_note_instant.elapsed() > self.idle_timeout;
        if is_shutdown_requested || is_idle {
            self.terminate_and_wait()?;
            if is_idle {
                self.suspend()?;
            }
            return Ok(!is_shutdown_requested);
        }

        if let Err(err) = self.start_midi_if_needed() {
            if let MidiStartPrgError::NoMatchingMidiPort = err {
                eprintln!("{}", err);
                return Ok(true);
            }
            return Err(err);
        }
        #[rustfmt::skip]
        let midi = self.midi.as_mut().expect("start_midi_if_needed initialized it on success");
        match midi.midi_in_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(channel_message) => match channel_message {
                Ok(midi_message) => {
                    self.tick_midi(midi_message)?;
                }
                Err(parse_error) => {
                    println!("MIDI parse error: {}", parse_error);
                }
            },
            Err(receive_error) => match receive_error {
                RecvTimeoutError::Timeout => {
                    if self.last_msg_instant.elapsed() > Duration::from_secs(10) {
                        println!("midi idle timeout");
                    }
                }
                RecvTimeoutError::Disconnected => {
                    println!("midi input thread disconnected");
                }
            },
        }
        Ok(true)
    }

    fn suspend(&mut self) -> Result<()> {
        self.rightmost_count = 0;
        self.midi = None;
        self.terminate_and_wait()?;
        systemctl_suspend()
    }

    fn terminate_and_wait(&mut self) -> Result<(), MidiStartPrgError> {
        if let Some(process) = self.process.as_mut() {
            process.terminate()?;
        }
        while let Some(process) = &mut self.process
            && !process.check_has_exited()?
        {
            println!("waiting for Pianoteq to exit");
            sleep(Duration::from_millis(100));
            self.maybe_cleanup_process()?;
        }
        Ok(())
    }

    fn start_midi_if_needed(&mut self) -> Result<()> {
        if self.midi.is_some() {
            return Ok(());
        }
        let client = MidiInput::new("midi-start-program_input")?;
        let ports = client.ports();
        let mut found_port = None;
        for port in ports {
            let port_name = client.port_name(&port)?;
            if port_name.starts_with(&self.args.device_prefix) {
                found_port = Some((port, port_name));
                break;
            }
        }

        let Some((port, port_name)) = found_port else {
            return Err(MidiStartPrgError::NoMatchingMidiPort);
        };

        println!("connecting to {}", port_name);
        let mut ctx = ReceiverContext::new();
        let (tx, rx) = channel();
        self.last_msg_instant = Instant::now();
        self.rightmost_count = 0;
        let midi_out = MidiOut::open(&self.args.device_prefix)?;
        if midi_out.is_none() {
            println!("no MIDI output port found");
        }
        let _midi_in = client.connect(
            &port,
            &port_name,
            move |_, bytes, ()| {
                let _ =
                    tx.send(MidiMsg::from_midi_with_context(bytes, &mut ctx).map(|(msg, _)| msg));
            },
            (),
        )?;

        println!("connected");
        self.midi = Some(MidiConnection {
            _midi_in,
            midi_out,
            midi_in_rx: rx,
        });
        Ok(())
    }
}

fn main() -> Result<(), MidiStartPrgError> {
    let shutdown = Arc::new(AtomicBool::new(false));
    flag::register(SIGTERM, Arc::clone(&shutdown))?;
    flag::register(SIGINT, Arc::clone(&shutdown))?;
    flag::register(SIGHUP, Arc::clone(&shutdown))?;
    let args = Args::parse();
    let mut app = App {
        shutdown,
        idle_timeout: Duration::from_secs(args.idle),
        args,
        process: None,
        last_note_instant: Instant::now(),
        last_msg_instant: Instant::now(),
        rightmost_count: 0,
        midi: None,
        volume: Volume::new(&xdg::BaseDirectories::with_prefix("midi-start-program"))?,
    };
    loop {
        if !app.tick_main()? {
            return Ok(());
        }
    }
}
