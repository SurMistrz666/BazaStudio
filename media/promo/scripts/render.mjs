// Renders the deliverables into `docs/media/` of the repository:
//
//   baza-studio-promo.mp4          the film, H.264, 1920x1080, 30 fps, no sound
//   baza-studio-promo.gif          the short loop for the README, 960 px wide
//   baza-studio-promo-poster.png   the poster
//   screenshot-*.png               still screenshots, straight from the captures
//
//   node scripts/render.mjs [film] [loop] [poster] [stills]     (default: all)
//
// Remotion draws the frames as PNG files with headless Chrome; ffmpeg (from
// PATH, or FFMPEG=<path>) encodes them, so the film is compressed only once.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import sharp from "sharp";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const media = path.resolve(root, "../../docs/media");
const out = path.join(root, "out");
const ffmpeg = process.env.FFMPEG ?? "ffmpeg";
const entry = "src/index.ts";
const wanted = process.argv.slice(2);
const want = (name) => wanted.length === 0 || wanted.includes(name);

function run(command, args) {
  const result = spawnSync(command, args, { cwd: root, stdio: "inherit" });
  if (result.status !== 0) throw new Error(`${command} ${args[0]} failed`);
}

const remotion = (...args) => run(process.execPath, ["node_modules/@remotion/cli/remotion-cli.js", ...args]);

/** Draw every frame of a composition as `out/frames/<name>/element-NNNN.png`. */
function frames(composition, name) {
  const dir = path.join(out, "frames", name);
  fs.rmSync(dir, { recursive: true, force: true });
  remotion("render", entry, composition, dir, "--sequence", "--image-format=png", "--log=error");
  const first = fs.readdirSync(dir).filter((file) => file.endsWith(".png")).sort()[0];
  const digits = /(\d+)\.png$/.exec(first)[1].length;
  return path.join(dir, `element-%0${digits}d.png`);
}

fs.mkdirSync(media, { recursive: true });
run(process.execPath, ["scripts/assets.mjs"]);

if (want("film")) {
  const pattern = frames("Promo", "promo");
  run(ffmpeg, [
    "-v", "error", "-y", "-framerate", "30", "-i", pattern,
    // Flat interface colour: say exactly which RGB to YUV matrix is meant.
    "-vf", "scale=out_color_matrix=bt709:out_range=tv,format=yuv420p",
    "-c:v", "libx264", "-preset", "veryslow", "-crf", process.env.FILM_CRF ?? "21",
    "-profile:v", "high", "-level", "4.2", "-g", "60",
    "-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709",
    "-movflags", "+faststart", "-an",
    path.join(media, "baza-studio-promo.mp4"),
  ]);
}

if (want("loop")) {
  const pattern = frames("Loop", "loop");
  const palette = path.join(out, "loop-palette.png");
  const fps = process.env.LOOP_FPS ?? "20";
  const width = process.env.LOOP_WIDTH ?? "960";
  const colours = process.env.LOOP_COLOURS ?? "256";
  const scale = `fps=${fps},scale=${width}:-1:flags=lanczos`;
  // Two passes: first the palette of the whole loop, then the frames in it.
  run(ffmpeg, [
    "-v", "error", "-y", "-framerate", "30", "-i", pattern,
    "-vf", `${scale},palettegen=max_colors=${colours}:stats_mode=diff`,
    palette,
  ]);
  run(ffmpeg, [
    "-v", "error", "-y", "-framerate", "30", "-i", pattern, "-i", palette,
    "-lavfi", `${scale}[frames];[frames][1:v]paletteuse=dither=none:diff_mode=rectangle`,
    "-loop", "0",
    path.join(media, "baza-studio-promo.gif"),
  ]);
}

if (want("poster")) {
  remotion("still", entry, "Poster", path.join(media, "baza-studio-promo-poster.png"), "--image-format=png", "--log=error");
}

if (want("stills")) {
  const stills = {
    "screenshot-sql-editor.png": "sql/result",
    "screenshot-table-data.png": "table/start",
    "screenshot-ssh-tunnel.png": "stills/ssh",
    "screenshot-theme-midnight.png": "themes/midnight",
  };
  for (const [file, capture] of Object.entries(stills)) {
    await sharp(path.join(root, "public/captures", `${capture}.webp`))
      .png({ compressionLevel: 9, effort: 10 })
      .toFile(path.join(media, file));
  }
}

for (const file of fs.readdirSync(media).sort()) {
  const size = fs.statSync(path.join(media, file)).size;
  console.log(`${(size / 1024 / 1024).toFixed(2).padStart(7)} MB  docs/media/${file}`);
}
