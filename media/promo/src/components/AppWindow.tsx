import React from "react";
import { Img, interpolate, staticFile } from "remotion";
import captures from "../data/captures.json";
import { anchor, color, fontFamily, titleBar } from "../theme";
import { Camera, Film, easeInOut, easeOut, shotRect, windowAt } from "../lib/timeline";

const { width, height } = captures.window;
const file = (name: string) => staticFile(`captures/${name}.webp`);

const Shot: React.FC<{ name: string; style?: React.CSSProperties }> = ({ name, style }) => {
  const rect = shotRect(name);
  return (
    <Img
      src={file(name)}
      style={{
        position: "absolute",
        left: rect.x,
        top: rect.y,
        width: rect.w,
        height: rect.h,
        ...style,
      }}
    />
  );
};

/** The ring a click leaves behind. It lives in the window, so it follows the camera. */
const Ripple: React.FC<{ x: number; y: number; age: number }> = ({ x, y, age }) => {
  if (age < 0 || age > 16) return null;
  const t = easeOut(age / 16);
  const radius = 5 + 19 * t;
  return (
    <div
      style={{
        position: "absolute",
        left: x - radius,
        top: y - radius,
        width: radius * 2,
        height: radius * 2,
        borderRadius: "50%",
        border: `1.6px solid ${color.cyan}`,
        background: `rgba(108, 213, 239, ${0.16 * (1 - t)})`,
        opacity: 1 - t,
        boxSizing: "border-box",
      }}
    />
  );
};

/**
 * The application window as the camera sees it: a drawn title bar and frame
 * around the captured pictures. Everything inside the frame is a capture; the
 * film only adds the click rings and the highlight outlines on top.
 */
export const AppWindow: React.FC<{ film: Film; frame: number; camera: Camera }> = ({
  film,
  frame,
  camera,
}) => {
  const { base, previous, overlays } = windowAt(film, frame);
  const wipe = base?.wipe;
  const wiping = wipe && frame - base.at < wipe.frames;
  let clip: string | undefined;
  if (wipe && wiping) {
    const reach = Math.hypot(
      Math.max(wipe.x, width - wipe.x),
      Math.max(wipe.y, height - wipe.y),
    );
    const radius = easeInOut((frame - base.at) / wipe.frames) * reach;
    clip = `circle(${radius}px at ${wipe.x}px ${wipe.y}px)`;
  }
  return (
    <div
      style={{
        position: "absolute",
        left: anchor.x - camera.x * camera.zoom,
        top: anchor.y - camera.y * camera.zoom,
        width,
        height,
        transform: `scale(${camera.zoom})`,
        transformOrigin: "0 0",
      }}
    >
      <div
        style={{
          position: "absolute",
          left: 0,
          top: -titleBar,
          width,
          height: height + titleBar,
          borderRadius: 11,
          overflow: "hidden",
          background: "#1e1e1e",
          boxShadow:
            "0 0 0 1px rgba(255,255,255,0.10), 0 30px 80px rgba(0,0,0,0.55), 0 8px 24px rgba(0,0,0,0.35)",
        }}
      >
        <div
          style={{
            position: "absolute",
            left: 0,
            top: 0,
            width,
            height: titleBar,
            background: color.chrome,
            borderBottom: "1px solid rgba(0,0,0,0.45)",
            boxSizing: "border-box",
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            fontFamily,
            fontSize: 13,
            fontWeight: 500,
            color: color.muted,
            letterSpacing: 0.1,
          }}
        >
          <div style={{ position: "absolute", left: 14, display: "flex", gap: 8 }}>
            {[0, 1, 2].map((dot) => (
              <div
                key={dot}
                style={{ width: 11, height: 11, borderRadius: "50%", background: "#454b57" }}
              />
            ))}
          </div>
          Baza Studio
        </div>
        <div style={{ position: "absolute", left: 0, top: titleBar, width, height }}>
          {wiping && previous ? <Shot name={previous.name} /> : null}
          {base ? <Shot name={base.name} style={clip ? { clipPath: clip } : undefined} /> : null}
          {wiping ? null : overlays.map((overlay) => <Shot key={overlay.name} name={overlay.name} />)}
          {film.highlights.map((highlight) => {
            if (frame < highlight.from || frame > highlight.to) return null;
            const opacity = interpolate(
              frame,
              [highlight.from, highlight.from + 8, highlight.to - 8, highlight.to],
              [0, 1, 1, 0],
              { extrapolateLeft: "clamp", extrapolateRight: "clamp" },
            );
            const grow = 6 * (1 - easeOut(Math.min(1, (frame - highlight.from) / 12)));
            return (
              <div
                key={highlight.from}
                style={{
                  position: "absolute",
                  left: highlight.rect.x - grow,
                  top: highlight.rect.y - grow,
                  width: highlight.rect.w + grow * 2,
                  height: highlight.rect.h + grow * 2,
                  borderRadius: 6,
                  border: `1.5px solid ${color.cyan}`,
                  boxShadow: `0 0 0 4px rgba(108,213,239,0.14), 0 0 22px rgba(108,213,239,0.35)`,
                  boxSizing: "border-box",
                  opacity,
                }}
              />
            );
          })}
          {film.clicks.map((click) => (
            <React.Fragment key={click.at}>
              <Ripple x={click.x} y={click.y} age={frame - click.at} />
              {click.double ? <Ripple x={click.x} y={click.y} age={frame - click.at - 4} /> : null}
            </React.Fragment>
          ))}
        </div>
      </div>
    </div>
  );
};
