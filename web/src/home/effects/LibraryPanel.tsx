import { Button } from "../../components/ui/button";
import type { LibraryDevice, LibraryEffect } from "../../proto/library";

const LOCK = (
  <svg viewBox="0 0 9 11" className="ml-1 inline h-2.5 w-2.5 align-baseline text-muted-foreground" aria-hidden>
    <rect x="0.5" y="4.5" width="8" height="6" rx="1" fill="currentColor" />
    <path d="M2 4.5V3a2.5 2.5 0 0 1 5 0v1.5" fill="none" stroke="currentColor" strokeWidth="1.3" />
  </svg>
);

interface LibraryPanelProps {
  effects: LibraryEffect[];
  devices: LibraryDevice[];
  maxEffects: number;
  selectedId: number | null;
  dirty: boolean;
  connectedPresetId: number | null;
  onSelect: (id: number) => void;
  onNew: () => void;
  onImportFile: () => void;
  onPasteText: () => void;
  onAssign: (addr: string, effectId: number) => void;
}

function usedByCount(effectId: number, devices: LibraryDevice[]): number {
  return devices.filter((d) => d.presetId === effectId).length;
}

/**
 * Left column: the on-device effect library (<=8, lock badge for
 * exact-value effects, usage count) plus the Headphones panel (per-device
 * assignment dropdown). Design of record: UMA DESIGN on pico-link-jyhk.8
 * section 3; mirrors the mock's `#lib`/`#devlist`.
 */
export function LibraryPanel({ effects, devices, maxEffects, selectedId, dirty, connectedPresetId, onSelect, onNew, onImportFile, onPasteText, onAssign }: LibraryPanelProps) {
  const full = effects.length >= maxEffects;
  return (
    <aside className="flex w-full flex-col gap-4 lg:w-72 lg:shrink-0" data-testid="effects-library">
      <div className="rounded-md border border-border bg-card p-3">
        <div className="mb-2 flex items-center justify-between">
          <h3 className="text-xs font-semibold uppercase tracking-[0.14em] text-muted-foreground">Effects</h3>
          <span className="text-xs tabular-nums text-muted-foreground" data-testid="effects-slots">
            {effects.length} of {maxEffects}
          </span>
        </div>
        <ul className="mb-3 flex flex-col gap-1" data-testid="effects-list">
          {effects.map((e) => {
            const n = usedByCount(e.id, devices);
            const playing = connectedPresetId === e.id;
            const isSel = e.id === selectedId;
            return (
              <li key={e.id}>
                <button
                  type="button"
                  onClick={() => onSelect(e.id)}
                  className={`flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-sm ${isSel ? "bg-accent/15 text-foreground" : "hover:bg-secondary"}`}
                  data-testid={`effect-item-${e.id}`}
                >
                  <span className="w-4 shrink-0 text-center text-success" title={playing ? "Playing on connected headphones" : ""}>
                    {playing ? "✓" : ""}
                  </span>
                  <span className="min-w-0 flex-1 truncate">
                    {e.preset.name}
                    {e.preset.eqLocked ? <span title="Exact values: its band rows are read-only on the device; crossfeed stays editable there">{LOCK}</span> : null}
                    {isSel && dirty ? <span className="ml-1 text-warning">{"•"}</span> : null}
                  </span>
                  <span className="shrink-0 text-xs text-muted-foreground">{n === 0 ? "unused" : n === 1 ? "1 device" : `${n} devices`}</span>
                </button>
              </li>
            );
          })}
          {effects.length === 0 ? <li className="px-2 py-3 text-sm text-muted-foreground">No effects yet.</li> : null}
        </ul>
        <div className="flex flex-wrap gap-2">
          <Button type="button" variant="outline" size="sm" disabled={full} title={full ? "8 of 8 effects: delete one first" : ""} onClick={onNew}>
            New
          </Button>
          <Button type="button" variant="outline" size="sm" disabled={full} title={full ? "8 of 8 effects: delete one first" : ""} onClick={onImportFile}>
            Import file
          </Button>
          <Button type="button" variant="outline" size="sm" disabled={full} title={full ? "8 of 8 effects: delete one first" : ""} onClick={onPasteText}>
            Paste text
          </Button>
        </div>
        <p className="mt-2 text-xs text-muted-foreground">
          Drop an Equalizer APO or AutoEQ .txt anywhere on this page. Effects live on the dongle and belong to no single headphone: forgetting a device never deletes one.
        </p>
      </div>

      <div className="rounded-md border border-border bg-card p-3">
        <h3 className="mb-2 text-xs font-semibold uppercase tracking-[0.14em] text-muted-foreground">Headphones</h3>
        <ul className="flex flex-col gap-2" data-testid="device-list">
          {devices.map((d) => (
            <li key={d.addr} className="flex items-center gap-2 text-sm">
              <span className={`h-2 w-2 shrink-0 rounded-full ${d.connected ? "bg-success" : "bg-muted-foreground"}`} />
              <span className="min-w-0 flex-1 truncate">
                {d.name}
                {d.connected ? <span className="ml-1 text-xs text-muted-foreground">connected</span> : null}
              </span>
              <select
                className="rounded border border-border bg-transparent px-1 py-0.5 text-xs"
                aria-label={`Effect for ${d.name}`}
                value={d.presetId}
                onChange={(e) => onAssign(d.addr, Number(e.target.value))}
              >
                <option value={0}>Off</option>
                {effects.map((e) => (
                  <option key={e.id} value={e.id}>
                    {e.preset.name}
                  </option>
                ))}
              </select>
            </li>
          ))}
          {devices.length === 0 ? <li className="text-sm text-muted-foreground">No paired headphones.</li> : null}
        </ul>
      </div>
    </aside>
  );
}
