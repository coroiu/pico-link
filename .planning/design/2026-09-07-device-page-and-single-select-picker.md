# The device page and the generic single-select picker: what to build, and what already exists

Bead `pico-link-7jol.4`, under epic `pico-link-7jol`. Author: Fern (frontend
architect), 2026-09-07. **Design of record for this bead.** Ruby implements
against this document; it does not re-decide anything settled in the four
documents it depends on.

Depends on / does not re-open:

- `.planning/design/2026-09-02-device-page.md` — Uma. The page's rows, copy,
  colours, entry/exit and rail. **Unchanged by this document.**
- `.planning/design/2026-09-02-field-list-widget-ruling.md` — my own prior
  ruling. It is the reason there is almost nothing to build in `render/`.
- `.planning/design/2026-09-07-ldac-quality-selector.md` — Uma. The picker
  contract: `A` applies live, the picker stays open, the check follows the
  **stored echo**, `B` is Back.
- `.planning/design/2026-09-02-device-page-seam.md` — Ada. The commands and
  events. **None of it is implemented yet** (verified: no `SetDeviceCodecPref`,
  no `CodecAvailability`, no `A2dpStreamStateChanged` anywhere in
  `core/`, `ui-ffi/` or `firmware/src/`).

---

## 0. The verdict, since the bead asks for it before anything else

> **The widget layer is pure composition. There is nothing to build in
> `core/src/render/`. The one thing that genuinely does not exist — and the
> reason `pico-link-7jol.3`'s UI half could not be built — is a way to keep a
> *pushed* screen live-synced to the model. That is a `Navigator`/`App`
> capability, not a widget capability.**

### 0.1 What already exists, and was built for exactly this

`core/src/render/fields.rs` (761 lines, shipped, tested, **zero production
callers**) is the device page and the picker, already designed against both:

| Requirement | Already in the code |
|---|---|
| Focusable row that refuses activation | `FieldKind::Readonly`, gate in `FieldList::on_focus`'s `Activated` arm (`fields.rs:330`) |
| Value *and* caret on the same row | `RowTrailing { value, caret }` (`fields.rs:409`) |
| Scrolling | `list::reconcile_top_index`, reused verbatim (`fields.rs:437`) |
| Per-row trailing font (the `ADDRESS` row) | `ValueFont::{Normal,Small}` |
| Leading check gutter for pickers | `RowStyle::FIELD_GUTTERED` + `CHECK_GUTTER_WIDTH` (`menu.rs:103`), `FieldList::with_leading_gutter`, `FieldRow::with_leading_glyph` |
| The check glyph itself | `theme::icon::CHECK` (`theme.rs:274`) |
| 12/12 alignment-grid margins | `RowStyle::FIELD` (`menu.rs:100`) |
| Caret never moves the value | `const _: () = assert!(...)` (`menu.rs:112`) |
| Red `Forget this device` label | `FieldRow::with_label_color` |
| Focus survives a rebuild that adds/removes rows | `with_selected_identity` + `with_scroll_top` |
| A-rail word, and "A does nothing on an inert row" made structural | `FieldList::activation` (`fields.rs:360`) |
| A correct `paint_key` folding every drawn field | `fields.rs:381` |
| The 7-row and scroll-peek budgets | `fields.rs` tests, already asserted |

**So: no new widget, no new row style, no second row-drawing path, no new
render primitive.** A reviewer who sees a new file under `core/src/render/` in
this bead's diff should reject it and ask which of the rows above was wrong.

### 0.2 The one capability that is missing

`App::rebuild_root` (`app.rs:1425`) refreshes the root, then refreshes exactly
one pushed screen, identified by **title string match**:

```rust
if self.navigator.title_at(1) == Some(DEVICES_TITLE) { ... }
```

That hack cannot carry the device page:

1. **The device page's title is the device's name** — variable, user-supplied,
   and two devices can share it. There is no constant to match against.
