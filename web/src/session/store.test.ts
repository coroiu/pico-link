import { describe, expect, it, vi } from "vitest";
import { createStore } from "./store";

describe("createStore", () => {
  it("returns the initial value", () => {
    const store = createStore(1);
    expect(store.getSnapshot()).toBe(1);
  });

  it("notifies subscribers on set", () => {
    const store = createStore(1);
    const listener = vi.fn();
    store.subscribe(listener);
    store.set(2);
    expect(store.getSnapshot()).toBe(2);
    expect(listener).toHaveBeenCalledTimes(1);
  });

  it("update derives the next value from the previous one", () => {
    const store = createStore(1);
    store.update((prev) => prev + 41);
    expect(store.getSnapshot()).toBe(42);
  });

  it("unsubscribe stops notifications", () => {
    const store = createStore(1);
    const listener = vi.fn();
    const unsubscribe = store.subscribe(listener);
    unsubscribe();
    store.set(2);
    expect(listener).not.toHaveBeenCalled();
  });
});
