// Takes the footage of the film: real screenshots of the real application.
//
// Every picture comes from `BazaStudio --native-ui-self-test`, which opens the
// GPU window on its own temporary vault and a sample SQLite database, plays a
// `--qa-script-file` of real pointer and keyboard input and saves a screenshot
// at every `s:` step. Nothing here opens the vault of the person running it.
//
//   BAZA_STUDIO_BIN=<path to BazaStudio> node scripts/capture.mjs [session…]
//
// Sessions: `sql`, `table`, `tour`, `stills` (default: all four).
//
// The connection test of the film is real, so `table` and `tour` need an SSH
// server and a MariaDB behind it, as described in media/promo/README.md.
//
// Environment:
//   BAZA_STUDIO_BIN   the binary to film (default: native/target/debug/BazaStudio)
//   CAPTURE_SSH_KEY   private key file for the SSH server
//   CAPTURE_SSH_USER  account on the SSH server (typed off camera, never stored)
//   CAPTURE_SSH_PORT  port of the SSH server (typed off camera)
//   CAPTURE_DB_PASSWORD  password of the database account (shown as dots)
//   CAPTURE_SCALE     pixels per point of the screenshots, 1 or 2 (default 2)
//   CAPTURE_WORK      scratch directory for the raw screenshots
//   CAPTURE_REUSE     set to process the raw screenshots of the last run again
//                     without starting the application
//
// Output: `public/captures/**.webp` (lossless) and `src/data/captures.json`,
// which tells the film where each picture belongs in the window.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import sharp from "sharp";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const points = JSON.parse(fs.readFileSync(path.join(root, "src/data/points.json"), "utf8"));
const text = JSON.parse(fs.readFileSync(path.join(root, "src/data/text.json"), "utf8"));

const binary =
  process.env.BAZA_STUDIO_BIN ?? path.resolve(root, "../../native/target/debug/BazaStudio");
const scale = Number(process.env.CAPTURE_SCALE ?? 2);
const work = process.env.CAPTURE_WORK ?? path.join(root, "out/capture-work");
const output = path.join(root, "public/captures");
const manifestPath = path.join(root, "src/data/captures.json");
const { width, height } = points.window;

// --- Script steps -----------------------------------------------------------

