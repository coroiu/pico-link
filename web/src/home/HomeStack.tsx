import * as React from "react";
import type { HomeSnapshot } from "../proto/telemetry";
import type { OutLevelBallistics } from "../meter/ballistics";
import type { ClockOffsetEstimator } from "../session/clock";
import { computeBanner } from "./volume";
import { OutMeter } from "./OutMeter";
import { FaultStrip } from "./FaultStrip";
import { useIsCompact } from "./useIsCompact";

interface HomeStackProps {
  snapshotRef: React.RefObject<HomeSnapshot | null>;
  ballistics: OutLevelBallistics;
  clock: ClockOffsetEstimator;
  /** Slow-changing text fields, re-read from `snapshotRef` on an interval by the caller -- never at 30Hz. */
  text: HomeSnapshot;
  onEditFx: () => void;
}

/**
 * The device Home layout at browser resolution (design of record: UMA
 * DESIGN on pico-link-jyhk.8, mock's `#page-home`): device name, hero codec
 * word, bitrate + ADAPTIVE tag, a persistent banner slot, the FX line, the
 * OUT meter, and the fault strip below a rule. Every text slot reserves its
 * height (mock's `min-height`s) so nothing shifts when it goes empty <->
 * non-empty.
 */
export function HomeStack({ snapshotRef, ballistics, clock, text, onEditFx }: HomeStackProps) {
  const compact = useIsCompact();
  const banner = computeBanner(text);
  const heroClass = !text.linkConnected ? "text-destructive" : "text-foreground";
  const heroWord = text.linkConnected ? text.codecWord || "–" : "NO LINK";
  const bitrateText = !text.linkConnected ? "" : text.kbpsIsLive ? `${text.kbps} kbps` : "idle";

  return (
    <div
      className="mx-auto grid max-w-[920px]"
      style={{ gridTemplateColumns: "minmax(0,1fr) auto", columnGap: compact ? 20 : 40 }}
      data-testid="home-stack"
    >
      <div className="min-w-0">
        <div className="mt-1 min-h-[1.2em] overflow-hidden text-ellipsis whitespace-nowrap text-[clamp(18px,2.2vw,26px)] font-semibold" data-testid="home-device-name">
          {text.linkConnected ? text.deviceName : ""}
        </div>
        <div
          className={`mt-2 whitespace-nowrap text-[clamp(48px,10vw,140px)] font-extrabold leading-[0.92] tracking-[-0.02em] ${heroClass}`}
          data-testid="home-hero"
        >
          {heroWord}
        </div>
        <div className="mt-1.5 flex min-h-[1.3em] items-baseline gap-3.5 text-[clamp(22px,3vw,36px)]" data-testid="home-bitrate">
          <span className="tabular-nums">{bitrateText}</span>
          <span className="text-[clamp(11px,1vw,13px)] font-bold uppercase tracking-[0.12em] text-muted-foreground">
            {text.linkConnected && text.kbpsAdaptive ? "Adaptive" : ""}
          </span>
        </div>
        <div className="mt-2.5 min-h-[1.6em] text-[clamp(14px,1.4vw,17px)] font-bold tracking-[0.04em] text-warning" data-testid="home-banner">
          {banner?.text ?? ""}
        </div>
        <div className="min-h-[1.6em] text-[clamp(15px,1.5vw,19px)] text-muted-foreground" data-testid="home-fx">
          {text.linkConnected && text.fxPresetName ? (
            <>
              FX{" "}
              <button type="button" className="cursor-pointer border-b border-dashed border-border text-inherit hover:border-muted-foreground hover:text-foreground" onClick={onEditFx}>
                {text.fxPresetName}
              </button>
            </>
          ) : null}
        </div>
      </div>

      <div style={{ gridColumn: 2, gridRow: compact ? "1" : "1 / span 2" }}>
        <OutMeter snapshotRef={snapshotRef} ballistics={ballistics} clock={clock} compact={compact} />
      </div>

      <div style={{ gridColumn: compact ? "1 / -1" : "1" }}>
        <FaultStrip faults={text.faults} nowMs={text.uptimeMs} />
      </div>
    </div>
  );
}
