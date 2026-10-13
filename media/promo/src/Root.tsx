import React from "react";
import { Composition, Still } from "remotion";
import "./fonts";
import { Loop, loopFrames } from "./Loop";
import { Poster } from "./Poster";
import { Promo, promoFrames } from "./Promo";
import { video } from "./theme";

export const Root: React.FC = () => (
  <>
    <Composition
      id="Promo"
      component={Promo}
      durationInFrames={promoFrames}
      fps={video.fps}
      width={video.width}
      height={video.height}
    />
    <Composition
      id="Loop"
      component={Loop}
      durationInFrames={loopFrames}
      fps={video.fps}
      width={video.width}
      height={video.height}
    />
    <Still id="Poster" component={Poster} width={video.width} height={video.height} />
  </>
);
