import fs from "node:fs";
import { Config } from "@remotion/cli/config";

// Frames go to the encoder as PNG: the footage is flat interface colour, and
// JPEG would leave marks on it before H.264 gets to see it.
Config.setVideoImageFormat("png");
Config.setPixelFormat("yuv420p");
Config.setCodec("h264");
Config.setOverwriteOutput(true);

// The headless shell that Remotion downloads never delivers an animation
// frame on macOS 27, so every render times out while "waiting for the root
// component". An installed Chrome works. CHROME=<path> picks another browser;
// CHROME=remotion goes back to the downloaded one.
const chrome =
  process.env.CHROME ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
if (chrome !== "remotion" && fs.existsSync(chrome)) {
  Config.setBrowserExecutable(chrome);
  Config.setChromeMode("chrome-for-testing");
}
