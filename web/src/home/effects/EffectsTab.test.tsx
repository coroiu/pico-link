import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Session } from "../../session/session";
import { LibraryController } from "../../session/library";
import { FakeTransport } from "../../transport/fake";
import type { FakeTransportOptions } from "../../transport/fake";
import { emptyHomeSnapshot } from "../../proto/telemetry";
import type { HomeSnapshot } from "../../proto/telemetry";
import type { LibrarySnapshot, Preset } from "../../proto/library";
import { emptyLibrarySnapshot } from "../../proto/library";
import { EffectsTab } from "./EffectsTab";

const WARM: Preset = {
  name: "Warm",
  crossfeed: "off",
  preamp: { kind: "auto" },
  eqLocked: true,
  bands: [{ kind: "peak", freqHalfHz: 2000, gainCdb: 300, qMilli: 1000 }],
};

function libraryWith(effects: LibrarySnapshot["effects"] = [{ id: 1, persistedSeq: 1, preset: WARM }], devices: LibrarySnapshot["devices"] = []): LibrarySnapshot {
  return { ...emptyLibrarySnapshot(1), presetsReady: true, effects, devices };
}

function snapshotFor(lib: LibrarySnapshot): HomeSnapshot {
  return { ...emptyHomeSnapshot(), snapSeq: 1, extras: { libraryRev: lib.libraryRev, hostPreviewActive: false, deviceEditorOpen: false, deviceEditorEffectId: 0, presetsReady: true, codecFallbackReason: 0, radioRev: 0 } };
}

let activeSessions: Session[] = [];

afterEach(async () => {
  for (const s of activeSessions) await s.stop();
  activeSessions = [];
});

interface FakeDoc {
  hidden: boolean;
  addEventListener(type: string, cb: () => void): void;
  removeEventListener(type: string): void;
}

function makeFakeDoc(): { doc: Document; impl: FakeDoc; fire: () => void } {
  const listeners = new Map<string, () => void>();
  const impl: FakeDoc = {
    hidden: false,
    addEventListener(type, cb) {
      listeners.set(type, cb);
    },
    removeEventListener(type) {
      listeners.delete(type);
    },
  };
  return { doc: impl as unknown as Document, impl, fire: () => listeners.get("visibilitychange")?.() };
}

async function setup(fakeOpts: FakeTransportOptions = {}, visibilityDocument?: Document): Promise<{ session: Session; controller: LibraryController; transport: FakeTransport }> {
  const lib = fakeOpts.library ?? libraryWith();
  const transport = new FakeTransport({ enableLibrary: true, library: lib, snapshot: () => snapshotFor(lib), ...fakeOpts });
  const session = new Session(transport, { now: () => 0, visibilityDocument });
  activeSessions.push(session);
  await session.start();
  const controller = new LibraryController(session, { previewKeepaliveMs: 10_000, visibilityDocument });
  controller.start();
  await controller.refreshLibrary();
  return { session, controller, transport };
}

describe("EffectsTab firmware gate", () => {
  it("shows an update-firmware message when GET_INFO lacks the v2 op bits", async () => {
    const { controller } = await setup({ enableLibrary: false });
    render(<EffectsTab library={controller} connectedAddr={null} />);
    expect(await screen.findByTestId("effects-firmware-gate")).toHaveTextContent(/update firmware/i);
  });
});

describe("EffectsTab library list", () => {
  it("lists effects with a lock badge for exact-value effects and shows usage counts", async () => {
    const { controller } = await setup({ library: libraryWith([{ id: 1, persistedSeq: 1, preset: WARM }], [{ addr: "AA:BB:CC:DD:EE:FF", presetId: 1, connected: true, name: "Cans" }]) });
    render(<EffectsTab library={controller} connectedAddr="AA:BB:CC:DD:EE:FF" />);
    const item = await screen.findByTestId("effect-item-1");
    expect(item).toHaveTextContent("Warm");
    expect(item).toHaveTextContent("1 device");
    expect(item.querySelector("svg.lock, svg")).toBeTruthy();
  });

  it("shows the empty state when there are no effects", async () => {
    const { controller } = await setup({ library: libraryWith([]) });
    render(<EffectsTab library={controller} connectedAddr={null} />);
    expect(await screen.findByTestId("effects-empty")).toHaveTextContent(/no effects yet/i);
  });
});

