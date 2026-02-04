use std::{fs, io, path::PathBuf, process::Command};

use crate::MidiStartPrgError;

pub struct Volume {
    volume_path: PathBuf,
    standard_volume: Option<String>,
    swapped: bool,
}

impl Volume {
    fn set_with_pipewire(volume: &str) -> Result<(), MidiStartPrgError> {
        // Parse volume - handle both "Volume: 0.50" format and plain "1.0" format
        let vol_str = volume.split_whitespace().nth(1).unwrap_or(volume);

        let output = Command::new("wpctl")
            .args(["set-volume", "@DEFAULT_AUDIO_SINK@", vol_str])
            .status()?;
        output.exit_ok()?;
        Ok(())
    }

    fn get_from_pipewire() -> Result<String, MidiStartPrgError> {
        let output = Command::new("wpctl")
            .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
            .output()?;
        let string_output = String::from_utf8_lossy(&output.stdout).trim().to_string();
        string_output
            .clone()
            .split_whitespace()
            .nth(1)
            .map(str::to_string)
            .ok_or(MidiStartPrgError::ParsingVolume(string_output))
    }

    pub fn new(base_directories: &xdg::BaseDirectories) -> Result<Volume, io::Error> {
        Ok(Volume {
            volume_path: base_directories.place_state_file("volume")?,
            standard_volume: None,
            swapped: false,
        })
    }

    pub fn set_swapped(&mut self, swapped: bool) -> Result<(), MidiStartPrgError> {
        if self.swapped != swapped {
            self.swapped = swapped;
            if swapped {
                let standard_volume = Volume::get_from_pipewire()?;
                let swapped_mode_volume = fs::read_to_string(&self.volume_path)?;
                Volume::set_with_pipewire(&swapped_mode_volume)?;
                self.standard_volume = Some(standard_volume);
            } else {
                let swapped_mode_volume = Volume::get_from_pipewire()?;
                fs::write(&self.volume_path, swapped_mode_volume)?;
                Volume::set_with_pipewire(&self.standard_volume.clone().expect("swapped starts out false so the branch that sets standard_volume always runs first"))?;
            }
        };
        Ok(())
    }
}
