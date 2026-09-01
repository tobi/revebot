// Compact port of bloub (https://bloub.vercel.app/, MIT, Jérémy).
// Radial body + two white capsule eyes. Colour/shape from profile.avatar
// (`orange:goutte`) or a hash of the bot id.
// Idle CSS plays blink, wink, gaze, wide-eyes and breathe; `.av.working`
// switches to bloub's thinking dots.
const BLOUB = (() => {
  const N = 64;
  const TAU = Math.PI * 2;
  const COLORS = {
    encre: "#0a0a0c",
    brun: "#8b5e3c",
    rouge: "#e8483f",
    orange: "#f08a24",
    ambre: "#f0b429",
    vert: "#3ecf8e",
    turquoise: "#2fbfa0",
    bleu: "#3b93f0",
    violet: "#8b5cf6",
    rose: "#e152b0",
    gris: "#a3a3a3",
    creme: "#f1efe9",
  };
  const COLOR_IDS = Object.keys(COLORS);
  const SHAPE_IDS = ["cercle", "galet", "squircle", "goutte", "triangle", "hexagone", "nuage", "capsule"];

  function hash(s) {
    let h = 2166136261;
    for (let i = 0; i < s.length; i++) h = Math.imul(h ^ s.charCodeAt(i), 16777619);
    return h >>> 0;
  }

  function parseSpec(spec, seed) {
    const h = hash(seed || "bot");
    let color = COLOR_IDS[h % COLOR_IDS.length];
    let shape = SHAPE_IDS[(h >>> 8) % SHAPE_IDS.length];
    if (spec && spec.includes(":")) {
      const [c, sh] = spec.split(":");
      if (COLORS[c]) color = c;
      if (SHAPE_IDS.includes(sh)) shape = sh;
    } else if (spec && COLORS[spec]) {
      color = spec;
    }
    return { color, shape, hex: COLORS[color] };
  }

  function radiiFor(shape) {
    const out = new Array(N);
    for (let i = 0; i < N; i++) {
      const a = (i / N) * TAU;
      let r = 1;
      if (shape === "galet") r = 1 + 0.075 * Math.cos(2 * a + 0.5) + 0.035 * Math.cos(3 * a + 2.1);
      else if (shape === "squircle") {
        const n = 4.2;
        r = (Math.abs(Math.cos(a)) ** n + Math.abs(Math.sin(a)) ** n) ** (-1 / n);
      } else if (shape === "goutte") {
        // Fat at the bottom (a ≈ π/2), pointed at the top (a ≈ 3π/2). y is down.
        const t = Math.cos(a - Math.PI / 2);
        r = 0.70 + 0.34 * Math.max(0, t) + 0.06 * Math.max(0, -t);
      } else if (shape === "triangle") {
        const sides = 3, rot = -Math.PI / 2, rc = 0.34, rad = 1.12;
        r = roundedPoly(a, sides, rad, rc, rot);
      } else if (shape === "hexagone") {
        r = roundedPoly(a, 6, 1.04, 0.26, 0);
      } else if (shape === "nuage") {
        r = unionCircles(a, [
          [-0.44, 0.2, 0.54], [0.46, 0.2, 0.5], [0.02, 0.3, 0.6],
          [-0.24, -0.3, 0.48], [0.3, -0.24, 0.44],
        ]);
      } else if (shape === "capsule") {
        r = unionCircles(a, [[-0.42, 0, 0.62], [0.42, 0, 0.62]]);
      }
      out[i] = r;
    }
    const peak = Math.max(...out);
    return out.map((v) => v / peak);
  }

  function roundedPoly(a, sides, radius, rc, rot) {
    // Approximate a rounded regular polygon by blending a circle with a poly radius.
    const local = ((a - rot) % TAU + TAU) % TAU;
    const sector = TAU / sides;
    const mid = ((local + sector / 2) % sector) - sector / 2;
    const poly = (radius - rc) / Math.cos(mid) + rc;
    return Math.min(poly, radius * 1.2);
  }

  function unionCircles(a, circles) {
    const dx = Math.cos(a), dy = Math.sin(a);
    let best = 0;
    for (const [cx, cy, cr] of circles) {
      const b = dx * cx + dy * cy;
      const disc = b * b - (cx * cx + cy * cy - cr * cr);
      if (disc < 0) continue;
      const t = b + Math.sqrt(disc);
      if (t > best) best = t;
    }
    return best || 1;
  }

  function bodyPath(radii, scale) {
    const pts = radii.map((r, i) => {
      const a = (i / N) * TAU;
      return { x: r * Math.cos(a) * scale, y: r * Math.sin(a) * scale };
    });
    const n = pts.length;
    const t = 1 / 6;
    let d = `M${pts[0].x.toFixed(2)} ${pts[0].y.toFixed(2)}`;
    for (let i = 0; i < n; i++) {
      const p0 = pts[(i - 1 + n) % n], p1 = pts[i], p2 = pts[(i + 1) % n], p3 = pts[(i + 2) % n];
      d += `C${(p1.x + (p2.x - p0.x) * t).toFixed(2)} ${(p1.y + (p2.y - p0.y) * t).toFixed(2)} ${(p2.x - (p3.x - p1.x) * t).toFixed(2)} ${(p2.y - (p3.y - p1.y) * t).toFixed(2)} ${p2.x.toFixed(2)} ${p2.y.toFixed(2)}`;
    }
    return d + "Z";
  }

  let seq = 0;
  function svg(spec, seed, size) {
    const { hex, shape } = parseSpec(spec, seed);
    const radii = radiiFor(shape);
    const vb = 100;
    const scale = 38;
    const mid = "m" + (++seq);
    const d = bodyPath(radii, scale);
    const h = hash(seed || "bot");
    // Per-bot phase so a row of avatars does not blink, look or wink together.
    const blinkDelay = -((h % 6800) / 1000).toFixed(2);
    const blinkDur = (4.5 + ((h >>> 7) % 34) / 10).toFixed(2);
    const lookDelay = -(((h >>> 3) % 11000) / 1000).toFixed(2);
    const lookDur = (9.5 + ((h >>> 11) % 40) / 10).toFixed(2);
    const breatheDur = (3.6 + ((h >>> 5) % 18) / 10).toFixed(2);
    const breatheDelay = -(((h >>> 9) % 3600) / 1000).toFixed(2);
    const bobDur = (3.2 + ((h >>> 13) % 16) / 10).toFixed(2);
    // Eyes lean \\ about 26° (bloub measurement), as mask holes on the body.
    // `.think` is bloub's thinking state: the body becomes three pulsing dots.
    return `<svg class="bloub" width="${size}" height="${size}" viewBox="${-vb/2} ${-vb/2} ${vb} ${vb}" aria-hidden="true" style="--blink-dur:${blinkDur}s;--blink-delay:${blinkDelay}s;--look-dur:${lookDur}s;--look-delay:${lookDelay}s;--breathe-dur:${breatheDur}s;--breathe-delay:${breatheDelay}s;--bob-dur:${bobDur}s">
      <defs>
        <mask id="${mid}">
          <path d="${d}" fill="#fff"/>
          <g fill="#000" transform="rotate(26)">
            <g class="bloub-gaze">
              <rect class="eye eye-l" x="-18" y="-14" width="8" height="18" rx="4"/>
              <rect class="eye eye-r" x="6" y="-14" width="8" height="18" rx="4"/>
            </g>
          </g>
        </mask>
      </defs>
      <g class="bloub-idle">
        <path class="bloub-body" d="${d}" fill="${hex}" mask="url(#${mid})"/>
      </g>
      <g class="think" fill="${hex}">
        <circle class="think-dot" cx="-24" cy="0" r="9"/>
        <circle class="think-dot" cx="0" cy="0" r="9"/>
        <circle class="think-dot" cx="24" cy="0" r="9"/>
      </g>
    </svg>`;
  }

  return { svg, parseSpec, COLORS, SHAPE_IDS };
})();

export { BLOUB };