2. **The picker is at depth 2**, and the branch is hardcoded to index 1.
3. **The parent must refresh too.** Picking `660 kbps` changes both the picker's
   check *and* the `QUALITY` row underneath it. Refreshing only the top screen
   leaves a stale device page waiting under the user's `B`.
4. Uma's central contract — *the check follows the stored echo, not the press* —
   **is** "a pushed screen re-renders from the model when an event lands." There
   is no way to honour it today short of an optimistic check, which her §5.1
   explicitly forbids.

Adding a second and third title-match branch is the quick fix. It is the wrong
one: the identity is not a title, and the loop is not two indices.

---

## 1. `ScreenId`: identity for live-rebuildable screens

```rust
// core/src/app.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenId {
    Home,
    Devices,
    DevicePage(DeviceAddr),
    Picker(PickerKind, DeviceAddr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind { Codec, LdacQuality }
```

```rust
// core/src/render/screen.rs
impl Screen {
    pub fn with_id(mut self, id: ScreenId) -> Self { self.id = Some(id); self }
    pub fn id(&self) -> Option<ScreenId> { self.id }
}

// core/src/render/navigator.rs
impl Navigator {
    pub fn id_at(&self, index: usize) -> Option<ScreenId>;
    /// Drops every screen above `index`. `index == 0` is `pop_to_root`.
    pub fn truncate_to(&mut self, index: usize);
}
```

**`Option<ScreenId>`, and `None` means "never refresh me."** The wizard, the
confirm views and Settings stay unidentified, so the change is
behaviour-preserving for every screen except the two it exists for. That is
also the honest model: a confirm dialog whose subject vanished should not
silently re-render with new text under the user's finger.

**On `render` depending on `app`:** `Screen` importing `crate::app::ScreenId` is
a second instance of a dependency `render/widget.rs` already has
(`use crate::app::LinkState`, `widget.rs:31`). I considered an app-agnostic
opaque token (`ScreenKey(u64)` minted by the app) to avoid it. Rejected: it
makes every call site unreadable and every test assert on a magic number, to
purchase a boundary the module already crosses. If Ada wants that boundary
restored, it is one refactor of two imports, and it should be its own bead.

### 1.1 `rebuild_root` becomes `refresh_stack`

```rust
struct ScreenCarry {
    selected_key: Option<ListItemKey>,
    selected_index: usize,
    scroll_top: Option<usize>,
}

enum Refresh {
    /// Replace the screen at this index with this one.
    Rebuild(Screen),
    /// This screen's subject no longer exists — drop it and everything above it.
    Gone,
}

fn refresh_stack(&mut self) {
    let mut truncate_at: Option<usize> = None;
    for index in 0..self.navigator.depth() {
        let Some(id) = self.navigator.id_at(index) else { continue };
        let carry = ScreenCarry {
            selected_key: self.navigator.selected_key_at(index),
            selected_index: self.navigator.selected_index_at(index).unwrap_or(0),
            scroll_top: self.navigator.scroll_top_at(index),
        };
        match self.build_identified_screen(id, &carry) {
            Refresh::Rebuild(screen) => self.navigator.replace_at(index, screen),
            Refresh::Gone => { truncate_at = Some(index); break; }
        }
    }
    if let Some(index) = truncate_at { self.navigator.truncate_to(index.saturating_sub(1)); }
    self.dirty = true;
}
```

`build_identified_screen(id, carry) -> Refresh` is **one match, one place** —
the only mapping from screen identity to builder in the app. Every existing
call site of `rebuild_root` (~15 event handlers) calls `refresh_stack` instead,
unchanged in every other respect.

**`Refresh::Gone` is not a nicety.** It is how "Forget pops two levels"
(device-page §3.7) becomes structural rather than a hand-written double pop in
the confirm's callback: the `PairedDeviceForgotten` echo removes the device from
`model.paired`, `build_identified_screen` cannot build a page for it, and the
stack unwinds to Devices — from *either* route, including the case where the
device disappears for a reason the user did not initiate.

---

## 2. Contract: the single-select picker

One free function in `app.rs`. **Not** a widget, **not** a `render/` module.

