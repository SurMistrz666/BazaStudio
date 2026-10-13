import { continueRender, delayRender, staticFile } from "remotion";

// Inter, the typeface of the application and of the website. The static
// instances are copied from native/assets/fonts by `npm run assets`. The film
// is in English, so the Latin subset of each weight is enough.
const faces: [number, string][] = [
  [400, "Inter-Regular-latin.ttf"],
  [500, "Inter-Medium-latin.ttf"],
  [600, "Inter-SemiBold-latin.ttf"],
];

if (typeof document !== "undefined") {
  const handle = delayRender("Loading Inter");
  Promise.all(
    faces.map(([weight, file]) =>
      new FontFace("Inter", `url(${staticFile(`fonts/${file}`)})`, {
        weight: String(weight),
        style: "normal",
      }).load(),
    ),
  )
    .then((loaded) => {
      for (const face of loaded) document.fonts.add(face);
      continueRender(handle);
    })
    .catch((error) => {
      throw new Error(`Inter could not be loaded: ${error}`);
    });
}
