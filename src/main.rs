#![feature(exit_status_error)]
#![feature(unix_send_signal)]

use clap::Parser;
use keepawake::KeepAwake;
use midi_msg::ChannelVoiceMsg::NoteOn;
use midi_msg::MidiMsg::ChannelVoice;
use midi_msg::{MidiMsg, ReceiverContext};
use midir::{MidiInput, MidiOutput, SendError};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::flag;
use std::io;
use std::os::unix::process::ChildExt;
use std::process::{Child, Command, ExitStatusError, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
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
}

fn pause_media() -> Result<(), MidiStartPrgError> {
    Ok(Command::new("playerctl")
        .args(["pause"])
        .stderr(Stdio::inherit())
        .output()?
        .exit_ok()
        .map(|_| ())?)
}

fn suspend_system() -> Result<(), MidiStartPrgError> {
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

struct MidiState {
    process: Option<PianoteqProcess>,
    last_note_instant: Instant,
    last_msg_instant: Instant,
    rightmost_count: u8,
    suspend_on_exit: bool,
    midi_out: Option<MidiOut>,
    volume: Volume,
    program: String,
    idle_timeout: Duration,
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

    fn kill(&mut self) -> Result<(), MidiStartPrgError> {
        self.child.send_signal(15).map_err(Into::into)
    }

    fn check_is_exited(&mut self) -> Result<bool, MidiStartPrgError> {
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
        if self.has_exited {
            return;
        }
        match self.child.try_wait() {
            Ok(Some(_)) => {
                self.has_exited = true;
            }
            Ok(None) => {
                panic!("PianoteqProcess dropped while Pianoteq is still running");
            }
            Err(err) => {
                panic!("PianoteqProcess drop failed to check child status: {err}");
            }
        }
    }
}

fn refresh_process_state(state: &mut MidiState) -> Result<(), MidiStartPrgError> {
    let has_exited = if let Some(process) = state.process.as_mut() {
        process.check_is_exited()?
    } else {
        false
    };

    if has_exited {
        state.process = None;
        state.volume.set_swapped(false)?;
        if let Some(midi_out) = &mut state.midi_out {
            midi_out.set_local_control(true)?;
        }
        if state.suspend_on_exit {
            state.suspend_on_exit = false;
            suspend_system()?;
        }
    }
    Ok(())
}

fn enforce_idle_timeout(state: &mut MidiState) -> Result<(), MidiStartPrgError> {
    if state.last_note_instant.elapsed() > state.idle_timeout
        && let Some(process) = state.process.as_mut()
    {
        process.kill()?;
    }
    Ok(())
}

fn handle_msg(
    bytes: &[u8],
    state: &mut MidiState,
    ctx: &mut ReceiverContext,
) -> Result<bool, MidiStartPrgError> {
    const RIGHTMOST_C: u8 = 108;
    const TRIGGER_NOTE: u8 = RIGHTMOST_C - 2;
    const START_NOTE: u8 = 60; // middle C
    const START_VELOCITY: u8 = 80;

    refresh_process_state(state)?;

    let (msg, _) = MidiMsg::from_midi_with_context(bytes, ctx)?;
    state.last_msg_instant = Instant::now();
    if let ChannelVoice {
        msg: NoteOn { note, velocity, .. },
        ..
    } = msg
    {
        if velocity > 0 {
            if note == RIGHTMOST_C {
                state.rightmost_count = state.rightmost_count.saturating_add(1);
            } else if note == TRIGGER_NOTE && state.rightmost_count >= 7 {
                if let Some(process) = state.process.as_mut() {
                    let _ = process.kill();
                }
                state.suspend_on_exit = true;
                state.rightmost_count = 0;
                return Ok(true);
            } else {
                state.rightmost_count = 0;
            }
        }
        state.last_note_instant = Instant::now();
        if state.process.is_none() {
            let mut volume_swapped = false;
            let start_result = (|| -> Result<(), MidiStartPrgError> {
                if let Some(midi_out) = &mut state.midi_out {
                    midi_out.set_local_control(false)?;
                    midi_out.send_note(START_NOTE, START_VELOCITY, true)?;
                    midi_out.send_note(START_NOTE, START_VELOCITY, false)?;
                }
                if pause_media().is_ok() {
                    state.volume.set_swapped(true)?;
                    volume_swapped = true;
                } else {
                    println!("playerctl pause failed, not swapping volume");
                }
                state.process = Some(PianoteqProcess::new(&state.program)?);
                Ok(())
            })();
            if let Err(err) = start_result {
                if volume_swapped {
                    let _ = state.volume.set_swapped(false);
                }
                if let Some(midi_out) = &mut state.midi_out {
                    let _ = midi_out.set_local_control(true);
                }
                return Err(err);
            }
        }
    }
    enforce_idle_timeout(state)?;
    Ok(true)
}

fn main() -> Result<(), MidiStartPrgError> {
    let args = Args::parse();

    let shutdown = Arc::new(AtomicBool::new(false));
    flag::register(SIGTERM, Arc::clone(&shutdown))?;
    flag::register(SIGINT, Arc::clone(&shutdown))?;
    flag::register(SIGHUP, Arc::clone(&shutdown))?;
    let state = Arc::new(Mutex::new(MidiState {
        process: None,
        last_note_instant: Instant::now(),
        last_msg_instant: Instant::now(),
        rightmost_count: 0,
        suspend_on_exit: false,
        midi_out: None,
        volume: Volume::new(&xdg::BaseDirectories::with_prefix("midi-start-program"))?,
        program: args.program,
        idle_timeout: Duration::from_secs(args.idle),
    }));
    loop {
        if shutdown.load(Ordering::Relaxed) {
            println!("shutdown signal received; exiting");
            return Ok(());
        }
        let client = MidiInput::new("midi-start-program_input")?;
        let ports = client.ports();
        let mut found_port = None;
        for port in ports {
            let port_name = client.port_name(&port)?;
            if port_name.starts_with(&args.device_prefix) {
                found_port = Some((port, port_name));
                break;
            }
        }

        let Some((port, port_name)) = found_port else {
            println!("no matching MIDI port; retrying");
            sleep(Duration::from_secs(2));
            continue;
        };

        println!("connecting to {}", port_name);
        let mut ctx = ReceiverContext::new();
        let (tx, rx) = channel::<Result<(), MidiStartPrgError>>();
        if let Ok(mut state) = state.lock() {
            state.last_msg_instant = Instant::now();
            state.rightmost_count = 0;
            state.midi_out = MidiOut::open(&args.device_prefix)?;
            if state.midi_out.is_none() {
                println!("no MIDI output port found");
            }
        }
        let state_cb = Arc::clone(&state);
        let _conn = client.connect(
            &port,
            &port_name,
            move |_, bytes, ()| {
                let mut state = match state_cb.lock() {
                    Ok(state) => state,
                    Err(_) => return,
                };
                match handle_msg(bytes, &mut state, &mut ctx) {
                    Ok(true) => {}
                    Ok(false) => tx.send(Ok(())).unwrap(),
                    Err(err) => tx.send(Err(err)).unwrap(),
                }
            },
            (),
        )?;

        loop {
            match rx.recv_timeout(Duration::from_secs(3)) {
                Ok(Ok(())) => {
                    println!("connection requested shutdown");
                    break;
                }
                Ok(Err(err)) => {
                    println!("midi error: {}", err);
                    break;
                }
                Err(RecvTimeoutError::Timeout) => {
                    if shutdown.load(Ordering::Relaxed) {
                        if let Ok(mut state) = state.lock()
                            && let Some(process) = state.process.as_mut()
                        {
                            let _ = process.kill();
                        }
                        println!("shutdown signal received; exiting");
                        return Ok(());
                    }
                    if let Ok(mut state) = state.lock() {
                        refresh_process_state(&mut state)?;
                        enforce_idle_timeout(&mut state)?;
                        if state.last_msg_instant.elapsed() > Duration::from_secs(10) {
                            println!("midi idle timeout; reconnecting");
                            break;
                        }
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    println!("midi channel disconnected; reconnecting");
                    break;
                }
            }
        }

        sleep(Duration::from_secs(1));
    }
}
