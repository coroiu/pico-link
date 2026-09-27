import * as React from "react";

/**
 * Tracks whether the viewport is narrower than `breakpointPx` -- the
 * mock's `@media (max-width:620px)` rule, where the fault strip drops below
 * the hero+meter row instead of sitting beside it. A single JS breakpoint
 * (rather than a CSS media query) so `HomeStack`'s grid layout and
 * `OutMeter`'s canvas-column geometry can't disagree about which mode they're in.
 */
export function useIsCompact(breakpointPx = 620): boolean {
  const [compact, setCompact] = React.useState(() => (typeof window === "undefined" ? false : window.innerWidth < breakpointPx));

  React.useEffect(() => {
    if (typeof window === "undefined") return;
    const onResize = () => setCompact(window.innerWidth < breakpointPx);
    onResize();
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, [breakpointPx]);

  return compact;
}
