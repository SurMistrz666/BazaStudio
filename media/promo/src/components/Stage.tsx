import React from "react";
import { AbsoluteFill } from "remotion";
import { video } from "../theme";
import { Film, cameraAt } from "../lib/timeline";
import { AppWindow } from "./AppWindow";
import { Captions, Scrim } from "./Caption";
import { Pointer } from "./Pointer";

/**
 * The window, the pointer and the captions of one frame of an application
 * film. `presence` is 0 while the window is away (before it arrives and after
 * it leaves) and 1 while it is in place; `leaving` picks the direction.
 */
export const Stage: React.FC<{
  film: Film;
  frame: number;
  presence?: number;
  leaving?: boolean;
  captionScale?: number;
}> = ({ film, frame, presence = 1, leaving = false, captionScale = 1 }) => {
  const camera = cameraAt(film, frame);
  const away = 1 - presence;
  // Arriving: the window slides up from below the frame, tilted back.
  // Leaving: it sinks away from the viewer and dims.
  const transform = leaving
    ? `perspective(2600px) translateY(${-40 * away}px) scale(${1 - 0.14 * away}) rotateX(${10 * away}deg)`
    : `perspective(2600px) translateY(${1000 * away}px) scale(${1 - 0.06 * away}) rotateX(${-10 * away}deg)`;
  return (
    <AbsoluteFill>
      <AbsoluteFill
        style={{
          transform,
          transformOrigin: `${video.width / 2}px ${video.height * 0.46}px`,
          // Arriving, the window is solid from its first frame: it slides, it does not fade.
          opacity: leaving ? Math.min(1, presence * 1.6) : 1,
          filter: leaving && away > 0 ? `blur(${10 * away}px)` : undefined,
        }}
      >
        <AppWindow film={film} frame={frame} camera={camera} />
        <div style={{ opacity: Math.max(0, Math.min(1, (presence - 0.9) * 10)) }}>
          <Pointer film={film} frame={frame} camera={camera} />
        </div>
      </AbsoluteFill>
      <Scrim opacity={presence} />
      <Captions film={film} frame={frame} scale={captionScale} />
    </AbsoluteFill>
  );
};
