// Rasterize an SVG to PNG with resvg.
// Usage: node render-svg.mjs <input.svg> <output.png> [width]
import { readFileSync, writeFileSync } from "node:fs";
import { Resvg } from "@resvg/resvg-js";

const [input, output, widthArg] = process.argv.slice(2);
if (!input || !output) {
  console.error("usage: node render-svg.mjs <input.svg> <output.png> [width]");
  process.exit(1);
}
const width = widthArg ? Number(widthArg) : 1024;
const svg = readFileSync(input, "utf8");
const resvg = new Resvg(svg, {
  fitTo: { mode: "width", value: width },
  background: "rgba(0,0,0,0)",
});
const png = resvg.render().asPng();
writeFileSync(output, png);
console.log(`wrote ${output} (${width}px wide, ${png.length} bytes)`);
