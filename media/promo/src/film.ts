import { Camera, Film, FilmScript } from "./lib/timeline";

// Camera positions. `x`, `y` is the window point in the middle of the clear
// part of the frame; `zoom` is video pixels per window point.
const wide: Camera = { x: 640, y: 328, zoom: 1.2 };
const editor: Camera = { x: 600, y: 242, zoom: 2.0 };
const runButton: Camera = { x: 800, y: 290, zoom: 1.5 };
const results: Camera = { x: 764, y: 462, zoom: 1.62 };
const grid: Camera = { x: 640, y: 300, zoom: 1.8 };
const filterBar: Camera = { x: 760, y: 232, zoom: 1.66 };
const filtered: Camera = { x: 760, y: 300, zoom: 1.5 };
const dialog: Camera = { x: 640, y: 322, zoom: 1.5 };
// The top of the SSH tab, down to the host of the tunnel.
const sshTop: Camera = { x: 640, y: 127, zoom: 2.4 };
// The SSL/TLS tab from its tabs to the buttons, where the test answers.
const dialogLow: Camera = { x: 640, y: 430, zoom: 1.4 };
// The answer of the test and the buttons under it.
const testResult: Camera = { x: 640, y: 512, zoom: 2.0 };
const sidebar: Camera = { x: 470, y: 320, zoom: 1.7 };
const tour: Camera = { x: 700, y: 318, zoom: 1.3 };

/** Write a query, run it, look at the result. */
function sqlEditor(film: FilmScript) {
  film.show("sql/default");
  film.mark("sql");
  film.wait(16);
  film.caption("SQL editor", "Write a query", "Ctrl/Cmd + Enter runs the selection or the whole query");
  film.camera(editor, 36, undefined, 4);
  film.move("editorLine2", 22).click();
  film.wait(3);
  film.show("sql/selected");
  film.wait(8);
  film.drift([500, 318], 16, 4);
  film.type("sql/typed", 50);
  film.wait(5);
  film.camera(runButton, 22);
  film.move("run", 20);
  film.show("sql/run-hover");
  film.wait(3);
  film.click();
  film.show("sql/result");
  film.wait(5);
  film.caption("Results", "Typed results, straight away", "Every column with its type · export to CSV, JSON or SQL");
  film.camera(results, 30);
  film.drift([1130, 380], 24, 4);
  film.wait(24);
  film.highlight({ x: 1000, y: 431, w: 98, h: 24 }, 30);
  film.wait(4);
  film.push(20);
  film.wait(20);
}

/** Change one cell in place, then filter the rows. */
function tableData(film: FilmScript) {
  film.camera(wide, 24);
  film.move("tabTable", 20).click();
  film.show("table/open");
  film.mark("table");
  film.caption("Table data", "Edit a cell in place", "Double-click or F2 · type · Enter saves");
  film.wait(6);
  film.camera(grid, 28);
  film.move("cellName3", 20).click(true);
  film.show("table/cell");
  film.show("table/edit/000");
  film.drift([622, 356], 9, 1);
  film.wait(10);
  film.type("table/edit", 12);
  film.wait(10);
  film.show("table/saved");
  film.highlight({ x: 492, y: 299, w: 196, h: 30 }, 22);
  film.wait(21);
  film.caption("Table data", "Filter, sort and page", "Pick a column · type a value · filter");
  film.camera(filterBar, 22);
  film.move("columnMenu", 16).click();
  film.show("table/column-menu");
  film.wait(5);
  film.move("columnStatus", 8).click();
  film.show("table/column");
  film.wait(2);
  film.move("filterInput", 10).click();
  film.show("table/filter/000");
  film.wait(2);
  film.type("table/filter", 12);
  film.wait(3);
  film.move("filterButton", 11).click();
  film.show("table/filtered");
  film.camera(filtered, 24);
  film.wait(13);
}

