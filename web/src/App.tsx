import { ThemeProvider } from "./theme/ThemeProvider";
import { DevPage } from "./dev/DevPage";
import { HomeApp } from "./home/HomeApp";

// The app's real entry point is `HomeApp` (pico-link-jyhk.12). `?dev` keeps
// the pre-jyhk.12 debug scaffold (`FakeTransport`/theme switcher/raw
// readout) reachable, per the FERN DESIGN note that dev/prod share one app
// distinguished by a query string.
function isDevMode(): boolean {
  return typeof window !== "undefined" && new URLSearchParams(window.location.search).has("dev");
}

function App() {
  return <ThemeProvider>{isDevMode() ? <DevPage /> : <HomeApp />}</ThemeProvider>;
}

export default App;
