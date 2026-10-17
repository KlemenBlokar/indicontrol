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