/** A new connection through an SSH tunnel: filled in, tested for real, saved. */
function connection(film: FilmScript) {
  film.camera(wide, 22);
  film.move("newConnection", 22).click();
  film.show("connection/open");
  film.mark("connection");
  film.caption("Connections", "Add a connection", "PostgreSQL · MySQL / MariaDB · SQLite");
  film.camera(dialog, 24);
  film.wait(6);
  film.move("fieldName", 11).click();
  film.show("connection/name/000");
  film.type("connection/name", 13);
  film.move("engineMenu", 8).click();
  film.show("connection/engine-menu");
  film.wait(7);
  film.move("engineMariaDb", 6).click();
  film.show("connection/engine");
  film.wait(3);
  // Host, then the Tab key from field to field.
  film.move("fieldHost", 7).click();
  film.show("connection/host/000");
  film.wait(2);
  film.type("connection/host", 7);
  film.drift([880, 330], 14);
  film.wait(2);
  film.show("connection/port/000");
  film.type("connection/port", 4);
  film.wait(2);
  film.show("connection/user/000");
  film.type("connection/user", 6);
  film.wait(2);
  film.show("connection/password/000");
  film.type("connection/password", 5);
  film.wait(2);
  film.show("connection/database/000");
  film.type("connection/database", 4);
  film.wait(7);

  film.move("tabSsh", 13).click();
  film.show("ssh/tab");
  film.mark("ssh");
  film.caption("SSH", "SSH tunnel built in", "Part of every connection · password or private key");
  // The camera is in place before the fields of the tunnel appear.
  film.camera(sshTop, 20);
  film.wait(11);
  film.move("sshCheck", 10).click();
  film.show("ssh/on");
  film.wait(5);
  film.move("sshHost", 9).click();
  film.show("ssh/host/000");
  film.wait(2);
  film.type("ssh/host", 8);
  film.wait(12);

  film.move("tabTls", 11).click();
  film.show("tls/tab");
  film.mark("tls");
  film.caption("TLS", "TLS for database connections", "Verify the certificate, or require encryption");
  film.camera(dialogLow, 22);
  film.wait(8);
  film.move("tlsMode", 12).click();
  film.show("tls/menu");
  film.wait(9);
  film.move("tlsRequired", 7).click();
  film.show("tls/required");
  film.wait(9);
  film.move("testConnection", 16);
  film.show("tls/test-hover");
  film.wait(3);
  film.click();
  film.wait(5);
  film.show("tls/tested");
  film.mark("tested");
  film.caption("SSH", "Tested through the tunnel", "The server key is remembered · a later change is flagged");
  // Closer, so the answer of the application can be read.
  film.camera(testResult, 22);
  film.highlight({ x: 334, y: 498, w: 612, h: 52 }, 48);
  film.wait(54);
  film.move("dialogSave", 13);
  film.show("tls/save-hover");
  film.wait(3);
  film.click();
  film.show("connection/saved");
  film.mark("saved");
  film.caption("Vault", "Saved in an encrypted vault", "AES-256-GCM · the key is kept by the operating system");
  film.camera(sidebar, 24);
  film.wait(14);
  film.highlight({ x: 10, y: 348, w: 228, h: 46 }, 30);
  film.wait(8);
  film.push(18, 1.02);
  film.wait(18);
}

/** A quick pass over the other sections. */
function sections(film: FilmScript) {
  film.camera(tour, 22);
  film.move("tabBuilder", 20).click();
  film.show("tour/builder-grouped");
  film.mark("tour");
  film.caption("More tools", "Query builder", "Columns, a summary and grouping from one table");
  film.wait(6);
  film.move("showResult", 16).click();
  film.show("tour/builder-result");
  film.push(22, 1.02);
  film.wait(22);
  film.move("tabHistory", 16).click();
  film.show("tour/history");
  film.caption("More tools", "Query history", "Kept on this computer, encrypted in the local vault");
  film.push(38, 1.03);
  film.wait(38);
  film.move("tabTransfer", 12).click();
  film.show("tour/transfer");
  film.caption("More tools", "Import and export", "CSV · JSON · SQL");
  film.push(32, 1.025);
  film.wait(32);
}

/** The five themes, switched from the menu in the top bar. */
function themes(film: FilmScript) {
  film.camera(wide, 22);
  film.move("tabSql", 18).click();
  film.show("themes/vscode");
  film.mark("themes");
  film.caption("Appearance", "Five dark themes", "VS Code · Graphite · Midnight · OLED · Amethyst");
  film.light(0);
  film.wait(5);
  film.move("appearance", 20).click();
  film.show("themes/menu");
  film.wait(7);
  film.move("themeGraphite", 7).click();
  film.show("themes/graphite", { from: "appearance", frames: 14 });
  film.light(1);
  film.push(74, 1.045);
  film.drift([930, 330], 26, 4);
  film.wait(17);
  film.show("themes/midnight", { from: "appearance", frames: 14 });
  film.light(2);
  film.wait(16);
  film.show("themes/oled", { from: "appearance", frames: 14 });
  film.light(3);
  film.wait(16);
  film.show("themes/amethyst", { from: "appearance", frames: 14 });
  film.light(4);
  film.wait(23);
}

/** The part of the film that happens inside the application. */
export function applicationFilm(): Film {
  const film = new FilmScript(wide, [800, 360]);
  sqlEditor(film);
  tableData(film);
  connection(film);
  sections(film);
  themes(film);
  return film.build();
}

/** The short loop for the README: a query, its result, the themes. */
export function loopFilm(): Film {
  // Closer than in the film and exactly as wide as the results: the loop is shown small.
  const loopResults: Camera = { x: 764, y: 466, zoom: 1.86 };
  const film = new FilmScript(editor, [640, 300]);
  film.show("sql/default");
  film.wait(10);
  film.caption("Baza Studio", "Write a query", "Run it with Ctrl/Cmd + Enter");
  film.move("editorLine2", 16).click();
  film.wait(3);
  film.show("sql/selected");
  film.wait(8);
  film.drift([500, 318], 16, 4);
  film.type("sql/typed", 76);
  film.wait(8);
  film.camera(runButton, 24);
  film.move("run", 22);
  film.show("sql/run-hover");
  film.wait(3);
  film.click();
  film.show("sql/result");
  film.wait(6);
  film.caption("Baza Studio", "Typed results, straight away", "PostgreSQL · MySQL / MariaDB · SQLite");
  film.camera(loopResults, 30);
  film.drift([1130, 380], 24, 4);
  film.wait(72);
  film.hidePointer(400);
  film.show("themes/graphite", { from: "appearance", frames: 15 });
  film.caption("Baza Studio", "Native, free and open source", "Windows and macOS · GPL-3.0");
  film.wait(26);
  film.show("themes/midnight", { from: "appearance", frames: 15 });
  film.wait(26);
  film.show("themes/amethyst", { from: "appearance", frames: 15 });
  film.wait(36);
  return film.build();
}
