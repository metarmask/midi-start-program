#![feature(exit_status_error)]
#![feature(unix_send_signal)]

use keepawake::KeepAwake;
use midi_msg::ChannelVoiceMsg::NoteOn;
use midi_msg::MidiMsg::ChannelVoice;
use midi_msg::{MidiMsg, ReceiverContext};
use midir::MidiInput;
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::flag;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::process::ChildExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatusError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{thread::sleep, time::Duration};
use thiserror::Error;

fn get_pipewire_volume() -> Result<String, MidiStartPrgError> {
    let output = Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn set_pipewire_volume(volume: &str) -> Result<(), MidiStartPrgError> {
    // Parse volume - handle both "Volume: 0.50" format and plain "1.0" format
    let vol_str = volume.split_whitespace().nth(1).unwrap_or(volume);

    let output = Command::new("wpctl")
        .args(["set-volume", "@DEFAULT_AUDIO_SINK@", vol_str])
        .status()?;
    output.exit_ok()?;
    Ok(())
}

fn pause_media() -> Result<(), MidiStartPrgError> {
    let output = Command::new("playerctl").args(["pause"]).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log_event(&format!(
            "playerctl pause failed (ignored): {}",
            stderr.trim()
        ))?;
        return Ok(());
    }
    Ok(())
}

fn log_event(message: &str) -> Result<(), MidiStartPrgError> {
    eprintln!("{}", message);
    let path = saved_volume_path()?
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("log");
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    use std::io::Write;
    writeln!(
        file,
        "[{}.{:03}] {}",
        now.as_secs(),
        now.subsec_millis(),
        message
    )?;
    Ok(())
}

fn saved_volume_path() -> Result<PathBuf, MidiStartPrgError> {
    let base = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| {
            env::var_os("HOME").map(|home| {
                let mut path = PathBuf::from(home);
                path.push(".local/state");
                path
            })
        })
        .or_else(|| {
            env::var_os("HOME").map(|home| {
                let mut path = PathBuf::from(home);
                path.push(".config");
                path
            })
        })
        .unwrap_or_else(|| PathBuf::from("."));

    let path = base.join("midi-start-program");
    fs::create_dir_all(&path)?;
    Ok(path.join("volume"))
}

fn load_saved_volume() -> Result<Option<String>, MidiStartPrgError> {
    let path = saved_volume_path()?;
    if !path.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(path)?;
    let volume = contents.trim();
    if volume.is_empty() {
        Ok(None)
    } else {
        Ok(Some(volume.to_string()))
    }
}

