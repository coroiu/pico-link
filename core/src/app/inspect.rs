use core::cell::Ref;

#[cfg(test)]
use super::ui_state::{HomeFace, WizardPhase};
use super::{App, BtModel};
#[cfg(test)]
use super::Command;
#[cfg(test)]
use crate::render::Screen;

impl App {
    /// Read-only access to the live Bluetooth model, for tests/diagnostics
    /// and for any future FFI accessor that needs to read it back. Returns
    /// a [`Ref`] rather than `&BtModel` as of bead `pico-link-bgnd` M0
    /// (`self.model` is now a [`ModelHandle`]) -- `Ref` derefs to
    /// `BtModel`, so every existing `app.model().some_field` call site
    /// keeps compiling unchanged; only a call site that needs an actual
    /// `&BtModel` (a function argument) must add an explicit `&`.
    ///
    /// # Panics
    ///
    /// If a `RefMut` borrow of the model is already held -- see
    /// [`ModelHandle`]'s doc comment for the borrow rule that's meant to
    /// make this never happen in practice.
    ///
    /// # Rule for unsafe FFI call sites (`ui-ffi`)
    ///
    /// In safe Rust the returned `Ref` is borrow-checked against `&App` and
    /// cannot outlive it. But `ui-ffi` derefs a raw `*mut PlUi` to get at
    /// `App`, which yields an *unbounded* lifetime -- so in `ui-ffi`, never
    /// bind `app.model()` to a `let` and hold it across any `pl_ui_*` call
    /// (especially `pl_ui_destroy`, which frees the `Rc<RefCell<BtModel>>`
    /// this `Ref` borrows from). Scope it in an inner block instead, so
    /// `Ref`'s `Drop` runs before the next `pl_ui_*` call. See the DECISION
    /// comment on bead `pico-link-bgnd.7` (a real heap-use-after-free was
    /// found and fixed this way in `ui-ffi/src/lib.rs`).
    #[must_use]
    pub fn model(&self) -> Ref<'_, BtModel> {
        self.model.borrow()
    }

    /// Whether the current volume reading means the display must never go
    /// fully blank (design section 5.4/6.1): muted, or at 0%, from ANY
    /// source. `None` (nothing connected, or no reading yet) is `false` --
    /// there is no banner to protect a floor for. Read every idle tick by
    /// both [`crate::run::Runner::step`] and `ui-ffi`'s `pl_ui_tick` (via
    /// `IdlePolicy::tick`'s `mute_or_zero` parameter), which is what makes
    /// this rule apply on the real target and not just the emulator -- see
    /// `IdlePolicy::tick`'s doc comment for the mechanism.
    #[must_use]
    pub fn volume_requires_dim_floor(&self) -> bool {
        self.model.borrow().volume.is_some_and(|volume| volume.muted || volume.level == 0)
    }

    /// The most recent `now_us` recorded via [`App::tick`]. `0` before the
    /// first tick.
    #[must_use]
    pub fn now_us(&self) -> u64 {
        self.now_us
    }

    /// How many screens are on the navigator's stack (>= 1). Exposed for
    /// tests/diagnostics.
    #[must_use]
    pub fn navigator_depth(&self) -> usize {
        self.navigator.depth()
    }

    /// Whether the navigator is currently showing Home at the root of its
    /// stack (depth 1) -- either face (design section 4/7's `HomeFace`).
    /// `false` for any pushed screen, including Devices (depth 2),
    /// Settings (depth 2), and the pairing wizard (depth 3, see
    /// `crate::render::wizard`'s own navigator-depth tests) -- the pairing
    /// wizard in particular must never be mistaken for "at Home root": a
    /// blanked screen mid-pairing reads as a crash (bead pico-link-4vb.3).
    /// `crate::run::Runner::step` uses this to gate the idle-screensaver
    /// tier so it arms only here.
    #[must_use]
    pub fn is_at_home_root(&self) -> bool {
        self.navigator_depth() == 1
    }

    /// The currently visible screen's title. Exposed for tests/diagnostics
    /// -- in particular, proving that a Bluetooth [`Event`] mid-navigation
    /// doesn't silently pop the user back to the root screen (see
    /// [`App::rebuild_root`]'s doc comment).
    #[must_use]
    pub fn current_screen_title(&self) -> &str {
        &self.navigator.current().title
    }

    /// The root (devices) screen's own selection index, if it currently has
    /// one. Exposed for tests/diagnostics -- proving an [`Event`] carries
    /// the user's list selection forward instead of resetting it to row 0.
    #[must_use]
    pub fn root_selected_index(&self) -> Option<usize> {
        self.navigator.root_selected_index()
    }

    /// Test-only: the Devices screen's own selection index, if it's
    /// currently on the navigator stack at index 1 -- since
    /// `pico-link-znb.8` (E7) made Home (not Devices) the root, this is
    /// what most of this module's pre-E7 `root_selected_index` tests
    /// actually needed to observe (Home's own top-level widget has no
    /// `ListItemKey`/index selection concept -- see `render::home`'s
    /// module doc). Not part of the public API.
    #[cfg(test)]
    pub(crate) fn devices_selected_index_for_test(&self) -> Option<usize> {
        self.navigator.selected_index_at(1)
    }

    /// Test-only: the Devices screen's own scroll-top row index, if it's
    /// currently on the navigator stack at index 1 -- the scroll-position
    /// counterpart to [`App::devices_selected_index_for_test`], proving
    /// [`App::rebuild_root`] carries the user's viewport forward across
    /// an unrelated model event (`.planning/design/2026-09-02-field-list-
    /// widget-ruling.md` §4.7). Not part of the public API.
    #[cfg(test)]
    pub(crate) fn devices_scroll_top_for_test(&self) -> Option<usize> {
        self.navigator.scroll_top_at(1)
    }

    /// Test-only: pushes an arbitrary screen onto the navigator stack, so
    /// tests can simulate "the user navigated away from root" without this
    /// bead building any real second screen (out of its scope -- see
    /// pico-link-a67's scope-discipline note). Not part of the public API.
    #[cfg(test)]
    pub(crate) fn push_screen_for_test(&mut self, screen: Screen) {
        self.navigator.push(screen);
    }

    /// Test-only: pops the top screen off the navigator stack, the
    /// counterpart to [`App::push_screen_for_test`] -- lets a test
    /// simulate "the user backed out to a previous screen" (e.g. back to
    /// Home root) without going through real screen-specific `B` handling.
    /// Not part of the public API.
    #[cfg(test)]
    pub(crate) fn pop_screen_for_test(&mut self) {
        self.navigator.pop();
    }

    /// Test-only: replaces the navigator's *root* screen with an arbitrary
    /// one, staying at depth 1 (Home root) -- unlike
    /// [`App::push_screen_for_test`], which adds a screen on top. Lets a
    /// test exercise generic run-loop behavior (e.g. focus/selection) that
    /// needs some focusable content, while still satisfying
    /// [`App::is_at_home_root`] (bead pico-link-4vb.3's screensaver gate)
    /// the way real Home content does. Not part of the public API.
    #[cfg(test)]
    pub(crate) fn replace_root_for_test(&mut self, screen: Screen) {
        self.navigator.replace_root(screen);
    }

    /// Test-only: enqueues a [`Command`] directly, bypassing the UI
    /// interaction that would normally queue one. `CancelScan` has no
    /// binding built by this bead (the wizard screen in pico-link-znb.7
    /// does that), so this is the only way to exercise its
    /// [`App::poll_command`] round-trip today. Not part of the public API.
    #[cfg(test)]
    pub(crate) fn push_command_for_test(&mut self, command: Command) {
        self.commands.borrow_mut().push_back(command);
    }

    /// Test-only: reads the pairing wizard's current phase. Not part of
    /// the public API.
    #[cfg(test)]
    pub(crate) fn wizard_phase_for_test(&self) -> WizardPhase {
        self.wizard_phase.borrow().clone()
    }

    /// Test-only: reads which of Home's two faces (`HomeFace::Status` vs
    /// `HomeFace::Menu`) is currently showing. Not part of the public API.
    /// Added for pico-link-l4d, whose bug (auto-dismiss landing on the
    /// menu face instead of the hero) was invisible to every existing
    /// test because they only ever asserted `navigator_depth()`, never
    /// the face -- depth alone can't tell Home's two faces apart.
    #[cfg(test)]
    pub(crate) fn home_face_for_test(&self) -> HomeFace {
        *self.home_face.borrow()
    }

}
