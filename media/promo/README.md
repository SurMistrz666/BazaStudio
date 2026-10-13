# Promotional film

The source of the film, the GIF and the screenshots in `docs/media/`. The film is a [Remotion](https://www.remotion.dev) project: React components drawn frame by frame in headless Chrome and encoded with ffmpeg.

Everything that is visible inside the application window is a real capture of the real program. The film adds only what lies on top: the pointer, the click rings, the highlight outlines, the camera and the captions.

## Layout

| Path | What it is |
| --- | --- |
| `scripts/capture.mjs` | Takes the footage: runs of `BazaStudio --native-ui-self-test` with a `--qa-script-file` of real clicks and typed text, and a screenshot after every step that matters. |
| `public/captures/` | The footage, lossless WebP at two pixels per point. Full-window pictures, and for typed text only the part of the window that changed. |
| `src/data/captures.json` | Where each picture belongs in the window. Written by the capture script. |
| `src/data/points.json`, `src/data/text.json` | The points that are clicked and the text that is typed, shared by the capture script and the film so the drawn pointer goes where the real click went. |
| `src/film.ts` | The film as a script: show a capture, move the pointer, click, type, move the camera, show a caption. |
| `src/lib/timeline.ts` | The small engine behind that script. |
| `src/components/`, `src/scenes/` | The window frame, pointer, captions, title, facts and end card. |
| `scripts/render.mjs` | Renders the deliverables into `docs/media/`. |
| `scripts/assets.mjs` | Copies Inter and the icon from `native/assets/` into `public/`. |

## Rendering

Node.js 22 or newer and ffmpeg with libx264 on `PATH`. Remotion downloads its own headless Chrome on the first render. On macOS the installed Google Chrome is used instead when it is there (`remotion.config.ts`): on macOS 27 the downloaded headless shell never delivered an animation frame and every render timed out. `CHROME=<path>` picks another browser, `CHROME=remotion` the downloaded one.

```text
cd media/promo
npm ci
npm run render                # everything
npm run render -- film        # only docs/media/baza-studio-promo.mp4
npm run render -- loop        # only docs/media/baza-studio-promo.gif
npm run render -- poster      # only docs/media/baza-studio-promo-poster.png
npm run render -- stills      # only docs/media/screenshot-*.png
npm run studio                # preview and scrub in the browser
```

`FILM_CRF` (default 21) sets the quality of the MP4; `LOOP_FPS`, `LOOP_WIDTH` and `LOOP_COLOURS` (defaults 20, 960, 256) set the size of the GIF.

## Taking the footage again

Needed after the interface changes. Build the application first, then:

```text
cd media/promo
export BAZA_STUDIO_BIN=../../native/target/debug/BazaStudio
export CAPTURE_SSH_KEY=<private key file>  CAPTURE_SSH_USER=<account>  CAPTURE_SSH_PORT=<port>
export CAPTURE_DB_PASSWORD=<password>
npm run capture               # all sessions
npm run capture -- tour       # one session only: sql, table, tour or stills
```

The script opens the real GPU window on a disposable vault and the sample SQLite database of the QA mode; it never opens your own vault. The story is taken in three runs (`sql`, `table`, `tour`) of two to six minutes each; a later run first repeats the input of the earlier ones, so the query, the edited cell, the saved connection and the history are all there. A fourth run, `stills`, takes the SSH tab with example values for the screenshot in the main README; it connects to nothing.

The connection test in the film is real: "Test connection" is pressed and the application answers through an SSH tunnel. The `table` and `tour` runs therefore need a rig on the loopback interface:

- an SSH server that accepts the key in `CAPTURE_SSH_KEY` for the account `CAPTURE_SSH_USER` on port `CAPTURE_SSH_PORT`, reachable as the host in `sshHost` of `src/data/text.json`;
- a MariaDB with TLS that the SSH server can reach, with the host, port, account and database named in `src/data/text.json` (`dbHost`, `dbPort`, `dbUser`, `dbName`) and the password in `CAPTURE_DB_PASSWORD`.

What is typed from the environment never reaches the footage: the password field shows dots, and the port and account of the SSH server are typed after the last picture of the SSH tab, before the script moves on to the SSL/TLS tab. That is why the film shows the SSH tab only down to its host field.

Leave the computer alone while it runs. The script checks every run before it keeps a picture: each screenshot must differ from the one before it, must have the expected size, and must be listed once in the report. A run that fails the check is taken again, up to four times. A change outside the part of the window a picture is cropped to is printed as a warning; look at those pictures before rendering.

The switches of the QA mode are described at the top of `native/crates/baza-studio/src/app/qa.rs`. If a control moved, correct its point in `src/data/points.json` (points of a 1280 x 720 window, from the top-left corner).

## Licences

The project is part of Baza Studio and is under GPL-3.0-or-later like the rest of the repository.

Remotion has its own licence, which is not an open-source one: it is free for individuals, for non-profit organisations and for companies with up to three employees, and larger companies need a company licence (`node_modules/remotion/LICENSE.md`, <https://www.remotion.dev/license>). It is used here only to render the film and is not part of the application. Inter is under the SIL Open Font License 1.1.
