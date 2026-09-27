import { ThemeProvider } from "./theme/ThemeProvider";
import { DevPage } from "./dev/DevPage";

// This scaffold's only route is the dev page (FakeTransport + theme
// switcher + debug readout). Home/EQ views land on jyhk.12+, once Uma's
// design exists (pico-link-jyhk.10's description: "no Home visuals yet").
function App() {
  return (
    <ThemeProvider>
      <DevPage />
    </ThemeProvider>
  );
}

export default App;
