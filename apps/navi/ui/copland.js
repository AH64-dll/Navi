import * as THREE from 'three';

// Shared pieces of the Navi screen: the Copland emblem, soft glow sprites, and the
// menu words that turn on a cylinder. Used by the desktop window (scene.js) and by
// the terminal header frames (artwork/stage.js), so both show the same mark.

export const SERIF = '"Nimbus Roman","Liberation Serif","Times New Roman",serif';
const FLAT = .55; // tubes are flattened in depth, like a raised badge

/** The Copland OS mark: core, dark band, ring opening into the Ω stem, chevrons, three dots. */
export function buildEmblem({gloss = false} = {}) {
  const group = new THREE.Group();
  const glossy = (base, glow, intensity) => gloss
    ? new THREE.MeshPhysicalMaterial({color: base, emissive: glow, emissiveIntensity: intensity, metalness: .1, roughness: .16, clearcoat: 1, clearcoatRoughness: .08})
    : new THREE.MeshStandardMaterial({color: base, emissive: glow, emissiveIntensity: intensity, metalness: .15, roughness: .22});
  const materials = {
    ring: glossy('#3ab4ff', '#139cf5', .7),
    wing: glossy('#2a86ff', '#1066e6', .65),
    core: glossy('#8fe4ff', '#3cbcff', .95),
    dark: glossy('#10309a', '#0c2a9a', .55),
  };
  const cap = (point, radius, material) => {
    const s = new THREE.Mesh(new THREE.SphereGeometry(radius, 24, 16), material);
    s.position.copy(point); s.scale.z = FLAT; group.add(s);
  };
  const pipe = (points, radius, material, segments = 220) => {
    const curve = new THREE.CatmullRomCurve3(points.map(([x, y]) => new THREE.Vector3(x, y, 0)), false, 'centripetal');
    const geometry = new THREE.TubeGeometry(curve, segments, radius, 24, false);
    geometry.scale(1, 1, FLAT);
    group.add(new THREE.Mesh(geometry, material));
    for (const t of [0, 1]) cap(curve.getPoint(t), radius, material);
  };
  // Ring opening into the Ω stem; legs curl outward at the foot.
  const R = .56, ring = [];
  for (let a = -116; a >= -424; a -= 6) ring.push([R * Math.cos(a * Math.PI / 180), R * Math.sin(a * Math.PI / 180)]);
  pipe([[-.47, -1.05], [-.31, -1.01], [-.2, -.93], [-.165, -.81], [-.17, -.67], ...ring,
    [.17, -.67], [.165, -.81], [.2, -.93], [.31, -1.01], [.47, -1.05]], .078, materials.ring, 320);
  for (const side of [-1, 1]) {
    pipe([[.57, .47], [.8, .29], [.99, .04], [.83, -.25], [.59, -.5]].map(([x, y]) => [side * x, y]), .086, materials.wing);
  }
  const band = new THREE.Mesh(new THREE.TorusGeometry(.375, .07, 20, 120), materials.dark);
  band.scale.z = FLAT; group.add(band);
  const core = new THREE.Mesh(new THREE.SphereGeometry(.235, 48, 32), materials.core);
  core.scale.z = .42; group.add(core);
  const dots = [[0, .8, .088], [-1.0, -.58, .08], [1.0, -.58, .08]].map(([x, y, r]) => {
    const dot = new THREE.Mesh(new THREE.SphereGeometry(r, 24, 16), materials.core);
    dot.position.set(x, y, 0); group.add(dot); return dot;
  });
  return {group, materials, core, dots};
}

export function glowTexture(stops = [[0, 'rgba(255,255,255,1)'], [.18, 'rgba(255,255,255,.55)'], [.5, 'rgba(255,255,255,.12)'], [1, 'rgba(255,255,255,0)']]) {
  const c = document.createElement('canvas'); c.width = c.height = 256;
  const g = c.getContext('2d'), gradient = g.createRadialGradient(128, 128, 0, 128, 128, 128);
  for (const [at, value] of stops) gradient.addColorStop(at, value);
  g.fillStyle = gradient; g.fillRect(0, 0, 256, 256);
  const texture = new THREE.CanvasTexture(c); texture.colorSpace = THREE.SRGBColorSpace; return texture;
}

/** A menu word on a segment of a cylinder around the emblem. From behind it reads mirrored, as in the show. */
export function makeWord(text, {radius, y, at, height = .4, vertical = false, tint = '#8dffe0', size = 86, opacity = .95}) {
  const c = document.createElement('canvas'), g = c.getContext('2d');
  g.font = `italic 700 ${size}px ${SERIF}`;
  const w = Math.ceil(g.measureText(text).width) + 40;
  const band = Math.round(size * 1.75);
  c.width = vertical ? band : w; c.height = vertical ? w : band;
  g.font = `italic 700 ${size}px ${SERIF}`;
  if (vertical) { g.translate(c.width / 2, 20); g.rotate(Math.PI / 2); } else g.translate(20, c.height / 2);
  g.textBaseline = 'middle';
  g.shadowColor = tint; g.shadowBlur = 16; g.fillStyle = tint; g.fillText(text, 0, vertical ? 0 : 4);
  g.shadowBlur = 0; g.fillStyle = '#f2fffb'; g.globalAlpha = .8; g.fillText(text, 0, vertical ? 0 : 4);
  const texture = new THREE.CanvasTexture(c); texture.colorSpace = THREE.SRGBColorSpace; texture.anisotropy = 8;
  const worldW = (vertical ? c.height / c.width : c.width / c.height) * height;
  const geometry = vertical
    ? new THREE.CylinderGeometry(radius, radius, worldW, 16, 1, true, at, height / radius)
    : new THREE.CylinderGeometry(radius, radius, height, 64, 1, true, at, worldW / radius);
  const material = new THREE.MeshBasicMaterial({map: texture, transparent: true, side: THREE.DoubleSide, depthWrite: false, blending: THREE.AdditiveBlending, opacity});
  const mesh = new THREE.Mesh(geometry, material); mesh.position.y = y;
  return {mesh, material};
}
