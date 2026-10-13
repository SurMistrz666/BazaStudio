import React from "react";
import { AbsoluteFill, interpolate, spring, useVideoConfig } from "remotion";
import { Icon } from "../components/Brand";
import { color, fontFamily } from "../theme";
import { easeInOut, easeOut } from "../lib/timeline";

export const introFrames = 80;
/** The frame on which the title starts to make room for the window. */
export const introLeaves = 68;

/** Icon, name and the one-line promise. */
export const Intro: React.FC<{ frame: number }> = ({ frame }) => {
  const { fps } = useVideoConfig();
  const pop = spring({ frame, fps, config: { damping: 16, stiffness: 120, mass: 0.9 } });
  const leave = interpolate(frame, [introLeaves, introFrames], [0, 1], {
    extrapolateLeft: "clamp",
    extrapolateRight: "clamp",
    easing: easeInOut,
  });
  const rise = (from: number) =>
    interpolate(frame, [from, from + 18], [0, 1], {
      extrapolateLeft: "clamp",
      extrapolateRight: "clamp",
      easing: easeOut,
    });
  const name = rise(8);
  const promise = rise(20);
  const engines = rise(32);
  return (
    <AbsoluteFill
      style={{
        alignItems: "center",
        justifyContent: "center",
        fontFamily,
        opacity: 1 - leave,
        transform: `translateY(${-70 * leave}px) scale(${1 - 0.06 * leave})`,
      }}
    >
      <div
        style={{
          transform: `scale(${0.6 + 0.4 * pop}) rotate(${-6 * (1 - pop)}deg)`,
          opacity: Math.min(1, pop * 1.4),
          filter: "drop-shadow(0 26px 60px rgba(59,141,219,0.28))",
        }}
      >
        <Icon size={236} />
      </div>
      <div
        style={{
          marginTop: 6,
          fontSize: 104,
          fontWeight: 600,
          letterSpacing: -3.6,
          color: color.text,
          opacity: name,
          transform: `translateY(${22 * (1 - name)}px)`,
        }}
      >
        Baza Studio
      </div>
      <div
        style={{
          marginTop: 14,
          fontSize: 42,
          fontWeight: 400,
          letterSpacing: -0.7,
          color: "#c3cad6",
          opacity: promise,
          transform: `translateY(${18 * (1 - promise)}px)`,
        }}
      >
        Your databases. One native workbench.
      </div>
      <div
        style={{
          marginTop: 38,
          display: "flex",
          gap: 14,
          opacity: engines,
          transform: `translateY(${14 * (1 - engines)}px)`,
        }}
      >
        {["PostgreSQL", "MySQL / MariaDB", "SQLite"].map((engine) => (
          <div
            key={engine}
            style={{
              padding: "11px 22px",
              borderRadius: 999,
              border: `1px solid ${color.line}`,
              background: "rgba(255,255,255,0.035)",
              fontSize: 25,
              fontWeight: 500,
              color: color.muted,
            }}
          >
            {engine}
          </div>
        ))}
      </div>
    </AbsoluteFill>
  );
};