fn persist_volume(volume: &str) -> Result<(), MidiStartPrgError> {
    let path = saved_volume_path()?;
    fs::write(path, volume.split_whitespace().nth(1).unwrap_or(volume))?;
    Ok(())
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
    #[error("process exited unsucessfully {0}")]
    ExitStatusError(#[from] ExitStatusError),
    #[error("tried to keep computer awake {0}")]
    KeepAwake(#[from] keepawake::Error),
    #[error("thread error {0}")]
    ThreadError(#[from] std::sync::mpsc::RecvError),
}

struct PianoteqProcess {
    child: Child,
    has_exited: bool,
    saved_volume: String,
    _keep_awake: KeepAwake,
}

struct MidiState {
    process: Option<PianoteqProcess>,
    last_note_instant: Instant,
    last_msg_instant: Instant,
}

impl PianoteqProcess {
    fn new() -> Result<Self, MidiStartPrgError> {
        let saved_volume = get_pipewire_volume()?;

        log_event("spawning Pianoteq")?;
        pause_media()?;
        if let Some(volume) = load_saved_volume()? {
            set_pipewire_volume(&volume)?;
        }
        println!("Spawning...");
        Ok(PianoteqProcess {
            child: Command::new("/home/metarmask/.local/opt/pianoteq_stage_linux_v843/Pianoteq 8 STAGE/x86-64bit/Pianoteq 8 STAGE").spawn()?,
            has_exited: false,
            _keep_awake: keepawake::Builder::default()
                    .sleep(true)
                    .reason("Playing piano")
                    .app_name("Auto-Pianoteq")
                    .app_reverse_domain("pianoteq.auto")
                    .create()?,
            saved_volume
        })
    }

    fn kill(&mut self) -> Result<(), MidiStartPrgError> {
        log_event("sending SIGTERM to Pianoteq")?;
        println!("Kill(15)ing");
        self.child.send_signal(15).map_err(Into::into)
        // self.child.kill().map_err(Into::into)
    }

    fn check_is_exited(&mut self) -> Result<bool, MidiStartPrgError> {
        if !self.has_exited && self.child.try_wait()?.is_some() {
            self.has_exited = true;

            log_event("Pianoteq exited; restoring volume")?;
            let current_volume = get_pipewire_volume()?;
            persist_volume(&current_volume)?;
            set_pipewire_volume(&self.saved_volume)?;
        }
        Ok(self.has_exited)
    }
}

fn handle_msg(
    bytes: &[u8],
    state: &mut MidiState,
    ctx: &mut ReceiverContext,
) -> Result<bool, MidiStartPrgError> {
    let (msg, _) = MidiMsg::from_midi_with_context(bytes, ctx)?;
    state.last_msg_instant = Instant::now();
    match msg {
        ChannelVoice {
            msg: NoteOn { .. }, ..
        } => {
            state.last_note_instant = Instant::now();
            if state.process.is_none() {
                state.process = Some(PianoteqProcess::new()?);
            }
        }
        MidiMsg::SystemRealTime { .. } => {
            if state.last_note_instant.elapsed() > Duration::from_mins(3)
                && let Some(process) = state.process.as_mut()
            {
                process.kill()?;
            }
            if let Some(p) = state.process.as_mut()
                && p.check_is_exited()?
            {
                state.process = None;
            }
        }
        _ => {}
    }
    Ok(true)
}

fn main() -> Result<(), MidiStartPrgError> {
    log_event("startup")?;
    let shutdown = Arc::new(AtomicBool::new(false));
    flag::register(SIGTERM, Arc::clone(&shutdown))?;
    flag::register(SIGINT, Arc::clone(&shutdown))?;
    flag::register(SIGHUP, Arc::clone(&shutdown))?;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            log_event("shutdown signal received; exiting")?;
            return Ok(());
        }
        let client = MidiInput::new("I am client")?;
        let ports = client.ports();
        let mut found_port = None;
        for port in ports {
            let port_name = client.port_name(&port)?;
            if port_name.starts_with("Digital Piano") {
                found_port = Some((port, port_name));
                break;
            }
        }

        let Some((port, port_name)) = found_port else {
            log_event("no matching MIDI port; retrying")?;
            sleep(Duration::from_secs(2));
            continue;
        };

        log_event(&format!("connecting to {}", port_name))?;
        let mut ctx = ReceiverContext::new();
        let (tx, rx) = channel::<Result<(), MidiStartPrgError>>();
        let state = Arc::new(Mutex::new(MidiState {
            process: None,
            last_note_instant: Instant::now(),
            last_msg_instant: Instant::now(),
        }));
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
                    log_event("connection requested shutdown")?;
                    break;
                }
                Ok(Err(err)) => {
                    log_event(&format!("midi error: {}", err))?;
                    break;
                }
                Err(RecvTimeoutError::Timeout) => {
                    if shutdown.load(Ordering::Relaxed) {
                        if let Ok(mut state) = state.lock()
                            && let Some(process) = state.process.as_mut()
                        {
                            let _ = process.kill();
                        }
                        log_event("shutdown signal received; exiting")?;
                        return Ok(());
                    }
                    if let Ok(state) = state.lock()
                        && state.last_msg_instant.elapsed() > Duration::from_secs(10)
                    {
                        log_event("midi idle timeout; reconnecting")?;
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    log_event("midi channel disconnected; reconnecting")?;
                    break;
                }
            }
        }

        sleep(Duration::from_secs(1));
    }
}
