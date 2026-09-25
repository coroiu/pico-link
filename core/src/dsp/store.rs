//! The global preset store: never-reused ids and active-preset
//! resolution (design sec 2.2: "IDS ARE MONOTONIC AND NEVER REUSED ...
//! At boot, `next_id = max(stored) + 1`, starting at 1"; sec 2.3: "Both 0
//! and an unknown id mean Off; `core` resolves this, not C").
//!
//! This is the in-memory model a future `pico-link-ryw.6` loads from
//! `PL:P:<slot>` records at boot and persists back to. Nothing here
//! touches flash or FFI -- see [`super`]'s module doc.

use alloc::collections::BTreeMap;

use super::preset::Preset;

/// `0` is reserved to mean "no preset" (design sec 2.3), matching the
/// existing device-record `preset_id` convention (`persist.c:101`: "`0`
/// means none"). The allocator never hands this id out.
pub const NO_PRESET_ID: u16 = 0;

/// Holds every loaded/created preset, keyed by its never-reused id.
/// Deleting a preset removes its entry but never reuses its id (design
/// sec 2.2/2.4): a device record still holding that id simply resolves to
/// [`PresetStore::resolve`] returning `None` (Off), by construction, not
/// by any explicit "is this id still valid" check the device side has to
/// remember to make.
#[derive(Debug, Clone, Default)]
pub struct PresetStore {
    presets: BTreeMap<u16, Preset>,
    next_id: u16,
}

impl PresetStore {
    /// An empty store. `next_id` starts at `1` (design sec 2.2: "starting
    /// at 1") -- id `0` is [`NO_PRESET_ID`] and never allocated.
    #[must_use]
    pub fn new() -> Self {
        Self { presets: BTreeMap::new(), next_id: 1 }
    }

    /// Rebuilds a store from records already loaded from flash (a future
    /// `pico-link-ryw.6`'s boot path: "load every valid `PL:P` record ...
    /// push `PresetLoaded{id, blob}` for each", design sec 2.2). `next_id`
    /// is recomputed as `max(stored ids) + 1`, exactly matching the
    /// design's boot-time allocator seed regardless of insertion order or
    /// gaps left by earlier deletions.
    #[must_use]
    pub fn from_loaded(presets: impl IntoIterator<Item = (u16, Preset)>) -> Self {
        let presets: BTreeMap<u16, Preset> = presets.into_iter().filter(|(id, _)| *id != NO_PRESET_ID).collect();
        let next_id = presets.keys().max().copied().unwrap_or(0).saturating_add(1).max(1);
        Self { presets, next_id }
    }

    /// Inserts (or overwrites) `preset` under an EXPLICIT `id`, bumping
    /// `next_id` past it if needed -- the incremental counterpart to
    /// [`Self::from_loaded`]'s batch constructor, for a caller that folds
    /// records in one at a time rather than collecting them first (bead
    /// `pico-link-ryw.5`: C's boot push is `count` x one-record-at-a-time
    /// [`Event::PresetLoaded`](crate::app::Event::PresetLoaded), and the
    /// SAME event is also a live [`Command::SavePreset`](crate::app::Command::SavePreset)
    /// echo arriving well after boot -- both cases need "insert this one
    /// id now," not "rebuild the whole store"). `id == 0`
    /// ([`NO_PRESET_ID`]) is a no-op: C is never expected to send it (its
    /// own allocator starts at 1, same as this store's `next_id`), and
    /// accepting it would let a preset alias the sentinel "no preset
    /// assigned" value.
    pub fn load(&mut self, id: u16, preset: Preset) {
        if id == NO_PRESET_ID {
            return;
        }
        self.presets.insert(id, preset);
        self.next_id = self.next_id.max(id.saturating_add(1)).max(1);
    }

    /// Allocates a fresh, never-before-used id and inserts `preset` under
    /// it. Returns the allocated id (design sec 3.2: `PresetLoaded` "is
    /// also the save echo and carries the allocated id").
    pub fn create(&mut self, preset: Preset) -> u16 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1).max(1); // never wrap back to 0
        self.presets.insert(id, preset);
        id
    }

    /// Overwrites an existing preset's contents in place -- the id is
    /// NOT reallocated. Returns `false` (no-op) if `id` isn't a live
    /// preset, so a caller can distinguish "updated" from "id no longer
    /// exists" without this module needing to know why (e.g. it was
    /// deleted in a race).
    pub fn update(&mut self, id: u16, preset: Preset) -> bool {
        if let Some(slot) = self.presets.get_mut(&id) {
            *slot = preset;
            true
        } else {
            false
        }
    }

    /// Removes a preset. Its id is never reallocated by
    /// [`Self::create`] (`next_id` only ever increases) -- see
    /// [`Self`]'s doc comment on why a dangling device reference to this
    /// id is safe rather than aliased. Returns the removed preset, if any.
    pub fn delete(&mut self, id: u16) -> Option<Preset> {
        self.presets.remove(&id)
    }

    #[must_use]
    pub fn get(&self, id: u16) -> Option<&Preset> {
        self.presets.get(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (u16, &Preset)> {
        self.presets.iter().map(|(id, preset)| (*id, preset))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.presets.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.presets.is_empty()
    }

    /// Resolves a device's `preset_id` to the [`Preset`] it should use.
    /// Both [`NO_PRESET_ID`] (`0`, "no preset assigned") and any id this
    /// store doesn't hold (a dangling reference -- the preset was
    /// deleted, design sec 2.4: "Dangling ids read as Off") resolve to
    /// `None`, meaning Off. The caller (a future `pico-link-ryw.6`) never
    /// has to special-case which of those two reasons applied -- both are
    /// exactly the same "nothing to play" outcome.
    #[must_use]
    pub fn resolve(&self, device_preset_id: u16) -> Option<&Preset> {
        if device_preset_id == NO_PRESET_ID {
            return None;
        }
        self.presets.get(&device_preset_id)
    }
}
