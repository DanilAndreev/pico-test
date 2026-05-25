# CLAUDE.md

Guidance for working on this project. Read this before editing.

## What this is

A Rust firmware for the **Raspberry Pi Pico W** (RP2040 + CYW43439) that:

1. Brings up the onboard wireless chip as an **open WiFi access point** (`PicoBlink`).
2. Acts as a **captive portal** — phones auto-open a settings page on connect.
3. Serves an HTML settings UI + JSON HTTP API on `http://192.168.4.1/`.
4. Blinks the onboard LED at a configurable frequency (1–20 Hz).
5. Persists settings to the **last 4 KB sector of flash**, with a compile-time-embedded `default-settings.json` as the source of defaults.

Everything runs on **Embassy** (async, no_std, no alloc).

## Hardware target

- **Board:** Raspberry Pi Pico W (the wireless variant). The LED on this board is wired to **GPIO 0 of the CYW43439**, not GPIO 25 of the RP2040 — so you cannot blink the LED without bringing the wireless chip up. The non-W Pico is not supported by this codebase.
- **MCU:** RP2040 (dual-core Cortex-M0+, 264 KB SRAM, 2 MB flash chip W25Q16JV).
- **Compile target:** `thumbv6m-none-eabi`.
- **Flash chip:** W25Q080 family — this drives the `boot2-w25q080` feature selection on `embassy-rp`.

## Build / flash workflow

```sh
rustup target add thumbv6m-none-eabi      # one-time
cargo install elf2uf2-rs --locked         # one-time
# Hold BOOTSEL on the Pico, plug USB → RPI-RP2 drive mounts
cargo run --release                       # builds + flashes via elf2uf2-rs -d
```

After flashing, the Pico re-enumerates as a USB-CDC serial device:
```sh
ls /dev/tty.usbmodem*
screen /dev/tty.usbmodem<X> 115200        # USB CDC ignores baud, use any value
```

## File layout

```
pico-test/
├── .cargo/config.toml         # target thumbv6m-none-eabi, elf2uf2-rs runner
├── memory.x                   # RP2040 flash/RAM map; references BOOT2_FIRMWARE
├── build.rs                   # makes memory.x visible to linker
├── Cargo.toml
├── default-settings.json      # SOURCE OF TRUTH for settings defaults
├── cyw43-firmware/
│   ├── 43439A0.bin            # CYW43 main firmware (~225 KB, include_bytes!)
│   └── 43439A0_clm.bin        # Country/Locale Matrix (~1 KB, include_bytes!)
├── assets/
│   └── index.html             # settings UI; {{HZ}} placeholder gets substituted
└── src/
    ├── main.rs                # tasks + orchestration (see below)
    └── settings.rs            # SettingsStore: flash persistence + JSON
```

## Runtime architecture

Six task families on a single Embassy executor:

| Task | Count | Role |
|---|---|---|
| `cyw43_task` | 1 | Drives the PIO-SPI link to the wireless chip |
| `logger_task` | 1 | USB-CDC serial logger — routes `log::*!` macros to `/dev/tty.usbmodem*` |
| `net_task` | 1 | Runs the `embassy-net` smoltcp event loop |
| `dhcp_task` | 1 | UDP/67 DHCP server, single-client (`192.168.4.42`). Advertises captive portal URL via DHCP option 114. |
| `dns_task` | 1 | UDP/53. Answers every A query with `192.168.4.1` (captive portal hijack). |
| `http_task` | **4** | TCP/80 server, `pool_size = 4` so 4 sockets accept concurrently. iOS captive sheet opens parallel connections — single-socket model RSTs the extras and triggers "could not connect to server." |
| `blink_task` | 1 | Reads `settings.blink_half_period_ms()` (atomic, lock-free) and toggles CYW43 GPIO 0. |

### CYW43 / PIO pin map (Pico W internal wiring)
- `PIN_23` → WL_REG_ON (power)
- `PIN_24` → WL_DATA (PIO-SPI MOSI/MISO half-duplex)
- `PIN_25` → WL_CS
- `PIN_29` → WL_CLK
- `DMA_CH0` — used by `PioSpi`. If you need DMA elsewhere, use `DMA_CH1`+.