```rust
pub(crate) struct PickerOption {
    /// Stable identity — carries focus and the check across rebuilds.
    pub key: ListItemKey,
    pub label: String,
    /// The trailing note: `best audio`, `660 now`, `not offered`, `SBC now`.
    pub note: Option<(String, Rgb565)>,
    /// `false` -> `FieldKind::Readonly`: focusable, dim, no caret, A dead.
    pub selectable: bool,
}

fn build_single_select_screen(
    id: ScreenId,
    title: impl Into<String>,
    options: Vec<PickerOption>,
    /// The row the check sits on, READ FROM THE MODEL. Never from the press.
    checked: Option<ListItemKey>,
    carry: &ScreenCarry,
    on_pick: impl Fn(ListItemKey) -> Action + 'static,
) -> Screen
```

Body, in full:

```rust
let rows = options.iter().map(|o| {
    let mut row = if o.selectable { FieldRow::action(&o.label) } else { FieldRow::readonly(&o.label) };
    if o.selectable { row = row.with_verb(Verb::Select); }
    if let Some((text, color)) = &o.note { row = row.with_value(text, *color); }
    if Some(o.key) == checked { row = row.with_leading_glyph(icon::CHECK); }
    row.with_key(o.key)
}).collect();

let list = FieldList::new(rows)
    .with_leading_gutter()
    .with_selected_identity(carry.selected_key, carry.selected_index)
    .on_activate_index(move |i| options_keys[i].map_or(Action::None, &on_pick));
let list = carry.scroll_top.map_or(list, |t| list.with_scroll_top(t));

Screen::new(title, vec![Box::new(list)]).with_id(id)
```

### 2.1 Five rules this shape makes structural, not remembered

1. **The check follows the stored value.** `checked` is a parameter derived from
   the model by the caller inside `build_identified_screen`. There is no local
   "pressed" state anywhere in the picker to make optimistic by accident.
   Uma's §5.1 is enforced by the absence of a place to put the bug.
2. **A picker that pops and a picker that stays open are the same function.**
   The difference is entirely `on_pick`'s return value: `Action::None` stays
   open (LDAC quality), `Action::PopView` pops (codec pin). Uma's rule — *"a
   picker pops when its effect cannot be evaluated in place, and stays when it
   can"* — is expressed as one return value, not as a picker mode flag. **Do not
   add a `stays_open: bool`.**
3. **The gutter is on the list, not the row**, so labels align at
   `L = 12 + 12` whether checked or not, and an unchecked row cannot silently
   shift left.
4. **An unavailable option cannot be picked.** `selectable: false` produces
   `FieldKind::Readonly`, whose activation gate lives in `FieldList`, not in
   `on_pick`. A caller that forgets a check in its closure still cannot activate
   a dim row.
5. **A never lies.** `Verb::Select` on selectable rows, no verb (dim A) on
   unavailable ones, both from `FieldList::activation`.

### 2.2 The `A` rail word

