//! Persistent settings store.
//!
//! - Settings live in the **last 4 KB sector** of flash. Layout inside the
//!   sector is `[magic: u32 LE][json_len: u32 LE][json bytes...][0xFF padding]`.
//! - On boot, [`SettingsStore::init`] tries to load that sector. If the magic
//!   doesn't match (fresh flash, corruption, etc.) it falls back to the
//!   compile-time defaults baked in from `default-settings.json`, and writes
//!   them so subsequent boots are stable.
//! - JSON imports are *partial*: any field missing from the incoming JSON
//!   is left untouched. This is implemented via [`PartialSettings`] whose
//!   fields are all `Option<T>` with `#[serde(default)]`.
//! - A blink half-period mirror lives in an `AtomicU32` so the blink task
//!   doesn't have to acquire the async mutex on every transition.

use core::sync::atomic::{AtomicU32, Ordering};

use embassy_rp::flash::{Blocking, Flash};
use embassy_rp::peripherals::FLASH;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use serde::{Deserialize, Serialize};

// ---------- constants ----------

/// Total flash size on the standard Pico W (W25Q16JV = 2 MB).
const FLASH_SIZE: usize = 2 * 1024 * 1024;
const SECTOR_SIZE: u32 = 4096;
/// Offset of the settings sector from start of flash.
const SETTINGS_OFFSET: u32 = (FLASH_SIZE as u32) - SECTOR_SIZE;
/// Stamped at the start of the record so we can detect "fresh / erased" flash.
const MAGIC: u32 = 0xC0FFEE42;
/// Upper bound on serialized settings JSON.
const MAX_JSON: usize = 1024;

/// Defaults baked into the firmware at compile time.
/// Changing `default-settings.json` triggers a rebuild and re-embed.
pub const DEFAULT_SETTINGS_JSON: &str = include_str!("../default-settings.json");

pub type FlashDriver = Flash<'static, FLASH, Blocking, FLASH_SIZE>;

// ---------- model ----------

/// User-facing settings. Add new fields here.
///
/// Every field must:
/// - have a `#[serde(default = "...")]` attribute so partial deserialization
///   (e.g. of `default-settings.json` with missing entries) still succeeds,
/// - have a corresponding `Option<...>` field in [`PartialSettings`] and a
///   merge clause in [`Settings::merge`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Settings {
    /// Onboard LED blink frequency in Hz. Clamped to 1..=20 at apply time.
    #[serde(default = "default_blink_hz")]
    pub blink_hz: u32,
}

fn default_blink_hz() -> u32 {
    1
}

impl Default for Settings {
    /// Defaults come from the embedded `default-settings.json`. If that
    /// somehow fails to parse (shouldn't happen), fall back to safe constants.
    fn default() -> Self {
        match serde_json_core::from_str::<Self>(DEFAULT_SETTINGS_JSON) {
            Ok((s, _)) => s,
            Err(_) => Self { blink_hz: 1 },
        }
    }
}

/// All fields optional — used for partial JSON imports.
#[derive(Deserialize, Default)]
struct PartialSettings {
    #[serde(default)]
    blink_hz: Option<u32>,
}

impl Settings {
    /// Apply only the `Some(_)` fields from `p`; leave the rest untouched.
    fn merge(&mut self, p: &PartialSettings) {
        if let Some(v) = p.blink_hz {
            self.blink_hz = v;
        }
    }

    /// Clamp every field to its valid range. Called after merge / import.
    fn clamp(&mut self) {
        self.blink_hz = self.blink_hz.clamp(1, 20);
    }
}

// ---------- errors ----------

#[derive(Debug)]
pub enum SettingsError {
    /// JSON serialization or deserialization failed.
    Json,
    /// Flash IO failed.
    Flash,
    /// Stored payload claims to be larger than `MAX_JSON`.
    TooLarge,
    /// No valid record in flash (first boot or corruption).
    NoStoredData,
}

impl From<embassy_rp::flash::Error> for SettingsError {
    fn from(_: embassy_rp::flash::Error) -> Self {
        Self::Flash
    }
}

impl From<serde_json_core::de::Error> for SettingsError {
    fn from(_: serde_json_core::de::Error) -> Self {
        Self::Json
    }
}

// ---------- store ----------

pub struct SettingsStore {
    flash: Mutex<CriticalSectionRawMutex, FlashDriver>,
    data: Mutex<CriticalSectionRawMutex, Settings>,
    /// Mirror of `derive_half_period(data)` for cheap lock-free reads
    /// from the blink task.
    blink_half_period_ms: AtomicU32,
}

