import React from "react";
import { AbsoluteFill, interpolate } from "remotion";
import { color, fontFamily } from "../theme";
import { easeOut } from "../lib/timeline";

export const factsFrames = 104;

const facts: { label: string; title: string; text: string }[] = [
  {
    label: "Engines",
    title: "PostgreSQL, MySQL / MariaDB, SQLite",
    text: "TLS for database connections.",
  },
  {
    label: "Platforms",
    title: "Windows and macOS",
    text: "Polish and English interface.",
  },
  {
    label: "Native",
    title: "Written in Rust",
    text: "egui and wgpu. No Electron, no browser engine.",
  },
  {
    label: "Vault",
    title: "Encrypted on your computer",
    text: "AES-256-GCM, key kept by DPAPI or the Keychain.",
  },
];

/** Four plain facts about the program. */
export const Facts: React.FC<{ frame: number }> = ({ frame }) => {
  const leave = interpolate(frame, [factsFrames - 12, factsFrames], [0, 1], {
    extrapolateLeft: "clamp",
    extrapolateRight: "clamp",
  });
  const head = interpolate(frame, [0, 16], [0, 1], {
    extrapolateLeft: "clamp",
    extrapolateRight: "clamp",
    easing: easeOut,
  });
  return (
    <AbsoluteFill
      style={{
        fontFamily,
        padding: "0 132px",
        justifyContent: "center",
        opacity: 1 - leave,
        transform: `scale(${1 - 0.03 * leave})`,
      }}
    >
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: 12,
          fontSize: 21,
          fontWeight: 600,
          letterSpacing: 2.8,
          textTransform: "uppercase",
          color: color.cyan,
          opacity: head,
          transform: `translateY(${16 * (1 - head)}px)`,
        }}
      >
        <span style={{ width: 8, height: 8, borderRadius: "50%", background: color.cyan }} />
        Baza Studio in four facts
      </div>
      <div
        style={{
          marginTop: 40,
          display: "grid",
          gridTemplateColumns: "1fr 1fr",
          gap: 28,
        }}
      >
        {facts.map((fact, index) => {
          const shown = interpolate(frame, [8 + index * 7, 28 + index * 7], [0, 1], {
            extrapolateLeft: "clamp",
            extrapolateRight: "clamp",
            easing: easeOut,
          });
          return (
            <div
              key={fact.label}
              style={{
                padding: "44px 48px 48px",
                borderRadius: 20,
                border: `1px solid ${color.line}`,
                background:
                  "linear-gradient(180deg, rgba(255,255,255,0.055), rgba(255,255,255,0.02))",
                opacity: shown,
                transform: `translateY(${34 * (1 - shown)}px)`,
              }}
            >
              <div
                style={{
                  fontSize: 20,
                  fontWeight: 600,
                  letterSpacing: 2.4,
                  textTransform: "uppercase",
                  color: color.blue,
                }}
              >
                {fact.label}
              </div>
              <div
                style={{
                  marginTop: 14,
                  fontSize: 47,
                  lineHeight: 1.12,
                  fontWeight: 600,
                  letterSpacing: -1.1,
                  color: color.text,
                }}
              >
                {fact.title}
              </div>
              <div
                style={{
                  marginTop: 16,
                  fontSize: 30,
                  lineHeight: 1.32,
                  color: color.muted,
                }}
              >
                {fact.text}
              </div>
            </div>
          );
        })}
      </div>
    </AbsoluteFill>
  );
};