`Verb::Select` renders `select`. Uma's ASCII sketches show `pick`, which is not
in the `Verb` vocabulary and would need `Verb::Exception("pick")` — and adding a
second `Verb::Exception` is explicitly a UX decision, not an implementation one
(`widget.rs`'s `Verb::Exception` doc). **Ruling for this bead: use
`Verb::Select`.** `select` is 6 chars, exactly the measured budget, and its
doc comment already reads *"Commits the focused row's choice on this screen"* —
written for this screen. If Uma wants `pick`, that is a one-line follow-up and
needs her, not Ruby.

### 2.3 Marking dirty on the local press — already handled

Uma's §9 item 5 and device-page §3.1.1 both tell Ruby the `A` handler must mark
the app dirty. **It already does:** `App::handle_input` sets `self.dirty = true`
unconditionally after dispatching any intent batch (`app.rs:2038`). Ruby must
**not** add a second dirty-marking path inside the picker callback; there is
nothing to fix.

---

## 3. Contract: the device page

```rust
fn build_device_page_screen(
    model: &BtModel,
    addr: DeviceAddr,
    carry: &ScreenCarry,
    commands: &Rc<RefCell<VecDeque<Command>>>,
) -> Refresh
```

Returns `Refresh::Gone` when `addr` is no longer in `model.paired`.

Two pieces:

```rust
/// PURE. Model in, rows out. Every row-presence rule (device-page §3,
/// quality-selector §4.2) is testable here with no Screen, no Navigator
/// and no framebuffer.
fn device_page_rows(model: &BtModel, addr: DeviceAddr) -> Vec<FieldRow>;

/// The wrapper that owns the X binding. `DevicesListView` (app.rs:1141) is
/// the precedent to copy, method for method.
struct DevicePageView { list: FieldList, addr: DeviceAddr, connected: bool, commands: ... }
```

### 3.1 The wrapper's forwarding obligations — the documented hazard

`DevicePageView` wraps a `FieldList` and therefore **must forward** every
method whose default silently looks like "no opinion" instead of "forgotten":
`measure`, `render`, `is_focusable`, `on_focus`, `on_intent` (after its own
`ShortcutX` arm), `activation`, `selected_index`, `selected_key`, `scroll_top`,
`paint_key`, `redraw_after`. This is `widget.rs`'s stated wrapper hazard
(pico-link-vxc D2), and `DevicesListView` already demonstrates the exact shape.
A test should assert `scroll_top`/`selected_key` round-trip through the wrapper.

### 3.2 The X binding, and it must not collide with the fault strip

- **Device page:** `X` = `drop` when this device is connected, `link` when it is
  not (device-page §2.1's amendment). Emitted from
  `DevicePageView::on_intent`'s `ShortcutX` arm, labelled via
  `chrome_contribution().x`. Both states are labelled and both are real.
- **Home is a different screen and is not in this bead.** For the record, so
  `pico-link-9eq2.2` and `pico-link-7jol.5` cannot collide: **X on Home's
  *status* face is the fault strip's `why?` (live only while the strip is
  non-empty); X on Home's *menu* face is "open the connected device's page."**
  Two faces, two bindings, no contest. Today both are inert
  (`home.rs:347` returns `Action::None` for `ShortcutX`), so neither has shipped
  and neither is blocked. If Uma disagrees with the face split, it is her call
  and it is cheap to change while both are unbuilt.

### 3.3 The top gutter is the one real layout gap

Uma's §5 puts a **12px top gutter** between the title bar and row 1 (rows start
at `y = 28`), and `fields.rs`'s own budget tests already assume it
(`224 - 12 = 212`). **`Screen` has no padding or spacer concept** — it stacks
widgets from the top of the content region, so a `FieldList` placed alone would
start at `y = 16` and every row would sit 12px above where the design and the
tests say.

Cheapest honest fix, and it is general: a **`Spacer`** in
`core/src/render/spacer.rs` — a non-focusable widget whose `measure` returns
`Size::new(constraints.width, self.height)`, whose `render` draws nothing, and
whose `paint_key` is a constant folding only its height. It is safe to draw
nothing because `Screen::render` already fills the damage rect with
`palette::BACKGROUND` before drawing widgets (`screen.rs:676`). ~25 lines.

Rejected alternative: `FieldList::with_top_inset(12)`. It puts the page's
layout inside the row container, so the next screen wanting a gutter either
repeats the option or invents a second one, and the inset would have to be
folded into `FieldList::paint_key` for no pixels of its own.

**This is the only new file this bead may add**, and it is in `render/` only
because a spacer is chrome-layout vocabulary, not app vocabulary.

### 3.4 Rows Ruby can build today vs rows that must wait

`device_page_rows` is where the seam's absence shows. **Every row whose value
needs a signal Ada has designed but nobody has implemented renders its
honest degraded form, per device-page §3.0** — a live field dashes, a stored
setting does not:

