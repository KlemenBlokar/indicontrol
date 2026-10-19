# indicontrol

Astrophotography control for INDI equipment: mount, cameras, filter wheel,
focuser, guiding and imaging sequences.

## Architecture

```
indiserver ──TCP/XML──► engine ──WebSocket/JSON──► UI (native app or browser)
```

- **engine** (`crates/engine`, binary `indicontrol-engine`) owns the INDI
  connection and all state. It will also run guiding, sequences and FITS
  output, so it should run on the computer at the telescope. It reconnects to
  INDI automatically and keeps working when no UI is attached.
- **UI** (`crates/app`, binary `indicontrol`) is an egui app that only
  displays state and sends commands. The same code builds natively and for
  the browser (wasm). Any number of UIs can be connected at once.
- **protocol** (`crates/protocol`) defines the engine ↔ UI messages.
- **indi-link** (`crates/indi-link`) is the INDI client and property model.

## Running

Local, everything in one process (the engine is embedded in the app):

```sh
cargo run --release --bin indicontrol -- --indi localhost:7624
```

Remote: run the engine next to `indiserver` and connect from anywhere:

```sh
# on the telescope computer
indicontrol-engine --listen 0.0.0.0:7700 --indi localhost:7624
# on the laptop
indicontrol --engine telescope-pi.local
```

The engine binds to `127.0.0.1` by default. Pass `--listen 0.0.0.0:7700`
only on a trusted network: like INDI itself, there is no authentication yet.

The engine saves its settings (INDI server, equipment roles) in
`~/.config/indicontrol/engine.json`.

## Imaging

The Imaging tab takes single exposures or loops (for framing and focusing)
with the main camera. The engine writes the FITS files on its own machine,
by default under `~/Pictures/indicontrol`, with a configurable path template:

```
{object}/{date}/{type}/{filter}/{object}_{type}_{filter}_{exposure}s_G{gain}_O{offset}_{temp}C_bin{bin}_{seq:04}
```

`{date}` is the night of the session, so frames taken after midnight stay
with the evening they belong to. Existing files are never overwritten.

Each file gets `IMAGETYP` (`Light Frame`, `Dark Frame`, `Bias Frame`,
`Flat Frame`), `FILTER`, `OBJECT`, `EXPTIME`, `GAIN`, `OFFSET`, `TELESCOP`,
`FOCALLEN`, `APTDIA`, J2000 `RA`/`DEC`/`OBJCTRA`/`OBJCTDEC`, `PIERSIDE`,
`AIRMASS`, `SITELAT`/`SITELONG`, `FOCPOS`/`FOCTEMP`, and `DATE-OBS`,
`CCD-TEMP`, `XBINNING` where the driver did not provide them. Pixel data is
saved exactly as the camera sent it.

The histogram panel shows linear or log histograms per channel, with
counts of pixels at 0 (black clipping: raise the offset), how far the dark
end of the signal sits above 0 in noise sigmas, and saturated pixels.

## Sequences

The Sequence tab builds a list of steps, each with frame type, filter,
exposure, count, and optional binning, gain, offset and sensor temperature.
Steps run one after another or interleaved (LRGBLRGB…). Presets add
LRGB or SHO lights (filters are found by name in the wheel), exposure
bracketing, darks, bias, flats and dark flats.

- **Flats** use automatic exposure. Test frames, which are not saved, adjust
  the exposure until the brightest channel's median sits at the target
  (45 % ± 5 % by default). Sky flats keep adjusting as the light changes.
- **Dark flats** reuse the exposure found for flats with the same filter,
  binning and gain. The engine remembers it across sessions.
- **Temperature**: a step with a temperature cools the sensor and waits
  until it is within 0.5 °C of the target before exposing.

Sequences are saved on the engine, so every connected UI shares them.
Pause takes effect after the current frame.

## Guiding

The Guiding tab runs a PHD2-style autoguider on the guide camera (assign it
in Equipment). Guide pulses go through the mount driver by default, or the
guide camera's ST-4 port.

- **Guide** does everything: picks the best unsaturated star, calibrates if
  needed, then guides. Click a star in the guide image to choose it yourself.
- **Multi-star** (on by default, nothing to tune): every other isolated,
  unsaturated star in the frame is tracked as well, and new ones are picked
  up as they appear. Each frame the guide position is the SNR-weighted
  average of all stars that agree; stars that disagree are ignored, then
  dropped. Movement shared by all stars is mount error. Slow movement that
  differs between stars (field rotation from polar misalignment, refraction)
  is absorbed into each star's reference, so it never turns into guide
  corrections. If the primary star is lost, another takes over seamlessly.
  With only one usable star this is ordinary single-star guiding. The
  measured field rotation is shown and hints at polar alignment error.
- **Calibration** moves the mount west/east and north/south (taking up Dec
  backlash first) to measure the camera angle and pulse rates. It is saved
  and reused, with the RA rate adjusted for declination.
- **RA** uses hysteresis guiding: it corrects a fraction (aggressiveness) of
  each error and ignores moves smaller than the minimum. **Dec** resists
  switching direction until several frames agree, or can be limited to
  north-only or south-only.
- The graph shows RA/Dec error in arcseconds once the pixel scale is known.
  Pixel size and focal length come from INDI or the settings.

