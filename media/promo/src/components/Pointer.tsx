import React from "react";
import { anchor } from "../theme";
import { Camera, Film, pointerAt } from "../lib/timeline";

/**
 * The pointer of the film. It is drawn by the film, not captured: it travels
 * between the points where the real clicks of the capture script landed.
 */
export const Pointer: React.FC<{ film: Film; frame: number; camera: Camera }> = ({
  film,
  frame,
  camera,
}) => {
  if (film.pointer.hidden.some((span) => frame >= span.from && frame < span.to)) return null;
  const [x, y] = pointerAt(film, frame);
  const left = anchor.x + (x - camera.x) * camera.zoom;
  const top = anchor.y + (y - camera.y) * camera.zoom;
  // The pointer dips for a moment when the button goes down.
  let press = 1;
  for (const click of film.clicks) {
    const age = frame - click.at;
    if (age >= -2 && age <= 5) press = Math.min(press, 0.86 + 0.14 * Math.abs(age - 1) / 4);
    if (click.double && age >= 2 && age <= 9) {
      press = Math.min(press, 0.86 + 0.14 * Math.abs(age - 5) / 4);
    }
  }
  const size = 1.3 * Math.min(1.2, Math.max(1, camera.zoom / 1.5));
  return (
    <svg
      width={40}
      height={48}
      viewBox="0 0 20 24"
      style={{
        position: "absolute",
        left,
        top,
        transform: `scale(${size * press})`,
        transformOrigin: "0 0",
        filter: "drop-shadow(0 3px 5px rgba(0,0,0,0.55))",
        overflow: "visible",
      }}
    >
      <path
        d="M1 1v16.2l4.3-3.9 3 6.9 2.9-1.3-3-6.7h5.9L1 1z"
        fill="#0b0d11"
        stroke="#ffffff"
        strokeWidth={1.25}
        strokeLinejoin="round"
      />
    </svg>
  );
};
