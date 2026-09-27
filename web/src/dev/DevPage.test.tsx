import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { ThemeProvider } from "../theme/ThemeProvider";
import { DevPage } from "./DevPage";

describe("DevPage", () => {
  it("renders the theme switcher and connects the fake transport", async () => {
    render(
      <ThemeProvider>
        <DevPage />
      </ThemeProvider>,
    );

    expect(screen.getByTestId("theme-device")).toBeInTheDocument();
    expect(screen.getByTestId("theme-light")).toBeInTheDocument();
    expect(screen.getByTestId("theme-dark")).toBeInTheDocument();
    expect(screen.getByTestId("meter-canvas")).toBeInTheDocument();

    await waitFor(() => expect(screen.getByTestId("info-line")).toHaveTextContent("telemetry proto"));
  });

  it("switching themes updates data-theme without touching the debug readout", async () => {
    render(
      <ThemeProvider>
        <DevPage />
      </ThemeProvider>,
    );
    fireEvent.click(screen.getByTestId("theme-light"));
    expect(document.documentElement.getAttribute("data-theme")).toBe("light");
    expect(screen.getByTestId("debug-readout")).toBeInTheDocument();
  });
});
