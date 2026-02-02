#!/usr/bin/env python3
"""
MIDI middleware:
 - Listens to a MIDI input device (or all inputs)
 - Creates a virtual output port that spawned apps can connect to
 - When a configured trigger NOTE_ON is received, launch an external app/command
 - While app runs, forward all MIDI messages to virtual output
 - When app exits, continue listening for next trigger

Usage:
 python midi_midware.py --trigger-note 60 --cmd "path/to/app --arg1" --input "Your MIDI Input Name"
"""

import argparse
import shlex
import subprocess
import time
import threading
import sys

import rtmidi
from mido import Message, MidiFile, open_input  # mido only used to pretty-print messages; rtmidi does low-level IO

# ---------- Configurable defaults ----------
VIRTUAL_PORT_NAME = "MIDWARE-VIRTUAL-OUT"
TRIGGER_NOTE_DEFAULT = 60  # middle C
TRIGGER_CHANNEL_DEFAULT = None  # None = any channel
DEBOUNCE_SEC = 0.2  # ignore retrigger for this many seconds after the trigger
# --------------------------------------------

def parse_args():
    p = argparse.ArgumentParser(description="MIDI middleware that spawns an app on note press.")
    p.add_argument("--trigger-note", type=int, default=TRIGGER_NOTE_DEFAULT,
                   help="MIDI note number that triggers the app (0-127). Default: 60")
    p.add_argument("--trigger-channel", type=int, default=None,
                   help="Specific MIDI channel (1-16) to listen for trigger. Default: any")
    p.add_argument("--cmd", type=str, required=True,
                   help='Command to run when triggered. Example: "python my_app.py --port MIDWARE-VIRTUAL-OUT"')
    p.add_argument("--input", type=str, default=None,
                   help="Name of MIDI input to open. If omitted, middleware listens on the default input or first available.")
    p.add_argument("--list-inputs", action="store_true", help="List available MIDI inputs and exit.")
    return p.parse_args()

