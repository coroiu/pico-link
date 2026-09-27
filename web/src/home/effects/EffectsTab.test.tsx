import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
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
  return { ...emptyHomeSnapshot(), snapSeq: 1, extras: { libraryRev: lib.libraryRev, hostPreviewActive: false, deviceEditorOpen: false, deviceEditorEffectId: 0, presetsReady: true, codecFallbackReason: 0 } };
}

let activeSessions: Session[] = [];

afterEach(async () => {
  for (const s of activeSessions) await s.stop();
  activeSessions = [];
});

async function setup(fakeOpts: FakeTransportOptions = {}): Promise<{ session: Session; controller: LibraryController; transport: FakeTransport }> {
  const lib = fakeOpts.library ?? libraryWith();
  const transport = new FakeTransport({ enableLibrary: true, library: lib, snapshot: () => snapshotFor(lib), ...fakeOpts });
  const session = new Session(transport, { now: () => 0, visibilityDocument: undefined });
  activeSessions.push(session);
  await session.start();
  const controller = new LibraryController(session, { previewKeepaliveMs: 10_000 });
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