| Row | Today | Needs |
|---|---|---|
| `CODEC` | value from `model.connected_codec` when connected, else `Automatic` | `PairedDeviceUpserted.codec_id` for the real pin; `pin_outcome` for the amber |
| `QUALITY` | **absent until 7jol.5** | seam + `ldac_quality` |
| `SAMPLE RATE` | `—` | `CodecChanged.sample_rate_hz` (seam §2.3, ABI 4→5) |
| `USB IN` | `—` | its own event (seam §2.5) |
| `A2DP` | `—` | `A2dpStreamStateChanged` (seam §2.6) |
| `ADDRESS` | real, `with_small_value()` | nothing |
| `Forget this device` | real, red, pushes the existing `build_forget_confirm_screen` | nothing |
| The `Saves when playback stops` note row | **absent** | the seam's write-state signals |

That is a page with two real rows and four dashes. **It is still worth
building**, because it is truthful (the dash is what an em-dash means), because
`7jol.5` needs the page to exist to put `QUALITY` on it, and because every dash
becomes real by editing one pure function with no structural change.

---

## 4. Input model

Nothing new. The existing `NavIntent` vocabulary covers the whole feature, and
the press-edge-only constraint is what makes the chosen shapes right rather than
merely acceptable.

| Intent | Device page | Picker |
|---|---|---|
| `Up`/`Down`/`JumpBy` | move focus, **never skipping a Readonly row** (`FieldList::on_intent`) | same |
| `Select` (joystick press or A) | activate the focused row if `Action`; guaranteed no-op if `Readonly` | apply the pick live; **stack unchanged** |
| `Back` (B) | pop to Devices | pop to the device page |
| `ShortcutX` | `drop` / `link` | **unbound and unlabelled** |
| `ShortcutY` | unbound and unlabelled | unbound and unlabelled |
| `Left`/`Right` | no-op | no-op |

Two consequences of press-edge-only worth restating because they are load-bearing:

- **Focus must land on every row.** A cursor that skips inert rows moves by an
  amount the user cannot predict when there is no key repeat to correct with.
  Already enforced in `FieldList::on_intent`.
- **`A` must never mean two things on one screen.** This is why the quality
  setting is a submenu and not a cycling field (quality-selector §3): on the
  device page `A` already means "descend", and there is no second gesture to
  disambiguate an in-place cycle with.

---

## 5. Damage keys: how these screens participate correctly

The project has already shipped one bug in this exact area (a damage key that
folded an undrawn value and silently reinstated full repaints while every test
passed). The rules for this bead:

1. **`FieldList::paint_key` is already correct and must not be touched.** It
   folds every drawn field including the leading glyph and the style's gutter
   width, and excludes `verb`/`key`, which are not pixels.
2. **`DevicePageView::paint_key` folds its child and nothing else:**
   `PaintKey::of(SEED).fold_key(self.list.paint_key(ctx))`. In particular it
   must **not** fold `connected`, `LinkState`, or the X label. The rail has its
   own key (`screen.rs:rail_paint_key`) fed by the already-resolved
   `ButtonLabel`s, so chrome is covered; folding link state into the *body* key
   would repaint the body on every link event for zero changed body pixels —
   precisely the "fold something you don't draw" defect.
3. **Everything the body actually shows reaches the key through the row text**,
   because the screen is rebuilt from the model. No extra fold is needed or
   wanted.
4. **`redraw_after` is `None` on both screens** — nothing here animates. Per the
   mechanical review rule, neither screen may fold time into a paint key.
   `FieldList` already does neither, correctly.
5. **Known cost, deliberately not fixed here:** `Navigator::replace_at` sets
   `force_full_damage = true` and the incoming `Screen` starts with an empty
   `paint_cache`, so **every model-driven rebuild is a full-frame repaint**.
   That is already true of Home today; `refresh_stack` does not make it worse
   per event, it just makes more screens subject to it. On the device page under
   Adaptive the live number updates as ABR steps, which is exactly when the
   encoder is starving core 0. If that measures badly, the fix is to transplant
   the outgoing screen's `paint_cache` into the replacement when the widget
   count and layout are identical — **a render-pipeline bead of its own, not
   this one.** Do not smuggle it in.

