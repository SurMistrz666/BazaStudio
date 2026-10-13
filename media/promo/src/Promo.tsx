import React from "react";
import { AbsoluteFill, interpolate, useCurrentFrame } from "remotion";
import { Background } from "./components/Background";
import { Stage } from "./components/Stage";
import { applicationFilm } from "./film";
import { cameraAt, easeInOut, easeOut } from "./lib/timeline";
import { EndCard, endFrames } from "./scenes/EndCard";
import { Facts, factsFrames } from "./scenes/Facts";
import { Intro, introFrames } from "./scenes/Intro";

const film = applicationFilm();

const arrive = 34;
const leave = 22;
// The window comes in from below the frame once the title is gone: the two are never on screen together.
const appStart = introFrames;
const appEnd = appStart + film.duration;
const factsStart = appEnd - 6;
const endStart = factsStart + factsFrames - 8;

export const promoFrames = endStart + endFrames;
export const promoMarks = Object.fromEntries(
  Object.entries(film.marks).map(([name, at]) => [name, at + appStart]),
);

/** The whole film: title, the application at work, four facts, where to get it. */
export const Promo: React.FC = () => {
  const frame = useCurrentFrame();
  const local = frame - appStart;
  const inApp = frame >= appStart && frame < appEnd;
  const presence = inApp
    ? Math.min(
        interpolate(local, [0, arrive], [0, 1], {
          extrapolateLeft: "clamp",
          extrapolateRight: "clamp",
          easing: easeOut,
        }),
        interpolate(frame, [appEnd - leave, appEnd], [1, 0], {
          extrapolateLeft: "clamp",
          extrapolateRight: "clamp",
          easing: easeInOut,
        }),
      )
    : 0;
  const camera = cameraAt(film, Math.max(0, local));
  const shift = inApp
    ? { x: ((640 - camera.x) / 640) * 3, y: ((328 - camera.y) / 360) * 3 }
    : { x: 0, y: 0 };
  return (
    <AbsoluteFill>
      <Background shift={shift} />
      {frame < introFrames ? <Intro frame={frame} /> : null}
      {inApp ? (
        <Stage film={film} frame={local} presence={presence} leaving={frame > appEnd - leave} />
      ) : null}
      {frame >= factsStart && frame < factsStart + factsFrames ? (
        <Facts frame={frame - factsStart} />
      ) : null}
      {frame >= endStart ? <EndCard frame={frame - endStart} /> : null}
    </AbsoluteFill>
  );
};