describe("EffectsTab editing and save", () => {
  it("renames the selected effect and saves it, then shows Saved once confirmed", async () => {
    const { controller } = await setup();
    render(<EffectsTab library={controller} connectedAddr={null} />);
    const nameInput = (await screen.findByTestId("effect-name-input")) as HTMLInputElement;
    expect(nameInput.value).toBe("Warm");

    fireEvent.change(nameInput, { target: { value: "Cozy" } });
    const saveButton = await screen.findByTestId("save-button");
    await waitFor(() => expect(saveButton).not.toBeDisabled());
    fireEvent.click(saveButton);

    await waitFor(() => expect(screen.getByTestId("effect-status")).toHaveTextContent("Saved"), { timeout: 3000 });
  });

  it("disables Save with a name-taken hint when renaming to a name another effect already uses", async () => {
    const other: Preset = { ...WARM, name: "Bright" };
    const { controller } = await setup({
      library: libraryWith([
        { id: 1, persistedSeq: 1, preset: WARM },
        { id: 2, persistedSeq: 1, preset: other },
      ]),
    });
    render(<EffectsTab library={controller} connectedAddr={null} />);
    const nameInput = (await screen.findByTestId("effect-name-input")) as HTMLInputElement;
    fireEvent.change(nameInput, { target: { value: "Bright" } });
    expect(await screen.findByTestId("effect-name-counter")).toHaveTextContent("name taken");
    expect(screen.getByTestId("save-button")).toBeDisabled();
  });

  it("adds a band via the table's Add band button, up to the 10-band cap", async () => {
    const { controller } = await setup();
    render(<EffectsTab library={controller} connectedAddr={null} />);
    await screen.findByTestId("band-rows");
    expect(screen.getByTestId("band-count")).toHaveTextContent("1 of 10");
    fireEvent.click(screen.getByRole("button", { name: "Add band" }));
    expect(screen.getByTestId("band-count")).toHaveTextContent("2 of 10");
  });

  it("switching effects while dirty prompts an unsaved-changes confirmation", async () => {
    const { controller } = await setup({
      library: libraryWith([
        { id: 1, persistedSeq: 1, preset: WARM },
        { id: 2, persistedSeq: 1, preset: { ...WARM, name: "Bright" } },
      ]),
    });
    render(<EffectsTab library={controller} connectedAddr={null} />);
    const nameInput = (await screen.findByTestId("effect-name-input")) as HTMLInputElement;
    fireEvent.change(nameInput, { target: { value: "Cozy" } });
    fireEvent.click(await screen.findByTestId("effect-item-2"));
    expect(await screen.findByText(/unsaved changes/i)).toBeTruthy();
  });

  it("offers a working Reload on CONFLICT that pulls in the winning save", async () => {
    const lib = libraryWith([{ id: 1, persistedSeq: 1, preset: WARM }]);
    const transport = new FakeTransport({ enableLibrary: true, library: lib, snapshot: () => snapshotFor(lib) });
    const session = new Session(transport, { now: () => 0, visibilityDocument: undefined });
    activeSessions.push(session);
    await session.start();
    const controller = new LibraryController(session, { previewKeepaliveMs: 10_000 });
    await controller.refreshLibrary();

    // Someone else's save lands first, advancing persisted_seq behind this
    // controller's already-fetched (now stale) snapshot.
    const otherController = new LibraryController(session, { previewKeepaliveMs: 10_000 });
    const winning = await otherController.saveEffect({ ...WARM, name: "Elsewhere" }, { id: 1, baseSeq: 1 });
    expect(winning.kind).toBe("queued");

    render(<EffectsTab library={controller} connectedAddr={null} />);
    const nameInput = (await screen.findByTestId("effect-name-input")) as HTMLInputElement;
    expect(nameInput.value).toBe("Warm");
    fireEvent.change(nameInput, { target: { value: "Cozy" } });
    const saveButton = await screen.findByTestId("save-button");
    await waitFor(() => expect(saveButton).not.toBeDisabled());
    fireEvent.click(saveButton);

    const reloadButton = await screen.findByTestId("reload-button");
    fireEvent.click(reloadButton);

    await waitFor(() => expect((screen.getByTestId("effect-name-input") as HTMLInputElement).value).toBe("Elsewhere"));
    expect(screen.queryByTestId("reload-button")).toBeNull();
  });
});

