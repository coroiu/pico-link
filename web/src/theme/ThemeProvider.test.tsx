import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { THEME_NAMES, ThemeProvider, useTheme } from "./ThemeProvider";

function Probe() {
  const { theme, setTheme } = useTheme();
  return (
    <div>
      <span data-testid="current">{theme}</span>
      {THEME_NAMES.map((name) => (
        <button key={name} onClick={() => setTheme(name)}>
          {name}
        </button>
      ))}
    </div>
  );
}

describe("ThemeProvider", () => {
  beforeEach(() => {
    window.localStorage.clear();
    document.documentElement.removeAttribute("data-theme");
  });

  afterEach(() => {
    window.localStorage.clear();
    document.documentElement.removeAttribute("data-theme");
  });

  it("defaults to the device theme and sets data-theme on <html>", () => {
    render(
      <ThemeProvider>
        <Probe />
      </ThemeProvider>,
    );
    expect(screen.getByTestId("current")).toHaveTextContent("device");
    expect(document.documentElement.getAttribute("data-theme")).toBe("device");
  });

  it("switches theme and persists the choice to localStorage", () => {
    render(
      <ThemeProvider>
        <Probe />
      </ThemeProvider>,
    );
    fireEvent.click(screen.getByText("dark"));
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");
    expect(window.localStorage.getItem("pico-link-web-theme")).toBe("dark");
  });

  it("restores a persisted theme on mount", () => {
    window.localStorage.setItem("pico-link-web-theme", "light");
    render(
      <ThemeProvider>
        <Probe />
      </ThemeProvider>,
    );
    expect(screen.getByTestId("current")).toHaveTextContent("light");
  });

  it("useTheme throws outside a ThemeProvider", () => {
    const Bare = () => {
      useTheme();
      return null;
    };
    expect(() => render(<Bare />)).toThrow();
  });
});