- **Dither** moves the lock position randomly by up to the dither amount
  along the mount axes, then waits until the error stays below the settle
  threshold for the settle time (or gives up after the timeout).
- After a **meridian flip** the saved calibration is reused, turned by 180°
  (Dec reversal can be switched off for mounts that reverse Dec themselves).

Not yet: backlash compensation.

## Mount and meridian flips

The Mount tab shows J2000/JNow position, altitude, hour angle, time to the
meridian and pier side, and controls tracking, park and goto.

**Find** looks an object up while you type and fills in its J2000
coordinates: Messier, NGC and IC numbers in any spelling (`M110`, `ngc 205`),
common names (`Orion Nebula`, `horsehead`) and other catalogue numbers listed
for the object (`UGC 454`, `Sh2-155`). The catalogue is
[OpenNGC](https://github.com/mattiaverga/OpenNGC), built into the engine, so
it works without internet. In the Sequence tab, **From mount** then also
sets the object name, as long as the mount still points at that object.

Sequences can **dither every N light frames** while guiding, waiting for
guiding to settle before the next frame. With **meridian flip** enabled, a
light frame is never started if it would run past the flip point (default
5 min after the meridian); the sequence waits, then stops guiding, re-slews
to the same position so the mount changes pier side, restarts guiding with
the flipped calibration, waits for it to settle and continues. A manual slew
while guiding stops the guider.

## Plate solving

Plate solving uses an external solver on the engine's computer, chosen in
the Mount tab: **ASTAP** (`astap` / `astap_cli`, fast, needs its star
database) or **astrometry.net** (`solve-field`, needs index files for your
field of view). The mount position and the pixel scale (from the camera
pixel size and focal length) are passed as hints.

- **Solve & sync** takes a frame with the main camera, solves it and syncs
  the mount.
- **Slew & center** slews, solves, syncs and slews again until the target
  is within the tolerance (default 1′).
- Sequences can have a target, centred before the first frame and again
  after a meridian flip.

## Focusing

The Focus tab moves the focuser (assign it in Equipment) and runs
**autofocus**: it samples star size (half-flux radius) at evenly spaced
positions around the current one, always approaching from outside (with
optional backlash overshoot), fits the V-curve, and moves to the minimum. If
best focus lies outside the sampled range, the range is extended. Every main
camera frame reports its star count and HFR.

Sequences can refocus at the start, after N light frames, on a filter
change, after a meridian flip, or when the focuser temperature has changed.

## Profiles and night mode

**Profiles** (top of the Equipment panel) save a whole rig under a name:
INDI server, device roles, imaging, guiding, solver and focus settings, the
guide calibration and remembered flat exposures. Loading one switches
everything, reconnecting to its INDI server if needed. Profiles live on the
engine, so every UI sees them.

**Night mode** turns the interface red: text and widgets, but also previews
and the guide image (tinted red), plots and status colours (mapped to reds of
similar brightness, with dashed/dotted lines where colour told series apart).

## Web UI

Build the wasm UI with [trunk](https://trunkrs.dev) (needs the
`wasm32-unknown-unknown` Rust target), then let the engine serve it:

```sh
cd crates/app && trunk build --release
indicontrol-engine --listen 0.0.0.0:7700 --web-root crates/app/dist
# open http://telescope-pi.local:7700/
```

Release builds also write Brotli (if `brotli` is installed) and gzip copies of
the wasm, and the engine serves those: the download shrinks from ~9 MB to
~2.7 MB. The page reconnects by itself when the engine restarts, and
remembers its own view settings (tab, night mode) in the browser.

There is no login: anyone who can reach the port can control the
equipment. Keep `--listen` on a trusted network (or behind a VPN/SSH tunnel).

## Testing without hardware

The INDI simulators (`indi_simulator_ccd`, `indi_simulator_telescope`,
`indi_simulator_wheel`, `indi_simulator_focus`, `indi_simulator_guide`) ship
with libindi:

```sh
indiserver indi_simulator_telescope indi_simulator_ccd indi_simulator_wheel
```

`cargo test --workspace` runs unit tests plus end-to-end tests against a
scripted fake INDI server.

Tests against the real simulators are opt-in. The CCD and guide simulators
only render stars when the GSC catalogue is installed (`gsc` on PATH and
`GSCDAT` pointing at its data); plate solving uses ASTAP:

```sh
GSCDAT=~/.local/share/GSC indiserver -p 7630 indi_simulator_telescope \
    indi_simulator_ccd indi_simulator_wheel indi_simulator_focus indi_simulator_guide
INDI_SIM=127.0.0.1:7630 ASTAP=~/.local/opt/astap/astap_cli \
    cargo test -p engine --test simulators -- --nocapture
```

INDI 2.x keeps the telescope focal length on each camera (`SCOPE_INFO`).
Set it under **Optics** in the Equipment panel: the engine writes it to the
cameras, and FITS metadata, plate solving and the guider rely on it.

## License

Copyright © 2026 Klemen Blokar. This work is free. You can redistribute it
and/or modify it under the terms of the Do What The Fuck You Want To Public
License, Version 2, as published by Sam Hocevar. See the [LICENSE](LICENSE)
file for more details.

The dependencies (egui, tokio and others) keep their own licenses. The
OpenNGC catalogue in `crates/engine/data/openngc` is by Mattia Verga, under
CC-BY-SA-4.0.
