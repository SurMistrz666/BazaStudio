import React from "react";
import { AbsoluteFill, interpolate, useCurrentFrame } from "remotion";
import { Background } from "./components/Background";
import { Stage } from "./components/Stage";
import { loopFilm } from "./film";
import { color } from "./theme";

const film = loopFilm();
export const loopFrames = film.duration;

/** The short loop shown inline in the README. It fades through the ground at the seam. */
export const Loop: React.FC = () => {
  const frame = useCurrentFrame();
  const seam = interpolate(
    frame,
    [0, 8, film.duration - 9, film.duration - 1],
    [1, 0, 0, 1],
    { extrapolateLeft: "clamp", extrapolateRight: "clamp" },
  );
  return (
    <AbsoluteFill>
      <Background />
      <Stage film={film} frame={frame} captionScale={1.22} />
      <AbsoluteFill style={{ background: color.background, opacity: seam }} />
    </AbsoluteFill>
  );
};
