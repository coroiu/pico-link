import * as React from "react";
import { Button } from "../../components/ui/button";
import type { Band, CrossfeedLevel, Preamp, Preset } from "../../proto/library";
import { MAX_BANDS } from "../../proto/library";
import { decodeParseApoResult } from "../../proto/ops";
import type { OpOutcome, SaveOutcome } from "../../session/library";
import type { LibraryController } from "../../session/library";
import { decodePresetBlob } from "../../proto/library";
import { BandTable } from "./BandTable";
import { CurveEditor } from "./CurveEditor";
import { LibraryPanel } from "./LibraryPanel";
import { checkName, uniqueName } from "./bandOps";
import { describeOpError, deriveImportBaseName, planImport } from "./importFlow";
import { effectivePreampDb, freqHzToWire, peakBoostDb } from "./math";

interface EffectsTabProps {
  library: LibraryController;
  /** Whether any headphone is currently connected -- affects the editor's "editing silently" line. */
  connectedAddr: string | null;
}

const XFEED_OPTIONS: { level: CrossfeedLevel; label: string }[] = [
  { level: "off", label: "Off" },
  { level: "weak", label: "Light" },
  { level: "medium", label: "Medium" },
  { level: "strong", label: "Strong" },
];

const DEFAULT_BANDS: Band[] = [
  { kind: "lowShelf", freqHalfHz: freqHzToWire(100), gainCdb: 0, qMilli: 700 },
  { kind: "peak", freqHalfHz: freqHzToWire(250), gainCdb: 0, qMilli: 1410 },
  { kind: "peak", freqHalfHz: freqHzToWire(1000), gainCdb: 0, qMilli: 1410 },
  { kind: "peak", freqHalfHz: freqHzToWire(4000), gainCdb: 0, qMilli: 1410 },
  { kind: "highShelf", freqHalfHz: freqHzToWire(8000), gainCdb: 0, qMilli: 700 },
];

function clonePreset(p: Preset): Preset {
  return { ...p, bands: p.bands.map((b) => ({ ...b })) };
}

function describeOutcome(outcome: Exclude<OpOutcome, { kind: "done" }> | Exclude<SaveOutcome, { kind: "queued" }>): string {
  switch (outcome.kind) {
    case "conflict":
      return "Someone else changed this effect first. Reload and try again.";
    case "editorOpen":
      return "The device's own editor is open. Close it there first.";
    case "timeout":
      return "The dongle didn't respond in time.";
    case "unavailable":
      return "This firmware doesn't support editing effects yet.";
    case "rejected":
      return describeOpError(outcome.error);
    default:
      return "Something went wrong.";
  }
}

type Modal =
  | { kind: "unsavedSwitch"; targetId: number | "new" }
  | { kind: "deleteConfirm"; usedByNames: string[] }
  | { kind: "importReplace"; collidesWithId: number; copyName: string; blob: Uint8Array }
  | { kind: "pasteText" };

/**
 * The real Effects tab: library list, per-device assignment, the curve +
 * band-table editor, live preview, and explicit Save. Design of record:
 * UMA DESIGN on pico-link-jyhk.8 section 3, decisions on pico-link-jyhk.14.
 * Replaces `EffectsPlaceholder`.
 */
