DESIGN: render framework for the effects editor -- value row + on-exit hook (Fern, 2026-09-25, desk pass, main 7229ecb)

== 1. LEFT/RIGHT RULE ==
- Design of record sec 4 (Left = B, Right = A) is RETIRED globally, not scoped. It was never built (navigator.rs dispatch forwards Left/Right to the focused widget only; FieldList/VerticalList/MenuList all ignore them). Scoping it would make Left mean Back on some screens and decrease on others; on the editor that turns the most natural decrease press into leave-and-write-flash.
- New rule for sec 4: Left/Right step the focused VALUE row; on any other row they are no-ops. Navigator must NEVER gain an unconsumed-Left-falls-through-to-Back fallback. Guard test below.
- Navigator::dispatch needs NO change: Left/Right already forward to the focused widget and run the returned Action through apply_action.

== 2. VALUE ROW (piece 1) ==
Shape: a third FieldKind in fields.rs, not a new widget. FieldList already owns scrolling, keys, sync via set_rows, draw_row and the A-liveness rule. A sibling widget would duplicate all of it; three separate focusable widgets would hit the known Up/Down forward-and-refocus simplification (navigator.rs dispatch doc) and move two things per press.
- FieldKind::Value(StepBounds) where StepBounds { prev: bool, next: bool }. Constructor FieldRow::value(label, text, bounds). Must have with_key (the step callback is keyed, never index-based).
- Clamp vs wrap is NOT a widget concept. The screen builder computes StepBounds from the model: clamped ladder at its low end -> prev false; wrapping ladder (NAME) -> both true. The widget only renders and gates.
- FieldList::on_step(callback: Fn(ListItemKey, Step) -> Action), Step = Prev | Next. on_intent: Left/Right on a selected Value row whose side is live -> call callback, return its Action. Side not live, or row not Value, or no callback -> Action::None and no callback call.
- The widget holds NO value state. The callback writes the App-owned draft mailbox (Rc RefCell, the abr_floor/cushion shape); sync() rebuilds rows from the draft. Values are views, never widget-local.
- Rendering: chevrons drawn on the FOCUSED value row only, flanking the value text; a dead side draws in DIVIDER. Unfocused value rows draw like Readonly-bright (label + value, no chevrons, no caret). Value rows never draw a caret.
- activation() returns None on a Value row -> A dim and inert, by the existing Screen::resolve_a / activate_focused gate. No new rule.
- Paint key: fold the drawn chevron liveness (prev/next) of the focused row and the value text; nothing undrawn (memory damage-keys-must-fold-only-what-is-drawn).
- Section gaps: FieldRow::with_gap_before() -- 6px + DIVIDER hairline, counted by measure/row geometry and reconcile_top_index. Footer is a separate non-focusable widget after the FieldList.
- Editor screen contract: exactly ONE focusable widget (an EffectEditorView wrapper around FieldList, the device_page.rs pattern, which also owns X bypass and Y reset). This keeps the Up/Down refocus simplification a no-op.

