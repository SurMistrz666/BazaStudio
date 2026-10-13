/** Colours of the brand and of the website the film has to sit next to. */
export const color = {
  background: "#14171d",
  backgroundDeep: "#0d0f13",
  cyan: "#6cd5ef",
  cyanSoft: "#a5ecfb",
  blue: "#3b8ddb",
  button: "#1d6ca1",
  text: "#eef1f6",
  muted: "#9aa3b3",
  faint: "#5d6675",
  line: "rgba(255, 255, 255, 0.09)",
  panel: "#1b1f27",
  chrome: "#22262e",
} as const;

export const fontFamily = "Inter, sans-serif";

export const video = { width: 1920, height: 1080, fps: 30 } as const;

/** Where the camera puts the window point it looks at. */
export const anchor = { x: 960, y: 470 } as const;

/** Height of the drawn title bar, in window points. */
export const titleBar = 34;
