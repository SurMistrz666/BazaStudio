import { Easing } from "remotion";
import captures from "../data/captures.json";
import points from "../data/points.json";

/** A point of the application window, in window points (1280 x 720). */
export type Point = readonly [number, number];

/**
 * The camera: the window point that sits at the anchor of the frame, and how
 * many video pixels one window point takes.
 */
export type Camera = { x: number; y: number; zoom: number };

export type Rect = { x: number; y: number; w: number; h: number };

type Segment<T> = { start: number; end: number; to: T; ease: (t: number) => number };

export type Caption = {
  from: number;
  to: number;
  eyebrow: string;
  title: string;
  sub?: string;
  /** Words of `sub` separated by " · " of which one is lit at a time. */
  lit?: { at: number; index: number }[];
};

export type Click = { at: number; x: number; y: number; double: boolean };
export type Highlight = { from: number; to: number; rect: Rect };
export type ShotChange = {
  at: number;
  name: string;
  /** A wipe that opens from this window point instead of a cut. */
  wipe?: { x: number; y: number; frames: number };
};

export type Film = {
  duration: number;
  bases: ShotChange[];
  overlays: ShotChange[];
  camera: { initial: Camera; segments: Segment<Camera>[] };
  pointer: { initial: Point; segments: Segment<Point>[]; hidden: { from: number; to: number }[] };
  clicks: Click[];
  captions: Caption[];
  highlights: Highlight[];
  marks: Record<string, number>;
};

type PointName = Exclude<keyof typeof points, "window">;

const shots = captures.shots as Record<string, Rect>;
const sequences = captures.sequences as Record<string, string[]>;

export const easeInOut = Easing.bezier(0.65, 0, 0.35, 1);
export const easeOut = Easing.bezier(0.16, 1, 0.3, 1);
export const easeSoft = Easing.bezier(0.45, 0, 0.25, 1);

export const shotRect = (name: string): Rect => {
  const rect = shots[name];
  if (!rect) throw new Error(`no capture named ${name}`);
  return rect;
};
export const isFullShot = (name: string) => {
  const rect = shotRect(name);
  return rect.w === captures.window.width && rect.h === captures.window.height;
};

const at = (name: PointName | Point): Point =>
  typeof name === "string" ? (points[name] as unknown as Point) : name;

/**
 * Writes the film as a script: every call happens at the current time, and
 * the calls that take time move the clock forward.
 */
export class FilmScript {
  private t = 0;
  private film: Film;
  private cameraNow: Camera;
  private pointerNow: Point;
  private openCaption: Caption | null = null;

  constructor(camera: Camera, pointer: Point) {
    this.cameraNow = camera;
    this.pointerNow = pointer;
    this.film = {
      duration: 0,
      bases: [],
      overlays: [],
      camera: { initial: camera, segments: [] },
      pointer: { initial: pointer, segments: [], hidden: [] },
      clicks: [],
      captions: [],
      highlights: [],
      marks: {},
    };
  }

  get now() {
    return this.t;
  }

  wait(frames: number) {
    this.t += frames;
    return this;
  }

  mark(name: string) {
    this.film.marks[name] = this.t;
    return this;
  }

  /** Show a capture: a full one replaces the window, a cropped one lies on top. */
  show(name: string, wipe?: { from: PointName | Point; frames: number }) {
    const change: ShotChange = { at: this.t, name };
    if (wipe) {
      const [x, y] = at(wipe.from);
      change.wipe = { x, y, frames: wipe.frames };
    }
    (isFullShot(name) ? this.film.bases : this.film.overlays).push(change);
    return this;
  }

  /** Move the camera while the script goes on. */
  camera(to: Camera, frames: number, ease = easeInOut, delay = 0) {
    const start = this.t + delay;
    this.film.camera.segments.push({ start, end: start + Math.max(frames, 1), to, ease });
    this.cameraNow = to;
    return this;
  }

  /** Keep a held frame alive: creep in by `factor` over `frames`, at an even pace. */
  push(frames: number, factor = 1.025) {
    const to = { ...this.cameraNow, zoom: this.cameraNow.zoom * factor };
    this.film.camera.segments.push({
      start: this.t,
      end: this.t + frames,
      to,
      ease: (t) => t,
    });
    this.cameraNow = to;
    return this;
  }

  /** Move the pointer and wait until it arrives. */
  move(to: PointName | Point, frames: number) {
    const target = at(to);
    this.film.pointer.segments.push({
      start: this.t,
      end: this.t + frames,
      to: target,
      ease: easeSoft,
    });
    this.pointerNow = target;
    this.t += frames;
    return this;
  }