### Network layout
- AP SSID: `PicoBlink`, channel 5, **open** (no password).
- Pico static IP: `192.168.4.1/24`, gateway = self, DNS = self.
- Single DHCP lease: `192.168.4.42`.

## Settings system (`src/settings.rs`)

**Flash layout** — last 4 KB sector of the chip (offset `2 MB − 4 KB`):
```
+0..4   magic (u32 LE = 0xC0FFEE42)   ← distinguishes saved vs erased flash
+4..8   json_len (u32 LE)
+8..    JSON bytes
...     0xFF padding to 4096
```

**Boot path** (`SettingsStore::init`, synchronous, uses `blocking_*` flash methods):
1. Read 8-byte header; if magic mismatches → no stored data, seed from defaults.
2. Read JSON, deserialize. On success, use it.
3. On parse failure or load error → fall back to embedded defaults and write them so the next boot is clean.

**Partial JSON imports:** every field on `Settings` has `#[serde(default = "...")]`, and `PartialSettings` mirrors each one as `Option<T>` with `#[serde(default)]`. `Settings::merge(&PartialSettings)` only overwrites `Some(_)` fields. Unknown JSON fields are silently ignored.

**Runtime mirror:** `blink_half_period_ms: AtomicU32` is updated whenever settings change. The blink task reads this with `Relaxed` ordering — no async lock on the hot path. (Cortex-M0+ supports atomic load/store of u32 natively; only CAS is missing.)

### Adding a new setting

1. Add field on `Settings` with `#[serde(default = "default_<name>")]` + default fn.
2. Add mirror field on `PartialSettings` as `Option<T>` with `#[serde(default)]`.
3. Add merge clause in `Settings::merge`.
4. Add range check in `Settings::clamp` if needed.
5. Add the field to `default-settings.json`.
6. If it drives runtime behavior, mirror it in `SettingsStore::refresh_atomics`.

## HTTP API

| Route | Behavior |
|---|---|
| `GET /` | Settings page with current values templated in |
| `GET /set?hz=N` | Quick form-style update; redirects to `/` |
| `GET /api/settings` | Export current settings as `application/json` |
| `POST /api/settings` | Import partial JSON; missing fields preserved; `400` on malformed body |
| `POST /api/reset` | Restore from embedded `default-settings.json`; redirects to `/` |
| `GET /…` (any other) | Falls through to settings page — required for captive portal: probes like `GET /hotspot-detect.html` and `GET /generate_204` must succeed. |

**Body parsing caveat:** `serve()` reads from the socket exactly once into a 2 KB buffer. For small POST bodies (our JSON is < 100 bytes) this is reliable, but a multi-segment request would be truncated. If you add larger payloads, add a read-loop that respects `Content-Length`.

## Captive portal behavior

Three layers cooperate:

1. **DHCP option 114** — points clients at `http://192.168.4.1/`. Honored by iOS 14+ / Android 11+; banner pops up immediately on DHCP completion. Older clients ignore it and fall through to layer 2.
2. **DNS hijack** — every hostname resolves to `192.168.4.1`. So when the OS probes `captive.apple.com` / `connectivitycheck.gstatic.com`, the probe lands on us.
3. **HTTP probe response mismatch** — we return 200 + HTML at every URL. iOS expects `<HTML>...Success...</HTML>` for its probe; Android expects 204. Mismatch → OS concludes "captive" and pops the sheet.

**Why 4 HTTP tasks:** iOS's captive sheet opens 2–4 parallel TCP connections. With `pool_size = 1` the extras got RSTd, surfacing as "Hotspot login cannot open the page because it could not connect to the server." Don't reduce this without testing.

## Build constraints / gotchas

These have all been hit during initial setup. Don't undo the workarounds.