---

## 6. The 240x240 budget, and the OUT-meter question

The bead asks whether the vertical OUT meter's 48px carve applies here.

**It does not.** `METER_STRIP_WIDTH = 48` is carved *inside*
`HeroStatusView::render` (`hero.rs:126`, `hero.rs:576`), against the area Home's
status face was handed. It is not in `chrome.rs` and no other screen sees it.
The device page and the picker get the full content region:

| | |
|---|---|
| Content region | `x 0..205` (240 − `RAIL_WIDTH` 34), `y 16..239` (240 − `TITLE_BAR_HEIGHT` 16) |
| Alignment | `L = 12`, `R = 194`, from `RowStyle::FIELD`'s 12/12 |
| Top gutter | 12px, via §3.3's `Spacer` |
| Row height | `row_height(RowStyle::FIELD, font::value())` — **derived, never a literal** |
| Rows that fit | ≥ 7 without scrolling, with a guaranteed peek at row 8 — both already asserted in `fields.rs`'s tests |

The 8-row LDAC state overflows, which is why `FieldList` scrolls, which it
already does. Nothing about this page is resolution-hardcoded and nothing needs
to be.

---

## 7. Migration plan — four steps, each reviewable, each mergeable alone

Steps 1-3 are `pico-link-7jol.4`. Step 4 is `pico-link-7jol.5`.

1. **`ScreenId` + `refresh_stack`, with no new screens.** Add `ScreenId`,
   `Screen::with_id`/`id`, `Navigator::id_at`/`truncate_to`. Tag Home as
   `ScreenId::Home` and Devices as `ScreenId::Devices`. Replace
   `rebuild_root`'s `title_at(1) == DEVICES_TITLE` branch with the generic loop.
   **Zero user-visible change** — this step is pure refactor and its test is
   that every existing test still passes, plus a new one asserting an
   unidentified screen (the wizard) is never replaced.
   → rust-embedded-supervisor
2. **`Spacer`** (§3.3), with a test that a `Spacer(12)` above a `FieldList`
   puts row 1's top at `y = 28` in the 240x240 chrome.
   → rust-embedded-supervisor
3. **The device page** — `device_page_rows` (pure), `DevicePageView` (wrapper,
   X binding), `build_device_page_screen`, wired into `build_identified_screen`
   as `ScreenId::DevicePage(addr)`, replacing the `build_device_detail_screen`
   stub at `app.rs:1271` and its call site at `app.rs:1088`. Rows per §3.4:
   `CODEC`, `SAMPLE RATE`, `USB IN`, `A2DP`, `ADDRESS`, `Forget this device`.
   Headless PNGs at zoom for device-page §10's cases 1, 3, 4 and 7.
   → rust-embedded-supervisor
4. **The picker** — `PickerOption`, `build_single_select_screen`,
   `ScreenId::Picker`, and `pico-link-7jol.5`'s `QUALITY` row on top of it.
   → rust-embedded-supervisor, under `pico-link-7jol.5`

Step 3 is mergeable and useful without step 4. Step 4 is not buildable without
steps 1 and 3.

---

## 8. Hacks to retire, and one not to introduce

**Retire in this bead:**

- **`title_at(1) == Some(DEVICES_TITLE)`** (`app.rs:1447`) — identity by
  display string, at a hardcoded stack index. It was a reasonable interim when
  there was exactly one pushed screen with a constant title. It cannot survive a
  screen whose title is a device's name, and extending it to two more branches
  would be porting a constraint-driven workaround forward instead of fixing it.
  Replaced by `ScreenId`.
- **`build_device_detail_screen(title: String) -> Screen::new(title, vec![])`**
  (`app.rs:1271`) — a labelled destination with nothing in it, taking a
  *rendered label* where every row needs the *record*. Replaced by an
  address-keyed builder.
- **Refreshing exactly one pushed screen** — the loop over the whole stack costs
  nothing extra and removes an arbitrary depth limit nobody chose deliberately.

