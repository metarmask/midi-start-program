use midi_msg::{Channel, ChannelModeMsg, MidiMsg};
use midir::{MidiOutput, MidiOutputConnection};

use crate::MidiStartPrgError;

pub struct MidiOut {
    conn: MidiOutputConnection,
    channel: Channel,
}

impl MidiOut {
    fn send_msg(&mut self, msg: MidiMsg) -> Result<(), midir::SendError> {
        let bytes = msg.to_midi();
        self.conn.send(&bytes)
    }

    pub fn set_local_control(&mut self, on: bool) -> Result<(), midir::SendError> {
        self.send_msg(MidiMsg::ChannelMode {
            channel: self.channel,
            msg: ChannelModeMsg::LocalControl(on),
        })
    }

    pub fn send_note(&mut self, note: u8, velocity: u8, on: bool) -> Result<(), midir::SendError> {
        self.send_msg(MidiMsg::ChannelVoice {
            channel: self.channel,
            msg: if on {
                midi_msg::ChannelVoiceMsg::NoteOn { note, velocity }
            } else {
                midi_msg::ChannelVoiceMsg::NoteOff { note, velocity }
            },
        })
    }

    pub fn open(port_prefix: &str) -> Result<Option<Self>, MidiStartPrgError> {
        let output = MidiOutput::new("midi-start-program_output")?;
        for port in output.ports() {
            let name = output.port_name(&port)?;
            if name.starts_with(port_prefix) {
                return Ok(Some(MidiOut {
                    conn: output.connect(&port, "midi-start-program_output")?,
                    channel: Channel::Ch1,
                }));
            }
        }
        Ok(None)
    }
}
