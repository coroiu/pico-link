// A small get/set/subscribe store (FERN DESIGN section 1: "one hand-rolled
// store (get/set/subscribe, ~30 lines)"), generic so both the session state
// machine and any future slow-changing state can use it. Pairs with
// `useStore` (React's `useSyncExternalStore`) for components that want
// re-renders on change -- the 30Hz meter data never goes through this: it
// lives in a plain ref the canvas reads directly (design: "without pushing
// 30Hz data through React state").
import * as React from "react";

export interface Store<T> {
  getSnapshot(): T;
  subscribe(listener: () => void): () => void;
  set(next: T): void;
  update(fn: (prev: T) => T): void;
}

export function createStore<T>(initial: T): Store<T> {
  let value = initial;
  const listeners = new Set<() => void>();

  return {
    getSnapshot: () => value,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    set(next) {
      value = next;
      for (const listener of listeners) listener();
    },
    update(fn) {
      const next = fn(value);
      value = next;
      for (const listener of listeners) listener();
    },
  };
}

/** Subscribes a component to `store`, re-rendering on every `set`/`update`. */
export function useStore<T>(store: Store<T>): T {
  return React.useSyncExternalStore(store.subscribe, store.getSnapshot);
}