  /** Move the pointer out of the way while the script goes on. */
  drift(to: PointName | Point, frames: number, delay = 0) {
    const target = at(to);
    const start = this.t + delay;
    this.film.pointer.segments.push({ start, end: start + frames, to: target, ease: easeSoft });
    this.pointerNow = target;
    return this;
  }

  click(double = false) {
    const [x, y] = this.pointerNow;
    this.film.clicks.push({ at: this.t, x, y, double });
    // The window answers a couple of frames after the button goes down.
    this.t += double ? 7 : 3;
    return this;
  }

  /** Play a sequence of typed text over `frames`. */
  type(sequence: string, frames: number) {
    const names = sequences[sequence];
    if (!names) throw new Error(`no typed sequence named ${sequence}`);
    // The picture numbered 000 is the empty field; it is shown by the caller.
    const typed = names.filter((name) => !name.endsWith("/000"));
    typed.forEach((name, index) => {
      const offset = Math.round(((index + 1) / typed.length) * frames);
      this.film.overlays.push({ at: this.t + offset, name });
    });
    this.t += frames;
    return this;
  }

  caption(eyebrow: string, title: string, sub?: string) {
    this.endCaption();
    this.openCaption = { from: this.t, to: Number.MAX_SAFE_INTEGER, eyebrow, title, sub };
    this.film.captions.push(this.openCaption);
    return this;
  }

  /** Light one of the " · " separated words of the current caption. */
  light(index: number) {
    if (this.openCaption) (this.openCaption.lit ??= []).push({ at: this.t, index });
    return this;
  }

  endCaption() {
    if (this.openCaption) this.openCaption.to = this.t;
    this.openCaption = null;
    return this;
  }

  highlight(rect: Rect, frames: number) {
    this.film.highlights.push({ from: this.t, to: this.t + frames, rect });
    return this;
  }

  hidePointer(frames: number) {
    this.film.pointer.hidden.push({ from: this.t, to: this.t + frames });
    return this;
  }

  build(): Film {
    this.endCaption();
    this.film.duration = this.t;
    return this.film;
  }
}

function track<T>(
  initial: T,
  segments: Segment<T>[],
  frame: number,
  mix: (a: T, b: T, t: number) => T,
): T {
  let value = initial;
  for (const segment of segments) {
    if (frame <= segment.start) break;
    if (frame >= segment.end) {
      value = segment.to;
      continue;
    }
    const t = segment.ease((frame - segment.start) / (segment.end - segment.start));
    return mix(value, segment.to, t);
  }
  return value;
}

export const cameraAt = (film: Film, frame: number): Camera =>
  track(film.camera.initial, film.camera.segments, frame, (a, b, t) => ({
    x: a.x + (b.x - a.x) * t,
    y: a.y + (b.y - a.y) * t,
    // Zoom moves evenly in ratio, not in difference, so it never rushes.
    zoom: a.zoom * (b.zoom / a.zoom) ** t,
  }));

export const pointerAt = (film: Film, frame: number): Point =>
  track(film.pointer.initial, film.pointer.segments, frame, (a, b, t) => {
    const dx = b[0] - a[0];
    const dy = b[1] - a[1];
    // A hand never moves in a ruler-straight line: bow the path a little.
    const bow = Math.sin(Math.PI * t) * 0.07;
    return [a[0] + dx * t - dy * bow, a[1] + dy * t + dx * bow] as const;
  });

const lastBefore = <T extends { at: number }>(items: T[], frame: number): T | undefined => {
  let found: T | undefined;
  for (const item of items) {
    if (item.at <= frame && (!found || item.at >= found.at)) found = item;
  }
  return found;
};

/**
 * What the window shows on a frame: the last full capture, and on top of it
 * the last cropped capture of every part of the window that changed since.
 */
export function windowAt(film: Film, frame: number) {
  const base = lastBefore(film.bases, frame);
  const previous = base ? lastBefore(film.bases, base.at - 1) : undefined;
  const parts = new Map<string, ShotChange>();
  for (const change of film.overlays) {
    if (change.at > frame || (base && change.at < base.at)) continue;
    const rect = shotRect(change.name);
    const part = `${rect.x},${rect.y},${rect.w},${rect.h}`;
    const known = parts.get(part);
    if (!known || change.at >= known.at) parts.set(part, change);
  }
  const overlays = [...parts.values()].sort((a, b) => a.at - b.at);
  return { base, previous, overlays };
}
