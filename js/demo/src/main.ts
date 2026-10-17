import { mount } from "svelte";
import App from "./App.svelte";
import Embed from "./components/Embed.svelte";

// ?embed renders a compact runner for embedding in an iframe (eryx.run).
const embed = new URLSearchParams(window.location.search).has("embed");
if (embed) document.documentElement.classList.add("embed");

const app = mount(embed ? Embed : App, {
  target: document.getElementById("app")!,
});

export default app;
