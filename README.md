# Description
eqvis is a real-time digital equalizer with a console-training UI. It applies the EQ to whatever you play and (optionally) to the selected microphone input, so you can train your ear on real desk EQ: a channel strip, ±15 dB gain, and a fixed 5-band layout with restricted Q.

# Usage
## Install
* Download the prebuilt binary for your platform from the releases page, or build from source with:

```
cargo build --release --bin eqvis
```

* Linux builds need a few system packages:
```
sudo apt-get install pkg-config libasound2-dev libgtk-3-dev \
  libxkbcommon-dev libwayland-dev libxcb-render0-dev \
  libxcb-shape0-dev libxcb-xfixes0-dev libfontconfig1-dev
```
* Windows and macOS builds just work (macOS cross-build is done in CI, so releases include both Apple Silicon and Intel).
* note: the macOS release is unsigned. The first time you open it, right-click the app -> Open, then Open again. It will remember you after that.
* The input (microphone) is optional - the EQ applies to the output signal either way. Sound comes out of the selected output device, so that's the one you want to match to your speakers or interface.

## Run
* Launch eqvis.
* Pick the audio output, and optionally an input to EQ along with it.
* Drag the dots on the graph to sculpt the curve. Drag up/down changes the band, or hover a dot and use the scroll wheel.
* Controls while hovering a band dot:
  * scroll = gain, shift + scroll = frequency, ctrl + scroll = Q
* Console mode (the "Console mode (training)" checkbox) locks you into a fixed 5-band channel strip: Low Shelf @ 80 Hz, Peak @ 400 Hz, Peak @ 1 kHz, Peak @ 3 kHz, High Shelf @ 12 kHz, ±15 dB, Q 0.5-5.0. Dots stay draggable like a digital console's touchscreen.
* Toggle the "Knob UI" setting to render the band tiles as rotary gain/freq/Q knobs instead of sliders.

## Config
* Settings are saved automatically to `config.txt` in:
  * Linux: `~/.config/eqvis/config.txt` (or `$XDG_CONFIG_HOME`)
  * macOS: `~/Library/Application Support/eqvis/config.txt`
  * Windows: `%APPDATA%\eqvis\config.txt`
* You can edit the file by hand or delete it to reset to defaults. Example:
```
theme_dark=1
idle_anim=1
peak_hold=1
console_mode=0
knob_ui=0
db_min=-60
db_max=60
vol_min=0.33333334
vol_max=3
num_bands=6
```
* note: there's no hand-written config like a yml - the app writes this file itself on close. Missing keys fall back to the defaults above.