export function EffectsTab({ library, connectedAddr }: EffectsTabProps) {
  const [libraryState, setLibraryState] = React.useState(library.store.getSnapshot());
  React.useEffect(() => library.store.subscribe(() => setLibraryState(library.store.getSnapshot())), [library]);

  const opsOk = library.opsAvailable();
  const snapshot = libraryState.snapshot;
  const effects = snapshot?.effects ?? [];
  const devices = snapshot?.devices ?? [];
  const maxEffects = snapshot?.maxEffects ?? 8;

  const [selectedId, setSelectedId] = React.useState<number | null>(null);
  const [isNew, setIsNew] = React.useState(false);
  const [draft, setDraft] = React.useState<Preset | null>(null);
  const [baseline, setBaseline] = React.useState<Preset | null>(null);
  const [dirty, setDirty] = React.useState(false);
  const [bypass, setBypass] = React.useState(false);
  const [selectedBand, setSelectedBand] = React.useState(0);
  const [saving, setSaving] = React.useState(false);
  const [saveMsg, setSaveMsg] = React.useState<string | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const [modal, setModal] = React.useState<Modal | null>(null);
  const [dropActive, setDropActive] = React.useState(false);
  const loadedIdRef = React.useRef<number | null>(null);
  const fileInputRef = React.useRef<HTMLInputElement | null>(null);
  const [pasteText, setPasteText] = React.useState("");

  // Auto-select the first effect once the library loads, if nothing is open.
  React.useEffect(() => {
    if (selectedId === null && !isNew && effects.length > 0) setSelectedId(effects[0].id);
  }, [effects, selectedId, isNew]);

  // Load/refresh the draft when the selection changes, without clobbering in-progress edits.
  React.useEffect(() => {
    if (isNew) return;
    if (selectedId === null) {
      setDraft(null);
      setBaseline(null);
      setDirty(false);
      loadedIdRef.current = null;
      return;
    }
    if (loadedIdRef.current === selectedId && draft) return;
    const effect = effects.find((e) => e.id === selectedId);
    if (!effect) return;
    loadedIdRef.current = selectedId;
    setDraft(clonePreset(effect.preset));
    setBaseline(effect.preset);
    setDirty(false);
    setBypass(false);
    setSelectedBand(0);
    setSaveMsg(null);
    setError(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedId, effects, isNew]);

  // --- Live preview: rate-limited (~10/s) while dirty or bypassed. -------
  const previewActiveRef = React.useRef(false);
  const pendingRef = React.useRef<{ effectId: number; preset: Preset; bypass: boolean } | null>(null);
  const lastSentRef = React.useRef(0);

  React.useEffect(() => {
    if (!opsOk || !draft) {
      pendingRef.current = null;
      return;
    }
    if (dirty || bypass) {
      pendingRef.current = { effectId: dirty ? 0 : (selectedId ?? 0), preset: draft, bypass };
    } else {
      pendingRef.current = null;
      if (previewActiveRef.current) {
        previewActiveRef.current = false;
        void library.previewEnd();
      }
    }
  }, [draft, dirty, bypass, selectedId, opsOk, library]);

  React.useEffect(() => {
    const id = setInterval(() => {
      const pending = pendingRef.current;
      if (!pending) return;
      const now = performance.now();
      if (now - lastSentRef.current < 100) return;
      lastSentRef.current = now;
      previewActiveRef.current = true;
      void library.previewStart(pending.effectId, pending.preset, pending.bypass);
    }, 100);
    return () => clearInterval(id);
  }, [library]);

  React.useEffect(
    () => () => {
      if (previewActiveRef.current) void library.previewEnd();
    },
    [library],
  );

  const nameCheck = draft ? checkName(draft.name, effects, isNew ? null : selectedId) : null;
  const canSave = dirty && !!draft && nameCheck && !nameCheck.invalid;

  function requestOpen(target: number | "new") {
    if (dirty) {
      setModal({ kind: "unsavedSwitch", targetId: target });
      return;
    }
    openTarget(target);
  }

  function openTarget(target: number | "new") {
    setError(null);
    setSaveMsg(null);
    if (target === "new") {
      setSelectedId(null);
      setIsNew(true);
      const names = effects.map((e) => e.preset.name);
      setDraft({ name: uniqueName("Effect 1", names), crossfeed: "off", bands: DEFAULT_BANDS.map((b) => ({ ...b })), preamp: { kind: "auto" }, eqLocked: true });
      setBaseline(null);
      setDirty(true);
      setBypass(false);
      setSelectedBand(0);
    } else {
      setIsNew(false);
      loadedIdRef.current = null;
      setSelectedId(target);
    }
  }

  function patchDraft(patch: Partial<Preset>) {
    setDraft((prev) => (prev ? { ...prev, ...patch } : prev));
    setDirty(true);
  }

  function patchBand(index: number, patch: Partial<Band>) {
    setDraft((prev) => {
      if (!prev) return prev;
      const bands = prev.bands.map((b, i) => (i === index ? { ...b, ...patch } : b));
      return { ...prev, bands };
    });
    setDirty(true);
  }

  function addBand(freqHz: number, gainDb: number) {
    setDraft((prev) => {
      if (!prev || prev.bands.length >= MAX_BANDS) return prev;
      const band: Band = { kind: "peak", freqHalfHz: freqHzToWire(freqHz), gainCdb: Math.round(gainDb * 100), qMilli: 1410 };
      const bands = [...prev.bands, band];
      setSelectedBand(bands.length - 1);
      return { ...prev, bands };
    });
    setDirty(true);
  }

  function removeBand(index: number) {
    setDraft((prev) => {
      if (!prev) return prev;
      const bands = prev.bands.filter((_, i) => i !== index);
      setSelectedBand((s) => Math.max(0, Math.min(s, bands.length - 1)));
      return { ...prev, bands };
    });
    setDirty(true);
  }

  async function handleSave() {
    if (!draft || !canSave) return;
    setSaving(true);
    setError(null);
    setSaveMsg(null);
    const toSave: Preset = { ...draft, name: draft.name.trim(), eqLocked: true };
    const existing = isNew ? undefined : { id: selectedId!, baseSeq: effects.find((e) => e.id === selectedId)?.persistedSeq ?? 0 };
    const outcome = await library.saveEffect(toSave, existing);
    setSaving(false);
    if (outcome.kind === "queued") {
      setIsNew(false);
      setSelectedId(outcome.effectId);
      loadedIdRef.current = null;
      setDirty(false);
      setSaveMsg(outcome.confirmed ? "Saved" : "Not confirmed yet — it may still be saving.");
    } else {
      setError(describeOutcome(outcome));
    }
  }

  function handleRevert() {
    if (isNew) {
      setSelectedId(effects[0]?.id ?? null);
      setIsNew(false);
      setDraft(null);
      return;
    }
    if (baseline) setDraft(clonePreset(baseline));
    setDirty(false);
    setBypass(false);
  }

  function handleDuplicate() {
    if (!draft) return;
    const names = effects.map((e) => e.preset.name);
    const name = uniqueName(`${draft.name.slice(0, 13)} 2`, names);
    setIsNew(true);
    setSelectedId(null);
    setDraft({ ...clonePreset(draft), name, eqLocked: true });
    setBaseline(null);
    setDirty(true);
  }

  async function doDelete() {
    if (selectedId === null) return;
    const effect = effects.find((e) => e.id === selectedId);
    const outcome = await library.deleteEffect(selectedId, effect?.persistedSeq ?? 0);
    if (outcome.kind === "done") {
      setSelectedId(null);
      setIsNew(false);
      setDraft(null);
      setBaseline(null);
      setDirty(false);
    } else {
      setError(describeOutcome(outcome));
    }
  }

  function handleDeleteClick() {
    if (selectedId === null) return;
    const usedByNames = devices.filter((d) => d.presetId === selectedId).map((d) => d.name);
    setModal({ kind: "deleteConfirm", usedByNames });
  }

  async function handleAssign(addr: string, effectId: number) {
    const outcome = await library.assign(addr, effectId);
    if (outcome.kind !== "done") setError(describeOutcome(outcome));
  }

  async function runImport(text: string, filename: string) {
    if (!opsOk) return;
    const base = deriveImportBaseName(filename);
    const outcome = await library.parseApo(base, text);
    if (outcome.kind !== "done") {
      setError(describeOutcome(outcome));
      return;
    }
    const result = decodeParseApoResult(outcome.status.payload);
    if (!result) {
      setError("The dongle's reply couldn't be read.");
      return;
    }
    const plan = planImport(result.collidesWith, effects.length, maxEffects, result.copyName);
    if (plan.kind === "full") {
      setError("8 of 8 effects: delete one first");
      return;
    }
    if (plan.kind === "confirmReplace") {
      setModal({ kind: "importReplace", collidesWithId: plan.collidesWithId, copyName: plan.copyName, blob: result.blob });
      return;
    }
    await commitImportedBlob(result.blob, null, null);
  }

  async function commitImportedBlob(blob: Uint8Array, overrideName: string | null, replaceId: number | null) {
    const decoded = decodePresetBlob(blob);
    const preset: Preset = { ...decoded, name: overrideName ?? decoded.name, eqLocked: true };
    const existing = replaceId !== null ? { id: replaceId, baseSeq: effects.find((e) => e.id === replaceId)?.persistedSeq ?? 0 } : undefined;
    const outcome = await library.saveEffect(preset, existing);
    if (outcome.kind === "queued") {
      setIsNew(false);
      setSelectedId(outcome.effectId);
      loadedIdRef.current = null;
      setDirty(false);
      setSaveMsg(outcome.confirmed ? "Saved" : null);
    } else {
      setError(describeOutcome(outcome));
    }
  }

  const onFileChosen: React.ChangeEventHandler<HTMLInputElement> = async (e) => {
    const file = e.target.files?.[0];
    if (file) await runImport(await file.text(), file.name);
    e.target.value = "";
  };

  const onDrop: React.DragEventHandler<HTMLDivElement> = async (e) => {
    e.preventDefault();
    setDropActive(false);
    const file = e.dataTransfer.files?.[0];
    if (file) await runImport(await file.text(), file.name);
  };

  if (!opsOk) {
    return (
      <div className="mx-auto mt-16 max-w-[520px] text-center text-muted-foreground" data-testid="effects-firmware-gate">
        <h1 className="mb-2 text-xl text-foreground">Effects need newer firmware</h1>
        <p>This dongle's firmware doesn't support editing effects from the page yet. Update firmware to edit effects here.</p>
      </div>
    );
  }

  if (libraryState.phase === "loading" && !snapshot) {
    return <div className="mx-auto mt-16 text-center text-muted-foreground">Loading effects…</div>;
  }

  if (libraryState.phase === "error" && !snapshot) {
    return (
      <div className="mx-auto mt-16 max-w-[420px] text-center text-muted-foreground">
        <p className="mb-3">Couldn't load effects from the dongle.</p>
        <Button variant="outline" onClick={() => void library.refreshLibrary()}>
          Retry
        </Button>
      </div>
    );
  }

  const connectedDevice = devices.find((d) => d.addr === connectedAddr) ?? devices.find((d) => d.connected) ?? null;
  const hasEditor = !!draft;

  return (
    <div
      className="relative flex flex-col gap-4 lg:flex-row"
      onDragOver={(e) => {
        e.preventDefault();
        setDropActive(true);
      }}
      onDragLeave={() => setDropActive(false)}
      onDrop={onDrop}
      data-testid="effects-tab"
    >
      {dropActive ? (
        <div className="pointer-events-none fixed inset-0 z-20 flex items-center justify-center bg-scrim text-lg font-semibold text-foreground">Drop to import as an effect</div>
      ) : null}

      <input ref={fileInputRef} type="file" accept=".txt,text/plain" hidden onChange={onFileChosen} data-testid="import-file-input" />

      <LibraryPanel
        effects={effects}
        devices={devices}
        maxEffects={maxEffects}
        selectedId={isNew ? null : selectedId}
        dirty={dirty}
        connectedPresetId={connectedDevice?.presetId ?? null}
        onSelect={(id) => requestOpen(id)}
        onNew={() => requestOpen("new")}
        onImportFile={() => fileInputRef.current?.click()}
        onPasteText={() => setModal({ kind: "pasteText" })}
        onAssign={(addr, effectId) => void handleAssign(addr, effectId)}
      />

      {!hasEditor ? (
        <div className="flex flex-1 items-center justify-center rounded-md border border-dashed border-border p-12 text-center text-muted-foreground" data-testid="effects-empty">
          No effects yet. New, or drop an AutoEQ file here.
        </div>
      ) : (
        <div className="flex flex-1 flex-col gap-3" data-testid="effects-editor">
          <div className="flex flex-wrap items-center gap-2">
            <input
              className="min-w-0 flex-1 rounded border border-border bg-transparent px-2 py-1 text-lg font-semibold"
              maxLength={32}
              spellCheck={false}
              aria-label="Effect name"
              value={draft.name}
              onChange={(e) => patchDraft({ name: e.target.value })}
              data-testid="effect-name-input"
            />
            <span className={`text-xs tabular-nums ${nameCheck?.invalid ? "text-destructive" : "text-muted-foreground"}`} data-testid="effect-name-counter">
              {nameCheck?.reason === "taken" ? "name taken" : nameCheck?.reason === "empty" ? "name required" : `${nameCheck?.byteLength ?? 0} / 16`}
            </span>
            <Button type="button" variant={bypass ? "default" : "outline"} size="sm" aria-pressed={bypass} onClick={() => setBypass((b) => !b)} title="A/B: hear the stream without this effect (key B)">
              {bypass ? "Bypassed" : "Bypass"}
            </Button>
            <Button type="button" variant="outline" size="sm" onClick={handleDuplicate}>
              Duplicate
            </Button>
            <Button type="button" variant="outline" size="sm" className="text-destructive" disabled={isNew} onClick={handleDeleteClick}>
              Delete
            </Button>
            <Button type="button" variant="outline" size="sm" disabled={!dirty} onClick={handleRevert}>
              Revert
            </Button>
            <Button type="button" size="sm" disabled={!canSave || saving} onClick={() => void handleSave()} data-testid="save-button">
              {saving ? "Saving…" : "Save to device"}
            </Button>
          </div>

          <div className="min-h-[1.4em] text-sm text-muted-foreground" data-testid="effect-status">
            {error ? (
              <span className="text-destructive">{error}</span>
            ) : saveMsg ? (
              <span className="text-success">{saveMsg}</span>
            ) : (
              <>
                {connectedDevice ? (
                  dirty ? (
                    <>
                      Previewing on <b className="text-foreground">{connectedDevice.name}</b>
                    </>
                  ) : connectedDevice.presetId === selectedId ? (
                    `Playing on ${connectedDevice.name}`
                  ) : (
                    `Opening an effect previews it on ${connectedDevice.name}`
                  )
                ) : (
                  "No headphones connected: you are editing silently"
                )}
                {bypass ? <span className="ml-2 text-warning">Bypassed: hearing the stream without this effect</span> : null}
                {dirty ? <span className="ml-2">Saving writes flash; audio may hiccup for a moment.</span> : null}
                {!draft.eqLocked ? <span className="ml-2">Made on the device. Saving here stores exact values, so its band rows become read-only on the device.</span> : null}
              </>
            )}
          </div>

          <CurveEditor
            bands={draft.bands}
            selected={selectedBand}
            bypass={bypass}
            onSelect={setSelectedBand}
            onChangeBand={patchBand}
            onAddBand={addBand}
            onRemoveBand={removeBand}
            onCycle={(dir) => setSelectedBand((s) => (s + dir + draft.bands.length) % draft.bands.length)}
          />
          <PreampLine draft={draft} />

          <div className="grid grid-cols-1 gap-3 lg:grid-cols-2">
            <BandTable bands={draft.bands} selected={selectedBand} onSelect={setSelectedBand} onChange={patchBand} onRemove={removeBand} onAdd={() => addBand(1000, 0)} />
            <div className="flex flex-col gap-3">
              <div className="rounded-md border border-border bg-card p-3">
                <h3 className="mb-2 text-xs font-semibold uppercase tracking-[0.14em] text-muted-foreground">Crossfeed</h3>
                <div className="flex gap-1" role="group" aria-label="Crossfeed">
                  {XFEED_OPTIONS.map((opt) => (
                    <Button key={opt.level} type="button" size="sm" variant={draft.crossfeed === opt.level ? "default" : "outline"} aria-pressed={draft.crossfeed === opt.level} onClick={() => patchDraft({ crossfeed: opt.level })}>
                      {opt.label}
                    </Button>
                  ))}
                </div>
                <p className="mt-2 text-xs text-muted-foreground">Eases hard-left/right stereo on long listens.</p>
              </div>

              <div className="rounded-md border border-border bg-card p-3">
                <h3 className="mb-2 text-xs font-semibold uppercase tracking-[0.14em] text-muted-foreground">Preamp</h3>
                <PreampControl draft={draft} onChange={(preamp) => patchDraft({ preamp })} />
              </div>

              <div className="rounded-md border border-border bg-card p-3">
                <h3 className="mb-2 text-xs font-semibold uppercase tracking-[0.14em] text-muted-foreground">Used by</h3>
                <ul className="flex flex-col gap-1 text-sm" data-testid="used-by-list">
                  {devices.map((d) => {
                    const other = d.presetId && d.presetId !== selectedId ? effects.find((e) => e.id === d.presetId) : undefined;
                    return (
                      <li key={d.addr} className="flex items-center gap-2">
                        <input
                          type="checkbox"
                          disabled={isNew}
                          checked={d.presetId === selectedId}
                          onChange={(e) => void handleAssign(d.addr, e.target.checked ? selectedId! : 0)}
                          aria-label={`Use on ${d.name}`}
                        />
                        <span>
                          {d.name}
                          {other ? <span className="ml-1 text-xs text-muted-foreground">(now {other.preset.name})</span> : null}
                        </span>
                      </li>
                    );
                  })}
                  {devices.length === 0 ? <li className="text-muted-foreground">No paired headphones.</li> : null}
                </ul>
              </div>
            </div>
          </div>
        </div>
      )}

      {modal ? (
        <ConfirmModals
          modal={modal}
          draft={draft}
          onClose={() => setModal(null)}
          onDiscardAndSwitch={(target) => {
            setModal(null);
            setDirty(false);
            openTarget(target);
          }}
          onSaveAndSwitch={async (target) => {
            setModal(null);
            await handleSave();
            openTarget(target);
          }}
          onConfirmDelete={async () => {
            setModal(null);
            await doDelete();
          }}
          onImportCopy={async (_collidesWithId, copyName, blob) => {
            setModal(null);
            if (effects.length >= maxEffects) {
              setError("8 of 8 effects: delete one first");
              return;
            }
            await commitImportedBlob(blob, copyName, null);
          }}
          onImportReplace={async (collidesWithId, blob) => {
            setModal(null);
            await commitImportedBlob(blob, null, collidesWithId);
          }}
          onSubmitPaste={async (text) => {
            setModal(null);
            await runImport(text, "Pasted");
          }}
          pasteText={pasteText}
          setPasteText={setPasteText}
        />
      ) : null}
    </div>
  );
}

function PreampLine({ draft }: { draft: Preset }) {
  const peak = peakBoostDb(draft.bands);
  const pre = effectivePreampDb(draft.preamp, draft.bands);
  const over = peak + pre;
  return (
    <div className="flex flex-wrap items-center justify-between gap-2 text-xs text-muted-foreground" data-testid="preamp-line">
      <span>Drag a point: frequency + gain (Shift: fine). Scroll on it: Q. Double-click empty space: add a band. Arrows nudge, Tab cycles, Delete removes.</span>
      <span className="tabular-nums">
        Peak boost +{peak.toFixed(1)} dB · preamp {pre.toFixed(1)} dB{draft.preamp.kind === "auto" ? " (auto)" : ""}
        {over > 0.05 ? <span className="ml-2 font-bold text-warning">+{over.toFixed(1)} dB over 0: may clip on loud tracks</span> : null}
      </span>
    </div>
  );
}

function PreampControl({ draft, onChange }: { draft: Preset; onChange: (p: Preamp) => void }) {
  const effective = effectivePreampDb(draft.preamp, draft.bands);
  return (
    <div>
      <div className="flex gap-1" role="group" aria-label="Preamp mode">
        <Button
          type="button"
          size="sm"
          variant={draft.preamp.kind === "auto" ? "default" : "outline"}
          aria-pressed={draft.preamp.kind === "auto"}
          onClick={() => onChange({ kind: "auto" })}
        >
          Auto
        </Button>
        <Button
          type="button"
          size="sm"
          variant={draft.preamp.kind === "explicit" ? "default" : "outline"}
          aria-pressed={draft.preamp.kind === "explicit"}
          onClick={() => onChange({ kind: "explicit", cdb: Math.round(effective * 100) })}
        >
          Fixed
        </Button>
      </div>
      <div className="mt-2 flex items-center gap-2">
        <input
          type="number"
          step={0.1}
          max={0}
          min={-24}
          disabled={draft.preamp.kind === "auto"}
          value={effective.toFixed(1)}
          onChange={(e) => {
            const v = parseFloat(e.target.value);
            if (!Number.isNaN(v)) onChange({ kind: "explicit", cdb: Math.round(v * 100) });
          }}
          className="w-24 rounded border border-border bg-transparent px-1.5 py-1 text-right tabular-nums"
          aria-label="Preamp dB"
        />
        <span className="text-xs text-muted-foreground">dB</span>
      </div>
    </div>
  );
}

interface ConfirmModalsProps {
  modal: Modal;
  draft: Preset | null;
  onClose: () => void;
  onDiscardAndSwitch: (target: number | "new") => void;
  onSaveAndSwitch: (target: number | "new") => Promise<void>;
  onConfirmDelete: () => Promise<void>;
  onImportCopy: (collidesWithId: number, copyName: string, blob: Uint8Array) => Promise<void>;
  onImportReplace: (collidesWithId: number, blob: Uint8Array) => Promise<void>;
  onSubmitPaste: (text: string) => Promise<void>;
  pasteText: string;
  setPasteText: (v: string) => void;
}

function ConfirmModals({ modal, draft, onClose, onDiscardAndSwitch, onSaveAndSwitch, onConfirmDelete, onImportCopy, onImportReplace, onSubmitPaste, pasteText, setPasteText }: ConfirmModalsProps) {
  return (
    <div className="fixed inset-0 z-30 flex items-center justify-center bg-scrim" role="dialog" aria-modal>
      <div className="w-full max-w-md rounded-md border border-border bg-popover p-4">
        {modal.kind === "unsavedSwitch" ? (
          <>
            <h2 className="mb-2 text-lg font-semibold">Unsaved changes</h2>
            <p className="mb-4 text-sm text-muted-foreground">Save &quot;{draft?.name}&quot; before opening another effect?</p>
            <div className="flex justify-end gap-2">
              <Button variant="outline" onClick={() => onDiscardAndSwitch(modal.targetId)}>
                Discard
              </Button>
              <Button onClick={() => void onSaveAndSwitch(modal.targetId)}>Save</Button>
            </div>
          </>
        ) : null}
        {modal.kind === "deleteConfirm" ? (
          <>
            <h2 className="mb-2 text-lg font-semibold">Delete &quot;{draft?.name}&quot;?</h2>
            <p className="mb-4 text-sm text-muted-foreground">
              {modal.usedByNames.length ? `${modal.usedByNames.join(" and ")} will play without effects (Off). ` : "No headphones use it. "}
              This cannot be undone.
            </p>
            <div className="flex justify-end gap-2">
              <Button variant="outline" onClick={onClose}>
                Cancel
              </Button>
              <Button variant="outline" className="text-destructive" onClick={() => void onConfirmDelete()}>
                Delete
              </Button>
            </div>
          </>
        ) : null}
        {modal.kind === "importReplace" ? (
          <>
            <h2 className="mb-2 text-lg font-semibold">Replace this effect?</h2>
            <p className="mb-4 text-sm text-muted-foreground">An imported effect with this name exists. Replacing keeps its place, its crossfeed and every headphone that uses it.</p>
            <div className="flex justify-end gap-2">
              <Button variant="outline" onClick={() => void onImportCopy(modal.collidesWithId, modal.copyName, modal.blob)}>
                Import as copy
              </Button>
              <Button onClick={() => void onImportReplace(modal.collidesWithId, modal.blob)}>Replace</Button>
            </div>
          </>
        ) : null}
        {modal.kind === "pasteText" ? (
          <>
            <h2 className="mb-2 text-lg font-semibold">Paste Equalizer APO / AutoEQ text</h2>
            <p className="mb-2 text-sm text-muted-foreground">A ParametricEQ.txt: one Preamp line and up to 10 Filter lines.</p>
            <textarea
              className="h-40 w-full rounded border border-border bg-transparent p-2 text-sm"
              value={pasteText}
              onChange={(e) => setPasteText(e.target.value)}
              placeholder={"Preamp: -6.2 dB\nFilter 1: ON LSC Fc 105 Hz Gain 5.4 dB Q 0.70"}
              data-testid="paste-textarea"
            />
            <div className="mt-3 flex justify-end gap-2">
              <Button variant="outline" onClick={onClose}>
                Cancel
              </Button>
              <Button onClick={() => void onSubmitPaste(pasteText)} data-testid="paste-submit">
                Import
              </Button>
            </div>
          </>
        ) : null}
      </div>
    </div>
  );
}
