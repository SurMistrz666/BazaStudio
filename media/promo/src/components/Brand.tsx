import React from "react";
import { Img, staticFile } from "remotion";

/** The application icon, from the same artwork the installers use. */
export const Icon: React.FC<{ size: number; style?: React.CSSProperties }> = ({ size, style }) => (
  <Img src={staticFile("brand/icon.svg")} style={{ width: size, height: size, ...style }} />
);