const click = (name) => ({ kind: "c", at: points[name] });
const move = (name) => ({ kind: "m", at: points[name] });
const key = (name) => ({ kind: "k", name });
const wait = (ms) => ({ kind: "w", ms });
/** A value that is typed but never shown or stored with the film. */
const secret = (name) => {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is not set (see media/promo/README.md)`);
  if (/[;\n]/.test(value)) throw new Error(`${name} cannot be typed by a script`);
  return value;
};
/** A screenshot. `crop` (points) keeps only the part of the window that changed. */
const shot = (name, crop) => ({ kind: "s", name, crop });
/** Typed text with a screenshot after every `step` characters and every line end. */
const typed = (value, prefix, crop, step = 1) => {
  const steps = [];
  let index = 0;
  let pending = "";
  const flush = () => {
    if (pending === "") return;
    steps.push({ kind: "t", text: pending });
    pending = "";
  };
  const picture = () => {
    index += 1;
    steps.push(shot(`${prefix}/${String(index).padStart(3, "0")}`, crop));
  };
  for (const character of value) {
    if (character === ";") throw new Error("a script cannot type a semicolon");
    if (character === "\n") {
      flush();
      steps.push(key("enter"));
      picture();
      continue;
    }
    pending += character;
    if (pending.length >= step) {
      flush();
      picture();
    }
  }
  if (pending !== "") {
    flush();
    picture();
  }
  return steps;
};
/** The same input without the pictures: to bring a later run to the same state. */
const silent = (steps) => {
  const result = [];
  for (const step of steps) {
    if (step.kind === "s" || step.kind === "m") continue;
    const last = result.at(-1);
    if (step.kind === "t" && last?.kind === "t") {
      result[result.length - 1] = { kind: "t", text: last.text + step.text };
    } else {
      result.push(step);
    }
  }
  return result;
};

// Parts of the window, in points.
const area = {
  editor: { x: 248, y: 128, w: 760, h: 136 },
  sqlToolbar: { x: 1100, y: 78, w: 180, h: 48 },
  tableToolbar: { x: 248, y: 78, w: 1032, h: 48 },
  tableRow3: { x: 248, y: 298, w: 1032, h: 32 },
  dialog: { x: 316, y: 75, w: 648, h: 570 },
  // One text field of the connection dialog, around its middle.
  field: ([, y]) => ({ x: 534, y: y - 18, w: 406, h: 36 }),
  themeBar: { x: 960, y: 0, w: 320, h: 240 },
};

// The film is one sitting with the application. It is told here in parts.

/** Write a query and run it. */
const sqlPart = () => [
  click("toastClose"),
  shot("table/start"),
  click("tabSql"),
  shot("sql/default"),
  click("editorLine2"),
  key("cmd+a"),
  shot("sql/selected", area.editor),
  // Two characters a picture: the film shows them faster than that anyway.
  ...typed(text.sql, "sql/typed", area.editor, 2),
  move("run"),
  shot("sql/run-hover", area.sqlToolbar),
  click("run"),
  shot("sql/result"),
];

/** Change one cell, then filter the rows. */
const tablePart = () => [
  click("tabTable"),
  shot("table/open"),
  click("cellName3"),
  shot("table/cell"),
  key("f2"),
  shot("table/edit/000", area.tableRow3),
  ...typed(text.cellSuffix, "table/edit", area.tableRow3),
  key("enter"),
  shot("table/saved"),
  click("columnMenu"),
  shot("table/column-menu"),
  click("columnStatus"),
  shot("table/column"),
  click("filterInput"),
  shot("table/filter/000"),
  ...typed(text.filterText, "table/filter", area.tableToolbar),
  click("filterButton"),
  shot("table/filtered"),
];

/**
 * A new connection through an SSH tunnel, tested for real and saved into the
 * vault. The values that show are in `src/data/text.json`; the two that must
 * not show come from the environment and are typed where no picture is taken.
 */
const connectionPart = () => [
  click("newConnection"),
  shot("connection/open"),
  click("fieldName"),
  shot("connection/name/000", area.dialog),
  ...typed(text.connectionName, "connection/name", area.field(points.fieldName)),
  click("engineMenu"),
  shot("connection/engine-menu", area.dialog),
  click("engineMariaDb"),
  shot("connection/engine", area.dialog),
  click("fieldHost"),
  key("cmd+a"),
  shot("connection/host/000", area.dialog),
  ...typed(text.dbHost, "connection/host", area.field(points.fieldHost)),
  key("tab"),
  shot("connection/port/000", area.dialog),
  ...typed(text.dbPort, "connection/port", area.field(points.fieldPort)),
  key("tab"),
  shot("connection/user/000", area.dialog),
  ...typed(text.dbUser, "connection/user", area.field(points.fieldUser)),
  key("tab"),
  shot("connection/password/000", area.dialog),
  // The field shows dots only.
  ...typed(secret("CAPTURE_DB_PASSWORD"), "connection/password", area.field(points.fieldPassword), 4),
  key("tab"),
  shot("connection/database/000", area.dialog),
  ...typed(text.dbName, "connection/database", area.field(points.fieldDatabase)),
  click("tabSsh"),
  shot("ssh/tab", area.dialog),
  click("sshCheck"),
  shot("ssh/on", area.dialog),
  click("sshHost"),
  shot("ssh/host/000", area.dialog),
  ...typed(text.sshHost, "ssh/host", area.field(points.sshHost)),
  // No picture from here until the SSH tab is left: the account name of the
  // SSH server is not for the film.
  key("tab"),
  { kind: "t", text: secret("CAPTURE_SSH_PORT") },
  click("sshChooseKey"),
  click("sshUser"),
  { kind: "t", text: secret("CAPTURE_SSH_USER") },
  click("tabTls"),
  shot("tls/tab", area.dialog),
  click("tlsMode"),
  shot("tls/menu", area.dialog),
  click("tlsRequired"),
  shot("tls/required", area.dialog),
  move("testConnection"),
  shot("tls/test-hover", area.dialog),
  click("testConnection"),
  wait(2500),
  shot("tls/tested", area.dialog),
  move("dialogSave"),
  shot("tls/save-hover", area.dialog),
  click("dialogSave"),
  wait(600),
  shot("connection/saved"),
  click("toastClose"),
];

/** The SSH tab with example values, for the screenshot in the README. Nothing connects. */
const stillsPart = () => [
  click("toastClose"),
  click("newConnection"),
  click("fieldName"),
  { kind: "t", text: text.connectionName },
  click("tabSsh"),
  click("sshCheck"),
  click("sshHost"),
  { kind: "t", text: text.stillSshHost },
  click("sshUser"),
  { kind: "t", text: text.stillSshUser },
  click("sshChooseKey"),
  move("dialogSave"),
  shot("stills/ssh"),
];

/** The other sections, then the themes on the query from the beginning. */
const tourPart = () => [
  click("builderLink"),
  click("builderColumnStatus"),
  click("summaryMenu"),
  click("summaryCount"),
  click("groupMenu"),
  click("groupStatus"),
  move("showResult"),
  shot("tour/builder-grouped"),
  click("showResult"),
  shot("tour/builder-result"),
  click("tabHistory"),
  shot("tour/history"),
  click("tabTransfer"),
  shot("tour/transfer"),
  click("tabSql"),
  shot("themes/vscode"),
  click("appearance"),
  shot("themes/menu", area.themeBar),
  click("themeGraphite"),
  shot("themes/graphite"),
  click("appearance"),
  click("themeMidnight"),
  shot("themes/midnight"),
  click("appearance"),
  click("themeOled"),
  shot("themes/oled"),
  click("appearance"),
  click("themeAmethyst"),
  shot("themes/amethyst"),
];

/**
 * Each session is one run of the application, short enough to be taken again
 * when something disturbed it. A later session first repeats the input of the
 * earlier ones without pictures, so the query, the edited cell, the saved
 * connection and the history are there when its own pictures are taken.
 */
const sessions = {
  sql: () => sqlPart(),
  table: () => [...silent(sqlPart()), ...tablePart(), ...connectionPart()],
  tour: () => [...silent([...sqlPart(), ...tablePart(), ...connectionPart()]), ...tourPart()],
  stills: () => stillsPart(),
};

// --- Running the application ------------------------------------------------

const stepText = (step) => {
  switch (step.kind) {
    case "c":
    case "m":
      return `${step.kind}:${step.at[0]},${step.at[1]}`;
    case "k":
      return `k:${step.name}`;
    case "w":
      return `w:${step.ms}`;
    case "t":
      return `t:${step.text}`;
    case "s":
      // A breath after every screenshot: one that is still being read back
      // when the next is asked for has been seen to take the wrong name.
      return `s:${step.file}\nw:300`;
    default:
      throw new Error(`unknown step ${step.kind}`);
  }
};

const sleep = (ms) => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);

function launch(report, steps) {
  const scriptFile = report.replace(/\.json$/, ".steps");
  fs.writeFileSync(scriptFile, `${steps.map(stepText).join("\n")}\n`);
  const args = [
    "--native-ui-self-test",
    "--report",
    report,
    "--qa-view",
    "table",
    "--qa-lang",
    "en",
    "--qa-theme",
    "vscode",
    "--qa-size",
    `${width}x${height}`,
    "--qa-scale",
    String(scale),
    "--qa-script-file",
    scriptFile,
  ];
  // The binary is rebuilt from time to time; a run that fails is tried again.
  for (let attempt = 1; ; attempt += 1) {
    const run = spawnSync(binary, args, {
      stdio: ["ignore", "ignore", "pipe"],
      timeout: 1_800_000,
      // The private key that the button "Choose file…" of the SSH tab loads.
      env: { ...process.env, BAZA_QA_SSH_KEY_FILE: secret("CAPTURE_SSH_KEY") },
    });
    if (run.status === 0) return;
    if (attempt >= 5) {
      throw new Error(`BazaStudio failed (${run.status}): ${run.stderr?.toString().trim()}`);
    }
    sleep(8000);
  }
}

// --- Pictures ---------------------------------------------------------------

const pixels = async (file) =>
  sharp(file).ensureAlpha().raw().toBuffer({ resolveWithObject: true });

/** How two screenshots differ: how many pixels, and where (in points). */
async function difference(file, before) {
  const [a, b] = await Promise.all([pixels(file), pixels(before)]);
  if (a.info.width !== b.info.width || a.info.height !== b.info.height) return null;
  const box = { x0: Infinity, y0: Infinity, x1: -1, y1: -1 };
  let count = 0;
  for (let i = 0; i < a.data.length; i += 4) {
    if (
      Math.abs(a.data[i] - b.data[i]) > 2 ||
      Math.abs(a.data[i + 1] - b.data[i + 1]) > 2 ||
      Math.abs(a.data[i + 2] - b.data[i + 2]) > 2
    ) {
      const x = (i / 4) % a.info.width;
      const y = Math.floor(i / 4 / a.info.width);
      count += 1;
      box.x0 = Math.min(box.x0, x);
      box.y0 = Math.min(box.y0, y);
      box.x1 = Math.max(box.x1, x);
      box.y1 = Math.max(box.y1, y);
    }
  }
  const p = (v) => v / scale;
  return { count, x0: p(box.x0), y0: p(box.y0), x1: p(box.x1 + 1), y1: p(box.y1 + 1) };
}

/**
 * Look for the marks of a disturbed run before any picture is kept. Returns
 * the problems found; an empty list means the run can be used.
 */
async function problems(report, shots) {
  const found = [];
  const names = JSON.parse(fs.readFileSync(report, "utf8")).qa_shots ?? [];
  const expected = shots.map((step) => step.file);
  if (names.join() !== expected.join()) {
    found.push("the report does not list every screenshot exactly once, in order");
  }
  let before = null;
  for (const step of shots) {
    if (!fs.existsSync(step.raw)) {
      found.push(`${step.name}: no screenshot was written`);
      continue;
    }
    const meta = await sharp(step.raw).metadata();
    if (meta.width !== width * scale || meta.height !== height * scale) {
      found.push(
        `${step.name}: screenshot is ${meta.width}x${meta.height}, expected ${width * scale}x${height * scale}` +
          " (was the window moved to a display with another pixel density?)",
      );
      continue;
    }
    if (before) {
      const change = await difference(step.raw, before.raw);
      if (change.count === 0) {
        // Every picture is taken after input that shows: no change means the
        // click or the typed text before it never arrived.
        found.push(`${step.name}: nothing changed since ${before.name}`);
      } else if (step.crop) {
        const { x, y, w, h } = step.crop;
        const inside =
          change.x0 >= x && change.y0 >= y && change.x1 <= x + w && change.y1 <= y + h;
        if (!inside) {
          step.stray = `changes reach ${change.x0},${change.y0} to ${change.x1},${change.y1}, outside its crop`;
        }
      }
    }
    before = step;
  }
  return found;
}

async function store(step) {
  const file = path.join(output, `${step.name}.webp`);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  let image = sharp(step.raw);
  const { crop } = step;
  if (crop) {
    image = image.extract({
      left: crop.x * scale,
      top: crop.y * scale,
      width: crop.w * scale,
      height: crop.h * scale,
    });
  }
  await image.webp({ lossless: true, effort: 6 }).toFile(file);
  return crop ? { x: crop.x, y: crop.y, w: crop.w, h: crop.h } : { x: 0, y: 0, w: width, h: height };
}

const manifest = fs.existsSync(manifestPath)
  ? JSON.parse(fs.readFileSync(manifestPath, "utf8"))
  : {};
if (manifest.scale !== scale || manifest.window?.width !== width) manifest.shots = {};
manifest.window = { width, height };
manifest.scale = scale;
manifest.shots ??= {};

async function runSession(name) {
  const steps = sessions[name]().map((step, index) =>
    step.kind === "s" ? { ...step, file: `shot-${String(index).padStart(4, "0")}` } : step,
  );
  const dir = path.join(work, name);
  fs.mkdirSync(dir, { recursive: true });
  const report = path.join(dir, "session.json");
  const shots = steps.filter((step) => step.kind === "s");
  for (const step of shots) step.raw = path.join(dir, `session-${step.file}.png`);
  console.log(`session ${name}: ${shots.length} pictures`);
  for (let attempt = 1; ; attempt += 1) {
    if (!process.env.CAPTURE_REUSE) {
      for (const step of shots) fs.rmSync(step.raw, { force: true });
      launch(report, steps);
    }
    const found = await problems(report, shots);
    if (found.length === 0) break;
    console.warn(`  attempt ${attempt} was disturbed:`);
    for (const line of found.slice(0, 6)) console.warn(`    ${line}`);
    if (process.env.CAPTURE_REUSE || attempt >= 4) {
      throw new Error(`session ${name} could not be taken cleanly`);
    }
  }
  for (const step of shots) {
    manifest.shots[step.name] = await store(step);
    if (step.stray) console.warn(`  warning: ${step.name}: ${step.stray}`);
  }
}

const wanted = process.argv.slice(2);
// With every session taken again, pictures of an earlier story must not stay behind.
if (wanted.length === 0) {
  fs.rmSync(output, { recursive: true, force: true });
  manifest.shots = {};
}
for (const name of wanted.length > 0 ? wanted : Object.keys(sessions)) {
  if (!sessions[name]) throw new Error(`unknown session ${name}`);
  await runSession(name);
}

// Sequences of typed text, in order, for the film.
const sequences = {};
for (const name of Object.keys(manifest.shots).sort()) {
  const match = /^(.*)\/(\d{3})$/.exec(name);
  if (!match) continue;
  (sequences[match[1]] ??= []).push(name);
}
manifest.sequences = sequences;
manifest.shots = Object.fromEntries(
  Object.entries(manifest.shots).sort(([a], [b]) => a.localeCompare(b)),
);
fs.writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`wrote ${path.relative(root, manifestPath)}: ${Object.keys(manifest.shots).length} pictures`);
