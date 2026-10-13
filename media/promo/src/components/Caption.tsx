import React from "react";
import { interpolate } from "remotion";
import { color, fontFamily, video } from "../theme";
import { Caption as CaptionData, Film, easeOut } from "../lib/timeline";

const enter = 12;
const leave = 8;

const Line: React.FC<{ caption: CaptionData; frame: number }> = ({ caption, frame }) => {
  const shown = interpolate(frame, [caption.from, caption.from + enter], [0, 1], {
    extrapolateLeft: "clamp",
    extrapolateRight: "clamp",
    easing: easeOut,
  });
  const gone = interpolate(frame, [caption.to - leave, caption.to], [0, 1], {
    extrapolateLeft: "clamp",
    extrapolateRight: "clamp",
  });
  const opacity = shown * (1 - gone);
  if (opacity <= 0) return null;
  const rise = (delay: number) =>
    18 *
    (1 -
      interpolate(frame, [caption.from + delay, caption.from + delay + enter], [0, 1], {
        extrapolateLeft: "clamp",
        extrapolateRight: "clamp",
        easing: easeOut,
      }));
  let lit = -1;
  for (const mark of caption.lit ?? []) if (mark.at <= frame) lit = mark.index;
  const words = caption.sub?.split(" · ") ?? [];
  return (
    <div
      style={{
        position: "absolute",
        left: 120,
        right: 120,
        bottom: 62,
        opacity,
        fontFamily,
      }}
    >
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: 12,
          fontSize: 19,
          fontWeight: 600,
          letterSpacing: 2.6,
          textTransform: "uppercase",
          color: color.cyan,
          transform: `translateY(${rise(0)}px)`,
        }}
      >
        <span
          style={{ width: 7, height: 7, borderRadius: "50%", background: color.cyan, flex: "none" }}
        />
        {caption.eyebrow}
      </div>
      <div
        style={{
          marginTop: 12,
          fontSize: 50,
          lineHeight: 1.12,
          fontWeight: 600,
          letterSpacing: -1.3,
          color: color.text,
          transform: `translateY(${rise(2)}px)`,
        }}
      >
        {caption.title}
      </div>
      {caption.sub ? (
        <div
          style={{
            marginTop: 12,
            fontSize: 27,
            lineHeight: 1.25,
            fontWeight: 400,
            letterSpacing: -0.2,
            color: color.muted,
            transform: `translateY(${rise(4)}px)`,
          }}
        >
          {caption.lit
            ? words.map((word, index) => (
                <React.Fragment key={word}>
                  {index > 0 ? <span style={{ color: color.faint }}> · </span> : null}
                  <span
                    style={{
                      color: index === lit ? color.cyan : color.muted,
                      fontWeight: index === lit ? 500 : 400,
                    }}
                  >
                    {word}
                  </span>
                </React.Fragment>
              ))
            : caption.sub}
        </div>
      ) : null}
    </div>
  );
};

/** The shade under the captions, so they never fight with the window above. */
export const Scrim: React.FC<{ opacity?: number }> = ({ opacity = 1 }) => (
  <div
    style={{
      position: "absolute",
      left: 0,
      top: video.height - 330,
      width: video.width,
      height: 330,
      opacity,
      background: `linear-gradient(to bottom, rgba(13,15,19,0) 0%, rgba(13,15,19,0.82) 20%, ${color.backgroundDeep} 35%)`,
    }}
  />
);

/** `scale` enlarges the captions for the small loop of the README. */
export const Captions: React.FC<{ film: Film; frame: number; scale?: number }> = ({
  film,
  frame,
  scale = 1,
}) => (
  <div
    style={{
      position: "absolute",
      inset: 0,
      transform: `scale(${scale})`,
      transformOrigin: "120px 1018px",
    }}
  >
    {film.captions.map((caption) => (
      <Line key={caption.from} caption={caption} frame={frame} />
    ))}
  </div>
);
