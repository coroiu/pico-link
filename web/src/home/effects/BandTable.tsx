import { Button } from "../../components/ui/button";
import type { Band, BandKind } from "../../proto/library";
import { MAX_BANDS } from "../../proto/library";
import { BAND_FREQ_MAX, BAND_FREQ_MIN, BAND_GAIN_MAX, BAND_GAIN_MIN, BAND_Q_MAX, BAND_Q_MIN, clampFreqHz, clampGainDb, clampQ } from "./bandOps";
import { bandFreqHz, bandGainDb, bandQ, fmtFreq, freqHzToWire, gainDbToWire, qToWire } from "./math";

interface BandTableProps {
  bands: Band[];
  selected: number;
  onSelect: (index: number) => void;
  onChange: (index: number, patch: Partial<Band>) => void;
  onRemove: (index: number) => void;
  onAdd: () => void;
}

const KIND_LABEL: Record<BandKind, string> = { peak: "Peak", lowShelf: "Low shelf", highShelf: "High shelf" };

/** The exact-value band table (design of record: UMA DESIGN section 3's "band table with exact numeric entry", max 10 -- mirrors the mock's `#bands`). */
export function BandTable({ bands, selected, onSelect, onChange, onRemove, onAdd }: BandTableProps) {
  return (
    <div className="rounded-md border border-border bg-card p-3">
      <div className="mb-2 flex items-center justify-between">
        <h3 className="text-xs font-semibold uppercase tracking-[0.14em] text-muted-foreground">Bands</h3>
        <span className="text-xs tabular-nums text-muted-foreground" data-testid="band-count">
          {bands.length} of {MAX_BANDS}
        </span>
      </div>
      <table className="w-full text-sm">
        <thead>
          <tr className="text-left text-xs text-muted-foreground">
            <th className="py-1 pl-1">#</th>
            <th>Type</th>
            <th className="text-right">Freq Hz</th>
            <th className="text-right">Gain dB</th>
            <th className="text-right">Q</th>
            <th />
          </tr>
        </thead>
        <tbody data-testid="band-rows">
          {bands.map((b, i) => (
            <tr
              key={i}
              className={`cursor-pointer border-t border-border ${i === selected ? "bg-accent/10" : ""}`}
              onClick={() => onSelect(i)}
              data-testid={`band-row-${i}`}
            >
              <td className="py-1 pl-1 text-muted-foreground">{i + 1}</td>
              <td>
                <select
                  className="bg-transparent"
                  value={b.kind}
                  onChange={(e) => onChange(i, { kind: e.target.value as BandKind })}
                  onClick={(e) => e.stopPropagation()}
                  aria-label={`Band ${i + 1} type`}
                >
                  <option value="peak">{KIND_LABEL.peak}</option>
                  <option value="lowShelf">{KIND_LABEL.lowShelf}</option>
                  <option value="highShelf">{KIND_LABEL.highShelf}</option>
                </select>
              </td>
              <td className="text-right">
                <input
                  className="w-20 bg-transparent text-right tabular-nums"
                  type="number"
                  min={BAND_FREQ_MIN}
                  max={BAND_FREQ_MAX}
                  step={1}
                  defaultValue={fmtFreq(bandFreqHz(b))}
                  key={`f-${i}-${b.freqHalfHz}`}
                  onClick={(e) => e.stopPropagation()}
                  onChange={(e) => {
                    const v = parseFloat(e.target.value);
                    if (!Number.isNaN(v)) onChange(i, { freqHalfHz: freqHzToWire(clampFreqHz(v)) });
                  }}
                  aria-label={`Band ${i + 1} frequency`}
                />
              </td>
              <td className="text-right">
                <input
                  className="w-16 bg-transparent text-right tabular-nums"
                  type="number"
                  min={BAND_GAIN_MIN}
                  max={BAND_GAIN_MAX}
                  step={0.1}
                  defaultValue={bandGainDb(b).toFixed(2)}
                  key={`g-${i}-${b.gainCdb}`}
                  onClick={(e) => e.stopPropagation()}
                  onChange={(e) => {
                    const v = parseFloat(e.target.value);
                    if (!Number.isNaN(v)) onChange(i, { gainCdb: gainDbToWire(clampGainDb(v)) });
                  }}
                  aria-label={`Band ${i + 1} gain`}
                />
              </td>
              <td className="text-right">
                <input
                  className="w-14 bg-transparent text-right tabular-nums"
                  type="number"
                  min={BAND_Q_MIN}
                  max={BAND_Q_MAX}
                  step={0.05}
                  defaultValue={bandQ(b).toFixed(2)}
                  key={`q-${i}-${b.qMilli}`}
                  onClick={(e) => e.stopPropagation()}
                  onChange={(e) => {
                    const v = parseFloat(e.target.value);
                    if (!Number.isNaN(v)) onChange(i, { qMilli: qToWire(clampQ(v)) });
                  }}
                  aria-label={`Band ${i + 1} Q`}
                />
              </td>
              <td>
                <button
                  type="button"
                  className="px-1 text-muted-foreground hover:text-destructive"
                  title="Remove band"
                  aria-label={`Remove band ${i + 1}`}
                  onClick={(e) => {
                    e.stopPropagation();
                    onRemove(i);
                  }}
                >
                  ×
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <div className="mt-2">
        <Button type="button" variant="outline" size="sm" disabled={bands.length >= MAX_BANDS} onClick={onAdd}>
          Add band
        </Button>
      </div>
    </div>
  );
}
