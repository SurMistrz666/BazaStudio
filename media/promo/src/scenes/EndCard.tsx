import React from "react";
import { AbsoluteFill, interpolate, spring, useVideoConfig } from "remotion";
import { Icon } from "../components/Brand";
import { color, fontFamily } from "../theme";
import { easeOut } from "../lib/timeline";

export const endFrames = 98;

/** Where to get it, and on which terms. */
export const EndCard: React.FC<{ frame: number }> = ({ frame }) => {
  const { fps } = useVideoConfig();
  const pop = spring({ frame, fps, config: { damping: 18, stiffness: 110 } });
  const rise = (from: number) =>
    interpolate(frame, [from, from + 18], [0, 1], {
      extrapolateLeft: "clamp",
      extrapolateRight: "clamp",
      easing: easeOut,
    });
  const title = rise(6);
  const licence = rise(14);
  const address = rise(22);
  const foot = rise(32);
  return (
    <AbsoluteFill style={{ alignItems: "center", justifyContent: "center", fontFamily }}>
      <div
        style={{
          transform: `scale(${0.7 + 0.3 * pop})`,
          opacity: Math.min(1, pop * 1.5),
          filter: "drop-shadow(0 22px 50px rgba(59,141,219,0.26))",
        }}
      >
        <Icon size={176} />
      </div>
      <div
        style={{
          marginTop: 4,
          fontSize: 84,
          fontWeight: 600,
          letterSpacing: -2.8,
          color: color.text,
          opacity: title,
          transform: `translateY(${18 * (1 - title)}px)`,
        }}
      >
        Baza Studio
      </div>
      <div
        style={{
          marginTop: 14,
          fontSize: 40,
          fontWeight: 500,
          letterSpacing: -0.6,
          color: color.cyan,
          opacity: licence,
          transform: `translateY(${16 * (1 - licence)}px)`,
        }}
      >
        Free and open source · GPL-3.0
      </div>
      <div
        style={{
          marginTop: 42,
          padding: "20px 38px",
          borderRadius: 16,
          border: "1px solid rgba(108,213,239,0.32)",
          background: "rgba(108,213,239,0.07)",
          fontSize: 39,
          fontWeight: 500,
          letterSpacing: -0.4,
          color: color.text,
          opacity: address,
          transform: `translateY(${16 * (1 - address)}px)`,
        }}
      >
        github.com/SurMistrz666/BazaStudio
      </div>
      <div
        style={{
          marginTop: 34,
          fontSize: 25,
          color: color.muted,
          opacity: foot,
        }}
      >
        Early preview 0.4.0-preview.1 · Windows and macOS
      </div>
    </AbsoluteFill>
  );
};
