// Copies the typeface and the icon of the application into `public/`, so the
// film uses the same files as the product instead of keeping a second copy.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const assets = path.resolve(root, "../../native/assets");
const copies = [
  ["fonts/Inter-Regular-latin.ttf", "fonts/Inter-Regular-latin.ttf"],
  ["fonts/Inter-Medium-latin.ttf", "fonts/Inter-Medium-latin.ttf"],
  ["fonts/Inter-SemiBold-latin.ttf", "fonts/Inter-SemiBold-latin.ttf"],
  ["brand/baza-studio-macos.svg", "brand/icon.svg"],
];
for (const [from, to] of copies) {
  const target = path.join(root, "public", to);
  fs.mkdirSync(path.dirname(target), { recursive: true });
  fs.copyFileSync(path.join(assets, from), target);
}
console.log(`copied ${copies.length} files from native/assets`);
