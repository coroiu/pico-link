import type { EnvScreen } from "./envState";

const DONGLE = (
  <div className="relative mb-6 ml-4.5 flex h-14 w-30 items-center justify-center rounded-lg border-2 border-border">
    <div className="h-9 w-9 rounded-sm bg-popover" />
  </div>
);

interface ConnectScreensProps {
  screen: Exclude<EnvScreen, { kind: "home" }>;
  onConnect: () => void;
}

/**
 * The non-Home environment states (mock's `STATES`): browser unsupported,
 * first-time/return connect, chooser-cancelled, device busy elsewhere, and
 * firmware too old. Design of record: UMA DESIGN on pico-link-jyhk.8's
 * environment-states section; text verbatim from the mock.
 */
export function ConnectScreens({ screen, onConnect }: ConnectScreensProps) {
  if (screen.kind === "nochromium") {
    return (
      <div className="mx-auto mt-[8vh] max-w-[560px]" data-testid="state-nochromium">
        {DONGLE}
        <h1 className="mb-3 text-[30px]">This page needs Chrome or Edge</h1>
        <p className="mb-3.5 text-base leading-[1.55] text-muted-foreground">
          The companion talks to Pico Link over WebUSB, which only Chromium browsers on a computer support: Chrome, Edge, Opera, Brave.
        </p>
        <p className="mb-3.5 text-base leading-[1.55] text-foreground">
          Your dongle does not need this page. Pairing, switching headphones, codec and effects all work from its own screen.
        </p>
      </div>
    );
  }

  if (screen.kind === "busy") {
    return (
      <div className="mx-auto mt-[8vh] max-w-[560px]" data-testid="state-busy">
        {DONGLE}
        <h1 className="mb-3 text-[30px]">Pico Link is open somewhere else</h1>
        <p className="mb-3.5 text-base leading-[1.55] text-muted-foreground">Another tab or app is already talking to it. Close the companion there, then retry. One page at a time.</p>
        <div className="mt-5.5 flex gap-2.5">
          <button type="button" onClick={onConnect} className="rounded-md border border-primary bg-primary px-3 py-1.5 font-semibold text-primary-foreground" data-testid="connect-retry">
            Retry
          </button>
        </div>
      </div>
    );
  }

  if (screen.kind === "oldfw") {
    return (
      <div className="mx-auto mt-[8vh] max-w-[560px]" data-testid="state-oldfw">
        {DONGLE}
        <h1 className="mb-3 text-[30px]">Firmware update needed</h1>
        <p className="mb-3.5 text-base leading-[1.55] text-muted-foreground">
          This Pico Link speaks an older protocol than this page. Everything on the device still works; only the companion needs newer firmware.
        </p>
      </div>
    );
  }

  // "connect": first visit or chooser-cancelled, distinguished only by `note`.
  return (
    <div className="mx-auto mt-[8vh] max-w-[560px]" data-testid="state-connect">
      {DONGLE}
      <h1 className="mb-3 text-[30px]">Connect your Pico Link</h1>
      <p className="mb-3.5 text-base leading-[1.55] text-muted-foreground">
        Plug it in, then pick <b className="text-foreground">Pico Link</b> in the list your browser shows. Once per computer; after that this page connects by itself.
      </p>
      {screen.note ? (
        <div className="mt-3.5 text-sm text-warning" data-testid="connect-note">
          {screen.note}
        </div>
      ) : null}
      <div className="mt-5.5 flex gap-2.5">
        <button type="button" onClick={onConnect} className="rounded-md border border-primary bg-primary px-3 py-1.5 font-semibold text-primary-foreground" data-testid="connect-button">
          Connect
        </button>
      </div>
      <p className="mt-5.5 text-[13px] text-foreground">Audio keeps playing while this page is open. It reads status and writes effects, nothing else.</p>
    </div>
  );
}
