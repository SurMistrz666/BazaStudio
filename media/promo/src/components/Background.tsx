import React from "react";
import { AbsoluteFill } from "remotion";
import { color } from "../theme";

/**
 * The dark ground of the website with two soft lights. `shift` moves the
 * lights a little against the camera, which reads as depth.
 */
export const Background: React.FC<{ shift?: { x: number; y: number }; glow?: number }> = ({
  shift = { x: 0, y: 0 },
  glow = 1,
}) => (
  <AbsoluteFill style={{ background: color.background }}>
    <AbsoluteFill
      style={{
        opacity: glow,
        background: [
          `radial-gradient(900px 620px at ${22 + shift.x}% ${12 + shift.y}%, rgba(108,213,239,0.13), rgba(108,213,239,0) 70%)`,
          `radial-gradient(1000px 700px at ${84 + shift.x * 0.6}% ${86 + shift.y * 0.6}%, rgba(59,141,219,0.14), rgba(59,141,219,0) 70%)`,
        ].join(", "),
      }}
    />
    <AbsoluteFill
      style={{
        backgroundImage:
          "radial-gradient(rgba(255,255,255,0.055) 1px, rgba(255,255,255,0) 1.4px)",
        backgroundSize: "36px 36px",
        backgroundPosition: `${shift.x * 6}px ${shift.y * 6}px`,
        maskImage: "radial-gradient(ellipse 70% 65% at 50% 45%, #000 25%, transparent 100%)",
        WebkitMaskImage: "radial-gradient(ellipse 70% 65% at 50% 45%, #000 25%, transparent 100%)",
      }}
    />
  </AbsoluteFill>
);