impl SettingsStore {
    /// Read flash, fall back to defaults if no valid record. Always returns
    /// a usable store.
    pub fn init(mut flash: FlashDriver) -> Self {
        let initial = match Self::load(&mut flash) {
            Ok(mut s) => {
                s.clamp();
                log::info!("settings: loaded from flash: blink_hz={}", s.blink_hz);
                s
            }
            Err(SettingsError::NoStoredData) => {
                let mut d = Settings::default();
                d.clamp();
                log::info!(
                    "settings: no stored data, seeding from defaults: blink_hz={}",
                    d.blink_hz
                );
                if let Err(e) = Self::store(&mut flash, &d) {
                    log::warn!("settings: initial persist failed: {:?}", e);
                }
                d
            }
            Err(e) => {
                let mut d = Settings::default();
                d.clamp();
                log::warn!("settings: load failed ({:?}), using defaults", e);
                let _ = Self::store(&mut flash, &d);
                d
            }
        };

        let half = derive_half_period(&initial);
        Self {
            flash: Mutex::new(flash),
            data: Mutex::new(initial),
            blink_half_period_ms: AtomicU32::new(half),
        }
    }

    /// Lock-free read for the blink task hot path.
    pub fn blink_half_period_ms(&self) -> u32 {
        self.blink_half_period_ms.load(Ordering::Relaxed)
    }

    /// Snapshot of the current settings.
    pub async fn current(&self) -> Settings {
        self.data.lock().await.clone()
    }

    /// Convenience: set just the blink frequency (clamped) and persist.
    pub async fn set_blink_hz(&self, hz: u32) -> Result<(), SettingsError> {
        let mut data = self.data.lock().await;
        data.blink_hz = hz;
        data.clamp();
        self.refresh_atomics(&data);
        let mut flash = self.flash.lock().await;
        Self::store(&mut *flash, &data)
    }

    /// Merge a partial JSON document into the current settings and persist.
    /// Missing fields are left untouched; unknown fields are silently ignored
    /// by serde unless `#[serde(deny_unknown_fields)]` is added.
    pub async fn import_json(&self, json: &[u8]) -> Result<(), SettingsError> {
        let (partial, _): (PartialSettings, _) = serde_json_core::from_slice(json)?;
        let mut data = self.data.lock().await;
        data.merge(&partial);
        data.clamp();
        self.refresh_atomics(&data);
        let mut flash = self.flash.lock().await;
        Self::store(&mut *flash, &data)
    }

    /// Restore from the compile-time embedded defaults and persist.
    pub async fn reset(&self) -> Result<(), SettingsError> {
        let mut defaults = Settings::default();
        defaults.clamp();
        let mut data = self.data.lock().await;
        *data = defaults;
        self.refresh_atomics(&data);
        let mut flash = self.flash.lock().await;
        Self::store(&mut *flash, &data)
    }

    /// Serialize current settings into `buf`. Returns the populated slice.
    pub async fn export_json<'a>(
        &self,
        buf: &'a mut [u8],
    ) -> Result<&'a [u8], SettingsError> {
        let data = self.data.lock().await;
        let n = serde_json_core::to_slice(&*data, buf).map_err(|_| SettingsError::Json)?;
        Ok(&buf[..n])
    }

    fn refresh_atomics(&self, data: &Settings) {
        self.blink_half_period_ms
            .store(derive_half_period(data), Ordering::Relaxed);
    }

    // ----- low-level flash IO -----

    fn load(flash: &mut FlashDriver) -> Result<Settings, SettingsError> {
        let mut header = [0u8; 8];
        flash.blocking_read(SETTINGS_OFFSET, &mut header)?;
        let magic = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        if magic != MAGIC {
            return Err(SettingsError::NoStoredData);
        }
        let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        if len == 0 || len > MAX_JSON {
            return Err(SettingsError::TooLarge);
        }
        let mut json = [0u8; MAX_JSON];
        flash.blocking_read(SETTINGS_OFFSET + 8, &mut json[..len])?;
        let (s, _) = serde_json_core::from_slice::<Settings>(&json[..len])?;
        Ok(s)
    }

    fn store(flash: &mut FlashDriver, data: &Settings) -> Result<(), SettingsError> {
        let mut json_buf = [0u8; MAX_JSON];
        let n = serde_json_core::to_slice(data, &mut json_buf).map_err(|_| SettingsError::Json)?;
        if n > MAX_JSON {
            return Err(SettingsError::TooLarge);
        }

        // Erase the whole sector — RP2040 flash erases in 4 KB units.
        flash.blocking_erase(SETTINGS_OFFSET, SETTINGS_OFFSET + SECTOR_SIZE)?;

        // Build a page-aligned 4 KB payload (writes must be a multiple of 256 B).
        let mut payload = [0xFFu8; SECTOR_SIZE as usize];
        payload[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        payload[4..8].copy_from_slice(&(n as u32).to_le_bytes());
        payload[8..8 + n].copy_from_slice(&json_buf[..n]);

        flash.blocking_write(SETTINGS_OFFSET, &payload)?;
        log::info!("settings: persisted ({n} bytes)");
        Ok(())
    }
}

fn derive_half_period(settings: &Settings) -> u32 {
    let hz = settings.blink_hz.clamp(1, 20);
    500 / hz
}