### Cortex-M0+ has no atomic CAS
- `portable-atomic` is a **direct dependency** with `features = ["critical-section"]`. This makes its compare-exchange use a critical section. `static_cell` and Embassy internals pull `portable-atomic` transitively; the feature must be set at the workspace root or those uses fail with `compare_exchange requires atomic CAS`.
- `embassy-rp`'s `critical-section-impl` feature provides the underlying critical-section using RP2040 hardware spinlocks (safe across both cores).

### `cargo test` won't work
- `Cargo.toml` has `[[bin]] test = false, bench = false`. The `test` crate is part of `std` and doesn't exist on `thumbv6m-none-eabi`. Implicit test harness builds (`cargo check --all-targets`, rust-analyzer) would fail without this.

### `embassy-rp` feature flags
The version we resolve to does **not** have a `rp2040` feature (chip selection wasn't split out yet in our published version). RP2040 is implicit. Required features:
- `time-driver`, `critical-section-impl`, `unstable-pac`, `rt`
- `boot2-w25q080` — the second-stage bootloader for the Pico W's flash chip. This populates the `BOOT2_FIRMWARE` symbol referenced by `memory.x`.

### Flash API in this `embassy-rp`
- Constructor is `Flash::<_, Blocking, FLASH_SIZE>::new_blocking(p.FLASH)` — not `Flash::new(...)` (the plain `new` is Async-mode only and needs a DMA channel).
- Methods are `blocking_read`, `blocking_erase`, `blocking_write`. They block the entire executor for ~30–50 ms per write — fine for occasional settings changes, but don't call them in hot loops.
- Erase granularity is 4 KB (`SECTOR_SIZE`); write granularity is 256 B (page). We write the whole sector at once (16 pages) to keep it simple.

### Task arena sizing
`embassy-executor` feature `task-arena-size-65536`. 4× HTTP tasks each capture ~3 KB of socket buffers in their futures. At 32 KB the arena ran tight; 64 KB has comfortable margin.

### Single-core executor
Embassy on RP2040 by default uses **one** of the two M0+ cores. Tasks don't need to be `Send`. Don't add `embassy_executor::Spawner` sharing tricks that assume multi-core unless you also bring up the second core deliberately.

## Common failure modes

| Symptom | Likely cause |
|---|---|
| LED stays dark, no USB CDC device | CYW43 firmware load failed. Check `cyw43-firmware/*.bin` are present and that the `panic-halt` haven't silenced an earlier panic. |
| WiFi visible, can't connect | AP started before `embassy-net` stack — order matters. `control.start_ap_open` must precede `Stack::new`. |
| Connects, no IP | DHCP server crashed at bind. Check serial log for `dhcp: bind(67) failed`. UDP socket pool may be exhausted — bump `StackResources<N>`. |
| iOS "could not connect to the server" | Single HTTP task RSTing parallel connections. Verify `pool_size = 4` and 4× `spawner.spawn(http_task(...))`. |
| Settings don't persist across reboots | Flash write failed silently. Check serial for `settings: persisted (N bytes)` after each change. |
| Build error `can't find crate for 'test'` | The `[[bin]] test = false` flag got removed. Add it back. |
| Build error `embassy-rp does not have feature 'rp2040'` | Someone tried to update feature flags to a newer embassy-rp shape. This published version uses `boot2-*` chip selection, not `rp2040`/`rp235xa`. |

## Things this project deliberately does NOT do

- **HTTPS/TLS** — would need `embedded-tls` + cert + significant flash/RAM. Captive portals work fine over HTTP.
- **WiFi station mode** — AP-only, intentional. Users connect to the Pico's SSID, not the other way around.
- **Heap allocation** — no `alloc` crate. All buffers are fixed-size on the stack or in `static`s. Adding `alloc` is possible (`embedded-alloc` or `linked_list_allocator`) but the current code doesn't need it.
- **Multi-client DHCP** — the server hands the same `192.168.4.42` to whoever asks. Fine for a single-user captive scenario; would need a real lease table for multi-client.
- **DNSSEC / proper DNS** — every query gets a hardcoded A record. Not a real resolver.
