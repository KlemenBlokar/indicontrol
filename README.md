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

Not yet: multi-star guiding, backlash compensation.

## Mount and meridian flips

The Mount tab shows J2000/JNow position, altitude, hour angle, time to the
meridian and pier side, and controls tracking, park and goto.

Sequences can **dither every N light frames** while guiding, waiting for
guiding to settle before the next frame. With **meridian flip** enabled, a
light frame is never started if it would run past the flip point (default
5 min after the meridian); the sequence waits, then stops guiding, re-slews
to the same position so the mount changes pier side, restarts guiding with
the flipped calibration, waits for it to settle and continues. A manual slew
while guiding stops the guider.

## Web UI

Build the wasm UI with [trunk](https://trunkrs.dev) (needs the
`wasm32-unknown-unknown` Rust target), then let the engine serve it:

```sh
cd crates/app && trunk build --release
indicontrol-engine --listen 0.0.0.0:7700 --web-root crates/app/dist
# open http://telescope-pi.local:7700/
```

## Testing without hardware

The INDI simulators (`indi_simulator_ccd`, `indi_simulator_telescope`,
`indi_simulator_wheel`, `indi_simulator_focus`, `indi_simulator_guide`) ship
with libindi:

```sh
indiserver indi_simulator_telescope indi_simulator_ccd indi_simulator_wheel
```

`cargo test --workspace` runs unit tests plus end-to-end tests against a
scripted fake INDI server.