== 3. ON-EXIT HOOK (piece 2) ==
Where: Screen, fired by Navigator. Not a widget (widgets cannot see stack removal; B-handler-only misses event unwinds), not Drop (would fire on pl_ui_destroy and test teardown, and runs closures during unwinding of a Vec mid-truncate).
- Screen::with_on_exit(f: impl FnOnce() + 'static) stores Option Box dyn FnOnce.
- Navigator gets ONE private funnel: fn retire(&mut self, mut screen: Screen) -> takes on_exit (Option::take) and calls it. Every removal routes through it:
  - pop: let s = self.stack.pop(); self.retire(s).
  - pop_to_root and truncate_to: loop pop-and-retire from the top down to the keep length (NOT Vec::truncate, which drops silently). Top-down order = LIFO, so a screen above the editor retires first.
  - replace_root: mem::replace stack[0], retire the old root.
  - Rule for Ruby: after this change, no code in navigator.rs may call stack.pop/truncate/assignment except through retire. Put that sentence in the stack field doc.
- Exactly once is structural: FnOnce taken out of an Option, and a retired Screen is consumed. Push over the editor (e.g. a toast/confirm) does NOT fire it.
- Hook runs AFTER the screen leaves the stack (the closure can never observe itself on top) and synchronously inside the navigator op.
- Hook contract: it may only write App-owned mailboxes / the commands queue; it has no navigator reference so it cannot push/pop. Callers must not hold a borrow of those mailboxes across a navigator op. Audited today: App::handle_input, fold.rs on_wizard_auto_dismiss (the wizard_phase borrow is a released temporary), App::prune_stack (model borrow released before truncate_to), inspect.rs pop/replace_root_for_test -- none hold one.
- Not fired on pl_ui_destroy / App drop. Nothing leaves by a route there.

Editor use (Ruby, ryw.7): the closure captures the draft mailbox and calls EffectEditState::close(): take draft; if new or differs from stored, set save_pending. close() on an empty draft is a no-op (second guard). App::take_effect_to_save() drains it and ui-ffi pl_ui_poll_command checks it alongside take_abr_floor_to_save (design D9 shape). Program-resolution rule 1 reads draft.is_some(), so the preview ends on exactly the same edge.

DEVICE PATH: pl_ui_input -> App::handle_input -> Navigator::dispatch -> pop -> retire; pl_ui_push_event -> fold -> pop_to_root / prune_stack truncate_to -> retire; pl_ui_poll_command drains. None of it touches run.rs, so emulator and firmware get identical behaviour.

CORRECTION to Uma sec 5.2: today NO connection event unwinds the Effects branch. replace_root is test-only (inspect.rs replace_root_for_test), pop_to_root fires only on wizard auto-dismiss (Devices branch, cannot coexist with the editor), prune_stack only prunes DevicePage/Picker ids. The funnel still covers them, so any future event-driven reset is safe by construction, but the event-driven test must drive the navigator op directly.

== 4. TESTS ==
navigator.rs (counting hook via Rc Cell u32):
- on_exit_fires_once_on_back_pop
- on_exit_fires_once_on_widget_popview_action
- on_exit_fires_once_on_b_b_escape (second B pops the parent; editor count stays 1)
- on_exit_fires_once_per_screen_on_pop_to_root_top_down (depth 3, both fire, order asserted)
- on_exit_fires_only_above_index_on_truncate_to
- on_exit_fires_for_old_root_on_replace_root
- on_exit_does_not_fire_when_a_screen_is_pushed_over_it
- on_exit_does_not_fire_on_back_at_root
- on_exit_not_fired_by_sync_or_render (20 sync_top + render cycles -> 0)
- left_never_pops_the_stack (Left/Right on a pushed screen keep depth)
fields.rs:
- value_row_left_right_calls_on_step_with_key_and_direction
- value_row_dead_side_is_noop_and_skips_callback
- left_right_on_action_and_readonly_rows_are_noops
- value_row_activation_is_none (A dim)
- chevrons_only_on_focused_value_row_and_dead_side_is_divider (zoomed pixel sample)
app / ui-ffi (the device path):
- effect_editor_back_saves_exactly_once (handle_input Back; take_effect_to_save Some then None)
- effect_editor_b_b_saves_exactly_once
- effect_editor_stack_unwind_saves_exactly_once (App test helper calling pop_to_root while editor open)
- effect_editor_unchanged_existing_effect_writes_nothing
- new_effect_always_saves_even_unchanged
- ffi_back_on_editor_yields_one_save_command (pl_ui_input B, then pl_ui_poll_command twice: save, then none)

== 5. HACKS NOT TO PORT ==
- No save in the B handler / on_intent Back arm. No widget-local was-saved flag. No Drop impl on Screen.
- No Left=Back special case anywhere.

== 6. OPEN ==
- Power loss mid-edit loses the whole edit (RAM draft, save only on exit). Uma accepted it; memory deferred-writes-lose-to-a-power-cycle says Andreas prefers write-early for pairing. Low stakes for an EQ, but it is his call. Framework supports either (the mailbox could also save on a debounce later).
- Home menu third row (Effects) needs no framework change; Ruby in ryw.7.
- Design doc not written to .planning/design (main is branch-protected, as Uma hit). Whoever branches next: commit this as .planning/design/2026-09-25-value-row-and-on-exit-hook.md and amend design of record sec 4.