**Do not introduce:**

- A `PickerView` widget, a `SingleSelectList`, or any new file under
  `core/src/render/` other than `spacer.rs`. §0.1 is the list of reasons there
  is nothing left to build.
- A `stays_open: bool` on the picker (§2.1 rule 2).
- An optimistic check that moves on press (quality-selector §5.1).
- A second dirty-marking path in the picker callback (§2.3).
- A hardcoded row height, gutter or row count. Every number on this page is
  derived from `RowStyle` and the font, and `fields.rs` already asserts the
  budgets.

---

## 9. Explicitly NOT in scope, so `pico-link-7jol.5` has a clean edge

| Not here | Where it belongs |
|---|---|
| The `QUALITY` row, the quality picker's four entries, the sample-rate→{990,660,330} mapping | `pico-link-7jol.5` |
| Home's bitrate `ADAPTIVE` tag and `BitrateStatus`'s adaptive flag (`hero.rs:207`/`hero.rs:436`) | `pico-link-7jol.5` — it is `hero.rs`, not a widget capability |
| Every seam item: `SetDeviceCodecPref`, `CodecAvailability`, `A2dpStreamStateChanged`, USB-IN format, `CodecChanged`'s rate/depth/`pin_outcome`, `PairedDeviceUpserted`'s settings fields | Ada, `2026-09-02-device-page-seam.md`, its own beads. **None exists yet.** Until then §3.4's rows dash. |
| The codec picker's option list and availability reasons | a device-page follow-up, once `CodecAvailability` exists |
| The `Saves when playback stops` / `Save failed` note rows | needs the seam's write-state signals |
| Rebinding Devices' `X` from forget-confirm to open-device-page (device-page §9) | **its own bead.** It changes shipped behaviour, and until it lands the device page is reachable only for the *connected* device — which is sufficient for `7jol.5`, since quality only matters on a live link. |
| Home's `X` → device page | its own bead; §3.2 records the ruling so it cannot collide with `pico-link-9eq2.2` |
| `EQ PRESET`, `LABEL`, rename | device-page §7; no persistence, no DSP |
| Transplanting `paint_cache` across `replace_at` | §5 item 5; a render-pipeline bead |
| The expiring display-power floor on `IdlePolicy` (`run.rs`) that `pico-link-9eq2.2` needs | `pico-link-9eq2.2`. Nothing here touches `run.rs`, and note `run.rs` is emulator-only — the firmware does not run it. |

---

## 10. Risks and open questions

1. **For Uma:** `A`'s rail word on the picker. I ruled `Verb::Select`
   (`select`); her sketches say `pick`, which needs a second
   `Verb::Exception` and therefore her sign-off (§2.2). Low stakes, one line.
2. **For Uma:** the Home `X` face split in §3.2 — status face = `why?`, menu
   face = device page. Both are unbuilt, so this is free to change now and
   expensive later.
3. **For Ada:** `Screen` importing `crate::app::ScreenId` (§1). Precedent exists
   (`widget.rs` imports `LinkState`); say so if you want the boundary restored,
   and it becomes its own refactor bead rather than a surprise in this diff.
4. **Measurement, for Ruby, before trusting any of it:** device-page §5's 28px
   row height is Uma's arithmetic; the shipped
   `row_height(RowStyle::FIELD, font::value())` is the truth. If they disagree,
   the code wins and the sketches are illustrative — do **not** hardcode 28.
5. **Unmeasured:** whether a full-frame repaint per model event on the device
   page is acceptable while the LDAC encoder is starving core 0 at 990 kbps
   (§5 item 5). This is `7jol.5`'s verification item, not a reason to change
   this design.
6. **The dashed page is a UX risk, not a technical one.** A device page whose
   `SAMPLE RATE`, `USB IN` and `A2DP` all read `—` may read as broken rather
   than as honest. It is correct per device-page §3.0 and it is the right thing
   to ship, but if Andreas looks at it and dislikes it, the answer is to land
   the seam beads, not to fake a value.