describe("EffectsTab live preview", () => {
  const CONNECTED_DEVICE: LibrarySnapshot["devices"][number] = { addr: "AA:BB:CC:DD:EE:FF", presetId: 1, connected: true, name: "Cans" };

  it("previews the opened effect once a connected device is present", async () => {
    const { controller, transport } = await setup({ library: libraryWith([{ id: 1, persistedSeq: 1, preset: WARM }], [CONNECTED_DEVICE]) });
    const controlOutSpy = vi.spyOn(transport, "controlOut");
    render(<EffectsTab library={controller} connectedAddr={CONNECTED_DEVICE.addr} />);
    await screen.findByTestId("effect-name-input");
    expect(screen.getByTestId("effect-status")).toHaveTextContent(/previewing on cans/i);
    await waitFor(() => expect(controlOutSpy.mock.calls.length).toBeGreaterThan(0), { timeout: 1000 });
  });

  it("sends no PREVIEW traffic at all while the document is hidden", async () => {
    const { doc, impl } = makeFakeDoc();
    impl.hidden = true;
    const { controller, transport } = await setup({ library: libraryWith([{ id: 1, persistedSeq: 1, preset: WARM }], [CONNECTED_DEVICE]) }, doc);
    const controlOutSpy = vi.spyOn(transport, "controlOut");
    render(<EffectsTab library={controller} connectedAddr={CONNECTED_DEVICE.addr} />);
    await screen.findByTestId("effect-name-input");
    await new Promise((resolve) => setTimeout(resolve, 250));
    expect(controlOutSpy).not.toHaveBeenCalled();
  });

  it("stops resending once the document goes hidden mid-preview", async () => {
    const { doc, impl, fire } = makeFakeDoc();
    const { controller, transport } = await setup({ library: libraryWith([{ id: 1, persistedSeq: 1, preset: WARM }], [CONNECTED_DEVICE]) }, doc);
    const controlOutSpy = vi.spyOn(transport, "controlOut");
    render(<EffectsTab library={controller} connectedAddr={CONNECTED_DEVICE.addr} />);
    await screen.findByTestId("effect-name-input");
    await waitFor(() => expect(controlOutSpy.mock.calls.length).toBeGreaterThan(0), { timeout: 1000 });
    const callsWhileVisible = controlOutSpy.mock.calls.length;

    impl.hidden = true;
    fire();
    await new Promise((resolve) => setTimeout(resolve, 250));
    expect(controlOutSpy.mock.calls.length).toBe(callsWhileVisible);
  });

  it("ends the preview when the effect's editor is closed by leaving the tab (unmount)", async () => {
    const { controller, transport } = await setup({ library: libraryWith([{ id: 1, persistedSeq: 1, preset: WARM }], [CONNECTED_DEVICE]) });
    const controlOutSpy = vi.spyOn(transport, "controlOut");
    const previewEndSpy = vi.spyOn(controller, "previewEnd");
    const { unmount } = render(<EffectsTab library={controller} connectedAddr={CONNECTED_DEVICE.addr} />);
    await screen.findByTestId("effect-name-input");
    // Wait for the debounced preview to actually start before leaving --
    // otherwise there is nothing for unmount's cleanup to end.
    await waitFor(() => expect(controlOutSpy.mock.calls.length).toBeGreaterThan(0), { timeout: 1000 });
    unmount();
    expect(previewEndSpy).toHaveBeenCalled();
  });

  it("keeps previewing with the saved values after Save, since the editor stays open", async () => {
    const { controller, transport } = await setup({ library: libraryWith([{ id: 1, persistedSeq: 1, preset: WARM }], [CONNECTED_DEVICE]) });
    const controlOutSpy = vi.spyOn(transport, "controlOut");
    render(<EffectsTab library={controller} connectedAddr={CONNECTED_DEVICE.addr} />);
    const nameInput = (await screen.findByTestId("effect-name-input")) as HTMLInputElement;
    // Wait for the debounced open-preview to actually land before editing --
    // otherwise Save can race ahead of `previewActiveRef` ever going true.
    await waitFor(() => expect(controlOutSpy.mock.calls.length).toBeGreaterThan(0), { timeout: 1000 });

    fireEvent.change(nameInput, { target: { value: "Cozy" } });
    const previewEndSpy = vi.spyOn(controller, "previewEnd");
    const previewStartSpy = vi.spyOn(controller, "previewStart");
    const saveButton = await screen.findByTestId("save-button");
    await waitFor(() => expect(saveButton).not.toBeDisabled());
    fireEvent.click(saveButton);
    await waitFor(() => expect(screen.getByTestId("effect-status")).toHaveTextContent("Saved"), { timeout: 3000 });

    // Save does not end the preview: the editor is still open (still shows
    // "Previewing on Cans"), so no PREVIEW_END is sent...
    expect(previewEndSpy).not.toHaveBeenCalled();
    expect(screen.getByTestId("effect-status")).toHaveTextContent(/previewing on cans/i);
    // ...and the preview effect resends with the now-persisted effect id and
    // the saved (renamed) values, once the debounce settles.
    await waitFor(() => expect(previewStartSpy).toHaveBeenCalledWith(1, expect.objectContaining({ name: "Cozy" }), false), { timeout: 1000 });
  });
});

describe("EffectsTab import", () => {
  it("imports pasted Equalizer APO text as a new effect", async () => {
    const { controller } = await setup({ library: libraryWith([]) });
    render(<EffectsTab library={controller} connectedAddr={null} />);
    fireEvent.click(await screen.findByRole("button", { name: "Paste text" }));
    const textarea = await screen.findByTestId("paste-textarea");
    fireEvent.change(textarea, { target: { value: "Preamp: -6.0 dB\nFilter 1: ON PK Fc 1000 Hz Gain 3.0 dB Q 1.00" } });
    fireEvent.click(screen.getByTestId("paste-submit"));

    await waitFor(() => expect(screen.queryByTestId("effects-empty")).toBeNull(), { timeout: 3000 });
    expect(await screen.findByTestId("effect-name-input")).toBeTruthy();
  });

  it("shows a rejection message for text with no filters", async () => {
    const { controller } = await setup({ library: libraryWith([]) });
    render(<EffectsTab library={controller} connectedAddr={null} />);
    fireEvent.click(await screen.findByRole("button", { name: "Paste text" }));
    fireEvent.change(await screen.findByTestId("paste-textarea"), { target: { value: "not a valid apo file" } });
    fireEvent.click(screen.getByTestId("paste-submit"));
    await waitFor(() => expect(screen.getByTestId("effects-empty")).toBeTruthy());
  });
});