class MidiMiddleware:
    def __init__(self, virtual_port_name, trigger_note, trigger_channel, cmd, input_name=None):
        self.virtual_port_name = virtual_port_name
        self.trigger_note = trigger_note
        self.trigger_channel = trigger_channel  # 1-16 or None
        self.cmd = cmd
        self.input_name = input_name
        self._last_trigger_ts = 0.0
        self._app_proc = None
        self._closing = False

        # rtmidi objects
        self.midi_in = rtmidi.MidiIn()
        self.midi_out = rtmidi.MidiOut()

        # create a virtual output port. The spawned app will connect to this port.
        try:
            self.midi_out.open_virtual_port(self.virtual_port_name)
            print(f"[midware] Opened virtual output port: '{self.virtual_port_name}'")
        except Exception as e:
            print("[midware] ERROR creating virtual port:", e)
            raise

    def list_inputs(self):
        ports = self.midi_in.get_ports()
        if not ports:
            print("No MIDI input ports found.")
            return
        print("Available MIDI input ports:")
        for i, p in enumerate(ports):
            print(f" {i}: {p}")

    def open_input(self):
        # If user provided an input_name, try to open it; fallback to first available.
        ports = self.midi_in.get_ports()
        if not ports:
            raise RuntimeError("No MIDI input ports available.")
        selected_index = None
        if self.input_name:
            # try to find a matching port (substring match)
            for i, p in enumerate(ports):
                if self.input_name.lower() in p.lower():
                    selected_index = i
                    break
            if selected_index is None:
                print(f"[midware] Could not find input matching '{self.input_name}'. Available ports:")
                for i, p in enumerate(ports):
                    print(f"  {i}: {p}")
                raise RuntimeError("Input port not found.")
        else:
            selected_index = 0
            print(f"[midware] Using default input port: {ports[0]}")

        self.midi_in.open_port(selected_index)
        # set callback that handles incoming raw midi data
        self.midi_in.set_callback(self._rtmidi_callback)
        print("[midware] Listening for MIDI on port:", ports[selected_index])

    def _rtmidi_callback(self, event, data=None):
        """
        event: (message_bytes, delta_time)
        We'll forward messages to the virtual output.
        We'll detect trigger NOTE_ON messages and spawn app.
        """
        message_bytes, delta = event
        if not message_bytes:
            return
        # message_bytes is a list of ints (raw)
        status = message_bytes[0]
        # Determine message type and channel
        msg_type_nibble = status & 0xF0
        channel = (status & 0x0F) + 1  # 1-16
        # NOTE ON message type is 0x90; NOTE OFF is 0x80 (or note on with vel 0)
        if msg_type_nibble == 0x90:
            note = message_bytes[1]
            velocity = message_bytes[2]
            is_note_on = velocity > 0
            if is_note_on:
                # check channel if necessary
                if (self.trigger_channel is None) or (channel == self.trigger_channel):
                    now = time.time()
                    if now - self._last_trigger_ts > DEBOUNCE_SEC:
                        print(f"[midware] Trigger note {note} (ch {channel}) received -> launching app")
                        self._last_trigger_ts = now
                        # spawn in a separate thread so MIDI callback returns quickly
                        threading.Thread(target=self._spawn_app, daemon=True).start()

        # Forward the raw message to virtual output (so the app connected receives everything)
        try:
            self.midi_out.send_message(message_bytes)
        except Exception as e:
            print("[midware] Warning: failed to forward message:", e)

    def _spawn_app(self):
        # Avoid spawning if an app is already running
        if self._app_proc and self._app_proc.poll() is None:
            print("[midware] App already running (ignoring trigger).")
            return
        # Use shlex.split to handle quoted args properly
        try:
            args = shlex.split(self.cmd)
        except Exception:
            args = [self.cmd]
        print("[midware] Running command:", args)
        try:
            # Start the app. Inherit stdio so you can see its logs in console.
            proc = subprocess.Popen(args)
            self._app_proc = proc
            print(f"[midware] Spawned app pid={proc.pid}. Waiting for it to exit...")
            # Wait for process to finish. This blocks this thread only.
            proc.wait()
            print(f"[midware] App pid={proc.pid} exited with code {proc.returncode}. Returning to listening.")
            self._app_proc = None
        except FileNotFoundError:
            print("[midware] ERROR: Command not found:", self.cmd)
        except Exception as e:
            print("[midware] ERROR launching command:", e)

    def run_forever(self):
        try:
            self.open_input()
        except Exception as e:
            print("[midware] Could not open input:", e)
            return
        print("[midware] Middleware running. Trigger note:", self.trigger_note)
        try:
            while not self._closing:
                time.sleep(0.1)
        except KeyboardInterrupt:
            print("\n[midware] KeyboardInterrupt — shutting down.")
        finally:
            self.close()

    def close(self):
        self._closing = True
        try:
            if self.midi_in:
                self.midi_in.close_port()
        except Exception:
            pass
        try:
            if self.midi_out:
                self.midi_out.close_port()
        except Exception:
            pass
        if self._app_proc:
            print("[midware] Terminating child app...")
            try:
                self._app_proc.terminate()
            except Exception:
                pass

def main():
    args = parse_args()
    # If user asked to list inputs -> show and exit
    midi_in = rtmidi.MidiIn()
    if args.list_inputs:
        ports = midi_in.get_ports()
        if not ports:
            print("No MIDI input ports.")
        else:
            print("MIDI input ports:")
            for i, p in enumerate(ports):
                print(f" {i}: {p}")
        return

    mm = MidiMiddleware(
        virtual_port_name=VIRTUAL_PORT_NAME,
        trigger_note=args.trigger_note,
        trigger_channel=args.trigger_channel,
        cmd=args.cmd,
        input_name=args.input
    )
    mm.run_forever()

if __name__ == "__main__":
    main()
