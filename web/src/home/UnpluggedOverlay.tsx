/** Mock's `#unplugged`: an overlay over Home (last-known content stays visible underneath), not a full-page replacement -- this page reconnects by itself once the device comes back. */
export function UnpluggedOverlay() {
  return (
    <div className="fixed inset-x-0 top-13 bottom-0 z-10 flex justify-center bg-scrim pt-[14vh]" data-testid="unplugged-overlay">
      <div className="max-w-[460px] rounded-lg border border-border bg-card p-6">
        <h2 className="mb-2 text-xl">Pico Link was unplugged</h2>
        <p className="text-muted-foreground">Plug it back in and this page reconnects by itself. Unsaved effect edits stay here until then.</p>
      </div>
    </div>
  );
}
