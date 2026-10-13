import React from "react";
import { AbsoluteFill } from "remotion";
import { AppWindow } from "./components/AppWindow";
import { Background } from "./components/Background";
import { Icon } from "./components/Brand";
import { FilmScript } from "./lib/timeline";
import { color, fontFamily } from "./theme";

const camera = { x: 150, y: 250, zoom: 1.04 };
const script = new FilmScript(camera, [0, 0]);
script.show("sql/result");
script.wait(1);
const film = script.build();

/** The still picture used as the poster of the film. */
export const Poster: React.FC = () => (
  <AbsoluteFill>
    <Background />
    <AbsoluteFill
      style={{
        transform: "perspective(2600px) rotateY(-9deg) rotateX(2deg)",
        transformOrigin: "1500px 520px",
      }}
    >
      <AppWindow film={film} frame={0} camera={camera} />
    </AbsoluteFill>
    <AbsoluteFill
      style={{
        background: `linear-gradient(90deg, ${color.background} 0%, rgba(20,23,29,0.94) 30%, rgba(20,23,29,0) 52%)`,
      }}
    />
    <AbsoluteFill style={{ fontFamily, padding: "0 0 0 120px", justifyContent: "center" }}>
      <Icon size={150} style={{ marginLeft: -14 }} />
      <div
        style={{
          marginTop: 10,
          fontSize: 92,
          fontWeight: 600,
          letterSpacing: -3.2,
          color: color.text,
        }}
      >
        Baza Studio
      </div>
      <div
        style={{
          marginTop: 16,
          width: 640,
          fontSize: 38,
          lineHeight: 1.22,
          letterSpacing: -0.6,
          color: "#c3cad6",
        }}
      >
        Your databases. One native workbench.
      </div>
      <div style={{ marginTop: 30, fontSize: 25, lineHeight: 1.5, color: color.muted }}>
        PostgreSQL · MySQL / MariaDB · SQLite
        <br />
        Windows and macOS · free and open source
      </div>
    </AbsoluteFill>
  </AbsoluteFill>
);
