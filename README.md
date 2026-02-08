## MIDI synth auto-start and idle-inhibiting utility

Use case: You use a software synthesizer with your digital piano placed away from your speakers and want:

- an intuitive way to start it
- a separate max volume setting
- to prevent your computer from sleeping while you are actively using it
- want to fallback to the built-in synthesizer when the computer is off
- the built-in metronome to be usable while the synthesizer is on
- work around desktop environment sleep inhibition bugs

### How to use

- (Recommended) Set up a [systemd service](/midi-start-program.service.example) and point --synth to the synthesizer executable.
- Press any button on the keyboard. If supported, you will hear a hard note on your piano confirming the executable was started.
- When you are done playing you can:
  - Close the program to continue to use the computer.
  - Perform the sleep gesture: hit rightmost C 5 times followed by the closest Bb.
  - Let the idle timeout expire, which causes the computer to sleep.

### Requirements

- Linux (SIGTERM and CLI commands are specific to it)
- PipeWire and WirePlumber with `wpctl` available
- `playerctl` installed (used to pause active media playback on synth start).
- SystemD (used for sleep/suspension)
- For the current sleep gesture, a 88-key keyboard is required.
- (Recommended for not requiring muting) A keyboard which responds to local control messages on channel 1.
- (Recommended) A MIDI input/output device whose port names share a prefix (`--device-prefix`).

Package manager command:

```
sudo pacman -Syu --needed wireplumber playerctl
```
