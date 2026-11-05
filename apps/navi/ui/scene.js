import * as THREE from 'three';
import {EffectComposer} from 'three/addons/postprocessing/EffectComposer.js';
import {RenderPass} from 'three/addons/postprocessing/RenderPass.js';
import {UnrealBloomPass} from 'three/addons/postprocessing/UnrealBloomPass.js';
import {ShaderPass} from 'three/addons/postprocessing/ShaderPass.js';
import {OutputPass} from 'three/addons/postprocessing/OutputPass.js';
import {buildEmblem, glowTexture, makeWord} from './copland.js';

// The Navi screen from Serial Experiments Lain, rebuilt as geometry: the Copland
// emblem (ring and Ω stem, chevrons, three dots), mirrored menu words turning on a
// cylinder, the dark elliptical floor, sweeping arcs, and a broadcast-video finish.
// Everything is procedural; no anime frames are bundled.
//
// window.naviScene — the layout owns the composition, the scene only renders it:
//   setFrame({x,y,w,h}, dim)  anchor the artwork inside the `#stage-region` rect.
//                             `dim` (0..1) fades the art while panels are open.
//                             Calling it without a rect keeps the last cell.
//   setLayout(p, dim)         legacy horizontal anchor; vertical stays centred.
//   setPhase(idle|working|error)  the single energy/glitch driver of a turn.
//   setEnergy(level)          legacy level-only setter; it never glitches.
//   setMotion(bool)           live motion authority (also follows the OS query).
//   glitch()                  one tearing pulse; ignored while still.
//   boot(done, instant)       entrance. `instant` is for tests only.
//   degrade(reason)           drop to the CSS backdrop; safe to call twice.
//   bounds()                  the art's screen rect: {x,y,w,h,scale,settled}.
//                             The backdrop is excluded on purpose: it stays
//                             full-bleed behind the whole window, so it has no
//                             cell of its own to fit into.

const canvas = document.getElementById('field');
const FALLBACK_BACKGROUND = 'radial-gradient(ellipse at 22% 48%,#1638ff,#030b6a 45%,#01052e 80%)';
const NOOP = {
  setMotion() {}, setEnergy() {}, setPhase() {}, setLayout() {}, setFrame() {},
  boot(done) { done?.(); }, glitch() {}, degrade() {}, bounds() { return null; },
};

let renderer;
try { renderer = new THREE.WebGLRenderer({canvas, antialias: true, powerPreference: 'default'}); }
catch { renderer = undefined; }

if (!renderer) {
  canvas.style.background = FALLBACK_BACKGROUND;
  window.naviScene = {...NOOP};
  canvas.dataset.renderer = 'fallback';
  // Let station.js attach its listener before reporting (BUG-096).
  setTimeout(() => window.dispatchEvent(
    new CustomEvent('navi:degraded', {detail: {reason: 'webgl-unavailable'}})
  ), 0);
} else {
  const color = hex => new THREE.Color(hex);
  renderer.setPixelRatio(Math.min(devicePixelRatio || 1, 2));
  renderer.outputColorSpace = THREE.SRGBColorSpace;
  renderer.toneMapping = THREE.NeutralToneMapping;
  renderer.toneMappingExposure = 1.0;

  const scene = new THREE.Scene();
  const camera = new THREE.PerspectiveCamera(36, innerWidth / innerHeight, .1, 80);

  // ── Backdrop: electric blue with slow concentric interference rings ──────────
  const backdrop = new THREE.Mesh(new THREE.PlaneGeometry(60, 40), new THREE.ShaderMaterial({
    depthWrite: false,
    uniforms: {
      uTime: {value: 0}, uBoot: {value: 0}, uEnergy: {value: 0},
      uDeep: {value: color('#010428')}, uBlue: {value: color('#0a26e0')}, uCyan: {value: color('#2f7bff')},
      uViolet: {value: color('#6a3bf0')}, uNavy: {value: color('#26306a')},
    },
    vertexShader: 'varying vec3 vPos; void main(){ vPos = position; gl_Position = projectionMatrix * modelViewMatrix * vec4(position,1.0); }',
    fragmentShader: `
      varying vec3 vPos;
      uniform float uTime, uBoot, uEnergy;
      uniform vec3 uDeep, uBlue, uCyan, uViolet, uNavy;
      void main(){
        vec2 p = vPos.xy - vec2(0.0, 0.25);
        float r = length(p * vec2(0.92, 1.1));
        vec3 col = mix(uBlue, uDeep, smoothstep(0.5, 10.5, r));
        float speed = 0.75 + uEnergy * 1.6;
        float a = smoothstep(0.35, 1.0, sin(r * 4.2 - uTime * speed));
        float b = smoothstep(0.62, 1.0, sin(r * 1.7 + uTime * 0.31 + 1.7));
        vec3 tint = mix(uCyan, uViolet, 0.5 + 0.5 * sin(r * 0.9 - uTime * 0.13));
        col += (a * 0.2 + b * 0.13) * tint * (1.0 - smoothstep(1.2, 10.0, r));
        col *= 0.92 + 0.08 * sin(vPos.y * 1.3 + uTime * 0.4);
        gl_FragColor = vec4(mix(uNavy, col, uBoot), 1.0);
      }`,
  }));
  backdrop.position.set(0, 0, -7);
  backdrop.renderOrder = -1; // behind every additive element of the composition
  scene.add(backdrop);

  // ── Lights: a cool key from upper left gives the pipes their gloss ───────────
  scene.add(new THREE.HemisphereLight(0x9fd8ff, 0x0a0a40, 1.2));
  const key = new THREE.DirectionalLight(0xe8f6ff, 2.6); key.position.set(-3, 4, 6); scene.add(key);
  const rim = new THREE.PointLight(0x6d4dff, 30, 12, 1.6); rim.position.set(2.5, -1.5, -1.5); scene.add(rim);

  // ── The Copland emblem ───────────────────────────────────────────────────────
  const stage = new THREE.Group(); scene.add(stage);
  const {group: emblem, materials: {ring: ringMat, core: coreMat}, core, dots} = buildEmblem();
  stage.add(emblem);

  const softGlow = glowTexture();
  const additive = (map, tint, opacity) => new THREE.SpriteMaterial({map, color: tint, opacity, transparent: true, depthWrite: false, blending: THREE.AdditiveBlending});
  // The halo is sized for a framed cell, not a full window, and is drawn after the
  // floor so the floor's alpha can never cut a terminator through it (BUG-023).
  const halo = new THREE.Sprite(additive(softGlow, 0x1d5cff, .6));
  halo.scale.set(3.2, 3.2, 1); halo.position.z = -.6; halo.renderOrder = 3; stage.add(halo);

  // ── Floor: the dark ellipse the emblem hovers over, lit along its rim ────────
  // Tilted toward the camera: seen at the lens' ~15° elevation a flat plane is
  // edge-on and invisible, which is why the glow never read (BUG-022). The inner
  // lift is a bright cyan so it clears the bloom threshold instead of sinking into
  // the blue channel, and a .75 alpha keeps it a glow rather than a slab.
  const floor = new THREE.Mesh(new THREE.CircleGeometry(1.75, 160), new THREE.ShaderMaterial({
    transparent: true, depthWrite: false,
    uniforms: {uTime: {value: 0}, uBoot: {value: 0}, uDark: {value: color('#000524')}, uRim: {value: color('#2d5bff')}, uLift: {value: color('#9fd8ff')}},
    vertexShader: 'varying vec2 vUv; void main(){ vUv = uv; gl_Position = projectionMatrix * modelViewMatrix * vec4(position,1.0); }',
    fragmentShader: `
      varying vec2 vUv; uniform float uTime, uBoot; uniform vec3 uDark, uRim, uLift;
      void main(){
        vec2 p = vUv * 2.0 - 1.0; float r = length(p);
        vec3 col = mix(uDark, uRim, smoothstep(0.72, 0.99, r));
        col += uLift * 0.85 * pow(max(0.0, 1.0 - r * 1.6), 2.0) * (0.85 + 0.15 * sin(uTime * 1.3));
        col += uRim * 0.6 * smoothstep(0.35, 1.0, -p.y) * smoothstep(0.86, 1.0, r);
        float alpha = (0.75 - 0.15 * smoothstep(0.97, 1.0, r)) * uBoot;
        gl_FragColor = vec4(col, alpha);
      }`,
  }));
  floor.rotation.x = -Math.PI / 2 + .28; floor.position.y = -1.5; floor.renderOrder = 2; stage.add(floor);

  // ── Menu words on a turning cylinder; mirrored as they pass behind ───────────
  const orbit = new THREE.Group(); stage.add(orbit);
  const fadeables = [];
  function word(text, options) {
    const {mesh, material} = makeWord(text, options);
    orbit.add(mesh); fadeables.push([material, material.opacity]);
    return mesh;
  }
  word('Serch Mail', {radius: 1.62, y: .12, at: .2});
  word('Network Connect', {radius: 1.9, y: -.35, at: 2.4, vertical: true, height: .26});
  word('Mail Menu', {radius: 1.75, y: .55, at: 4.1, vertical: true, height: .22, tint: '#9fd4ff'});
  // Hairlines of light standing in the same turning space.
  const lineMat = new THREE.MeshBasicMaterial({color: 0x7fd8ff, transparent: true, opacity: .55, depthWrite: false, blending: THREE.AdditiveBlending});
  fadeables.push([lineMat, .55]);
  for (const [angle, radius, height, y] of [[.7, 1.15, 3.1, -.1], [2.0, 1.55, 3.6, .1], [3.6, 1.25, 2.6, .35], [4.8, 1.65, 3.2, -.2], [5.7, 1.1, 2.2, .5]]) {
    const hair = new THREE.Mesh(new THREE.PlaneGeometry(.009, height), lineMat);
    hair.position.set(Math.sin(angle) * radius, y, Math.cos(angle) * radius); orbit.add(hair);
  }

  // ── Sweeping arcs ────────────────────────────────────────────────────────────
  // `turns` collects every part that spins about its own origin, with the radius
  // of the disc it sweeps: the fit is measured from that envelope, so a turning
  // ring can never make the art breathe (see "the art's envelope", below).
  // The radii sit just outside the halo: the art is framed by its own cell, and a
  // ring sweeping three times the mark's width is what used to squeeze the emblem
  // down to a dot in a docked cell (BUG-023).
  const turns = [];
  const arcs = [];
  const arcMat = new THREE.MeshBasicMaterial({color: 0x86ecff, transparent: true, opacity: .8, depthWrite: false, blending: THREE.AdditiveBlending});
  fadeables.push([arcMat, .8]);
  for (const [radius, arc, rx, rz, speed] of [[1.72, 1.35, 1.36, .2, .09], [2.12, 1.05, 1.22, -.22, -.06], [1.42, .9, 1.48, .05, .14]]) {
    const holder = new THREE.Group(); holder.rotation.set(rx, 0, rz); stage.add(holder);
    const mesh = new THREE.Mesh(new THREE.TorusGeometry(radius, .0075, 6, 360, Math.PI * arc), arcMat);
    holder.add(mesh); arcs.push([mesh, speed]);
    turns.push([holder, radius + .0075]);
  }

  // ── Wandering lights that circle in front of and behind the emblem ───────────
  const wisps = [];
  for (const [radius, tilt, speed, phase, size] of [[1.32, .5, .42, 0, .075], [1.6, -.35, -.3, 2.2, .06]]) {
    const plane = new THREE.Group(); plane.rotation.set(1.25, 0, tilt); stage.add(plane);
    const body = new THREE.Mesh(new THREE.SphereGeometry(size, 20, 14), new THREE.MeshBasicMaterial({color: 0xdff8ff}));
    const glow = new THREE.Sprite(additive(softGlow, 0x62c8ff, .9)); glow.scale.set(size * 9, size * 9, 1);
    body.add(glow); plane.add(body); wisps.push({body, radius, speed, phase});
    turns.push([plane, radius + size * 4.5]);
  }

  // ── The art's envelope: the shape the composition can sweep ──────────────────
  // The art is framed by its own cell, and it must be framed once: the rings spin
  // in their planes and the menu words yaw on their turntable, so an axis-aligned
  // box over the group grows and shrinks every frame — fitting to that made the
  // art breathe, and the shrinking box then chased itself down to the floor.
  // Every part is bounded by what it can *reach* instead: a turning part by the
  // disc it sweeps, the turntable by the cylinder it turns inside, a still part by
  // its exact box. It is stage-local and measured once, here, before the first
  // render, so the fit never depends on the clock.
  const artEnvelope = new THREE.Box3();
  {
    const box = new THREE.Box3(), point = new THREE.Vector3();
    const grow = (x, y, z) => artEnvelope.expandByPoint(point.set(x, y, z));
    const still = object => { box.setFromObject(object); artEnvelope.union(box); };
    // A disc of `radius` in the plane spanned by the object's local x and y axes
    // reaches radius·√(u²+v²) along each stage axis.
    const disc = (object, radius) => {
      object.updateWorldMatrix(true, false);
      const e = object.matrixWorld.elements, centre = [e[12], e[13], e[14]];
      const half = [0, 1, 2].map(axis => radius * Math.hypot(e[axis], e[4 + axis]));
      grow(centre[0] - half[0], centre[1] - half[1], centre[2] - half[2]);
      grow(centre[0] + half[0], centre[1] + half[1], centre[2] + half[2]);
    };
    still(halo); still(floor); still(emblem);
    for (const [object, radius] of turns) disc(object, radius);
    // The turntable only yaws: its radius turns, its height does not.
    box.setFromObject(orbit);
    const reach = Math.max(-box.min.x, box.max.x, -box.min.z, box.max.z);
    grow(-reach, box.min.y, -reach); grow(reach, box.max.y, reach);
  }

  // ── Post: bloom, then a late-90s broadcast finish ────────────────────────────
  const composer = new EffectComposer(renderer);
  composer.addPass(new RenderPass(scene, camera));
  const bloom = new UnrealBloomPass(new THREE.Vector2(innerWidth / 2, innerHeight / 2), .6, .45, .42);
  composer.addPass(bloom);
  composer.addPass(new OutputPass());
  const video = new ShaderPass({
    uniforms: {tDiffuse: {value: null}, uRes: {value: new THREE.Vector2(1, 1)}, uTime: {value: 0}, uGlitch: {value: 0}, uSeed: {value: 0}},
    vertexShader: 'varying vec2 vUv; void main(){ vUv = uv; gl_Position = projectionMatrix * modelViewMatrix * vec4(position,1.0); }',
    fragmentShader: `
      uniform sampler2D tDiffuse; uniform vec2 uRes; uniform float uTime, uGlitch, uSeed; varying vec2 vUv;
      float rand(vec2 c){ return fract(sin(dot(c, vec2(12.9898, 78.233))) * 43758.5453); }
      void main(){
        vec2 uv = vUv;
        float row = floor(uv.y * 38.0);
        float tear = uGlitch * step(0.82, rand(vec2(row, uSeed)));
        uv.x += tear * (rand(vec2(uSeed, row)) - 0.5) * 0.06;
        float px = 1.0 / uRes.x, ca = 1.2 * px + tear * 6.0 * px;
        vec3 c = vec3(texture2D(tDiffuse, uv + vec2(ca, 0.0)).r, texture2D(tDiffuse, uv).g, texture2D(tDiffuse, uv - vec2(ca, 0.0)).b);
        vec3 smear = (texture2D(tDiffuse, uv + vec2(2.5 * px, 0.0)).rgb + texture2D(tDiffuse, uv - vec2(2.5 * px, 0.0)).rgb) * 0.5;
        c = mix(c, smear, 0.18);
        c *= mod(gl_FragCoord.y, 3.0) < 1.0 ? 0.8 : 1.0;
        c += (rand(uv * uRes + fract(uTime)) - 0.5) * 0.045;
        vec2 v = uv - 0.5; c *= 1.0 - 0.42 * pow(length(v * vec2(1.0, 1.15)) * 1.35, 2.4);
        gl_FragColor = vec4(c, 1.0);
      }`,
  });
  composer.addPass(video);

  // ── Framing: the lens follows the stage cell, never the viewport centre ──────
  // The artwork is placed at (p,q) of the window; the virtual lens grows to
  //   2·W·max(p,1−p) × 2·H·max(q,1−q)
  // so moving the cell off-centre costs crop, never an object pushed off-screen.
  // p,q ∈ [.08,.92] (the cell can sit near an edge; the art is re-scaled, below).
  const LENS_Z = 7.6;          // camera distance used to derive the art scale
  const CAM_Y = .55, LOOK_Y = -.05;  // the resting lens: level, aimed below the mark
  const ART_MIN = .15, ART_MAX = 1.4;  // sanity bounds on the fitted scale
  const DEAD_ZONE = .06;       // the middle of the cell does not tilt the camera
  const FIT_MARGIN = 16;       // px of clearance the art keeps inside its own cell
  const PARALLAX_PX = 7;       // pointer parallax travel, in pixels
  const BOB_PX = 3;            // floor bob amplitude, in pixels
  const state = {
    motion: true, visible: true, elapsed: 0, energy: 0, energyTarget: 0,
    stageX: .5, stageY: .5, stageXTarget: .5, stageYTarget: .5,
    dim: 0, dimTarget: 0, boot: 0, glitch: 0, phase: 'idle',
    frame: null, painted: -1,
  };
  const pointer = {x: 0, y: 0}, smooth = {x: 0, y: 0};

  function lens() {
    const W = innerWidth, H = innerHeight;
    const p = Math.min(.92, Math.max(.08, state.stageX));
    const q = Math.min(.92, Math.max(.08, state.stageY));
    const fullW = 2 * W * Math.max(p, 1 - p);
    const fullH = 2 * H * Math.max(q, 1 - q);
    return {W, H, p, q, fullW, fullH, offX: fullW / 2 - p * W, offY: fullH / 2 - q * H};
  }
  function frame() {
    const {W, H, fullW, fullH, offX, offY} = lens();
    camera.aspect = fullW / fullH;
    camera.setViewOffset(fullW, fullH, offX, offY, W, H);
    camera.updateProjectionMatrix();
  }
  // Apparent size tracks the cell, not the window: the lens factor cancels exactly,
  // so opening a panel shifts the art instead of stretching it across the panels.

  // ── The art's screen rect: the measurement the fit and the tests share ───────
  // A world box is not a screen rect — perspective turns it into a trapezoid — so
  // the eight corners go through the camera, whose projection matrix already
  // carries the lens' view offset, and the extreme pixels are the bounds. The
  // backdrop is deliberately absent: it stays full-bleed behind the whole window,
  // so it has no cell to fit into. The stage transform is a uniform scale and a
  // lift, applied here by hand so nothing has to be walked again.
  const corner = new THREE.Vector3();
  const round1 = value => Math.round(value * 10) / 10;
  function projectEnvelope(scale, lift) {
    camera.updateMatrixWorld();
    const {min, max} = artEnvelope;
    let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
    for (let i = 0; i < 8; i++) {
      corner.set((i & 1 ? max.x : min.x) * scale, (i & 2 ? max.y : min.y) * scale + lift, (i & 4 ? max.z : min.z) * scale)
        .project(camera);
      const x = (corner.x * .5 + .5) * innerWidth, y = (.5 - corner.y * .5) * innerHeight;
      if (x < x0) x0 = x;
      if (x > x1) x1 = x;
      if (y < y0) y0 = y;
      if (y > y1) y1 = y;
    }
    return {x: x0, y: y0, w: x1 - x0, h: y1 - y0};
  }

  // ── Fit: the largest scale whose whole envelope lands inside its own cell ────
  // A height-only scale is what pushed the halo and the rings of a narrow cell
  // (dock + graph open) under the terminal, where an opaque panel cut them with a
  // straight vertical edge (BUG-020/023). The fit maximises instead: both axes
  // constrain it, so a narrow cell shrinks the art and a wide one grows it until
  // the binding axis is full. It is cached until the cell or the lens changes: the
  // envelope already covers the art's turn and bob, so animation never fits.
  let fit = {key: '', scale: ART_MIN, px: 1};
  function fitArt() {
    const {W, H, p, q} = lens();
    // Before the station reports a cell, the window is the cell; the real one
    // arrives on the first setFrame and changes the key.
    const cell = state.frame || {x: 0, y: 0, w: innerWidth, h: innerHeight};
    const key = [cell.x, cell.y, cell.w, cell.h].map(value => value.toFixed(2)).join()
      + `|${W}x${H}|${p.toFixed(4)},${q.toFixed(4)}`;
    if (key === fit.key) return fit;
    // Measure from the resting lens: hovering and the entrance fly must never
    // change how big the art is.
    camera.position.set(0, CAM_Y, LENS_Z);
    camera.lookAt(0, LOOK_Y, 0);
    frame();
    const left = cell.x + FIT_MARGIN, right = cell.x + cell.w - FIT_MARGIN;
    const top = cell.y + FIT_MARGIN, bottom = cell.y + cell.h - FIT_MARGIN;
    const inside = candidate => {
      const screen = projectEnvelope(candidate, 0);
      return screen.x >= left && screen.x + screen.w <= right
        && screen.y >= top && screen.y + screen.h <= bottom;
    };
    // Halving, not a multiplicative step: perspective makes the projected size
    // fall faster than the scale, so `scale *= ratio` overshoots and leaves the
    // art smaller than its cell.
    let scale = ART_MAX;
    if (!inside(scale)) {
      let loose = scale, tight = ART_MIN;
      for (let pass = 0; pass < 12; pass++) {
        const middle = (tight + loose) / 2;
        if (inside(middle)) tight = middle; else loose = middle;
      }
      scale = tight;
    }
    const measured = projectEnvelope(scale, 0);
    const width = Math.max(1e-3, (artEnvelope.max.x - artEnvelope.min.x) * scale);
    fit = {key, scale, px: measured.w / width};
    return fit;
  }

  // ── bounds(): the art's envelope on screen, and whether it has stopped moving ─
  // Eight projections and no traversal, so an inspector can poll it every frame.
  function settled() {
    const cell = state.frame;
    const tx = cell ? (cell.x + cell.w / 2) / innerWidth : .5;
    const ty = cell ? (cell.y + cell.h / 2) / innerHeight : .5;
    return Math.abs(state.stageX - tx) < .002 && Math.abs(state.stageY - ty) < .002
      && Math.abs(state.dimTarget - state.dim) < .005 && state.boot === 1;
  }
  function bounds() {
    const box = projectEnvelope(stage.scale.x, stage.position.y);
    const cell = state.frame;
    return {
      x: round1(box.x), y: round1(box.y), w: round1(box.w), h: round1(box.h),
      scale: Math.round(stage.scale.x * 1000) / 1000, settled: settled(),
      /* The cell the art was framed for, so a probe can check the art against the
       * cell rather than guessing which rect the scene was given. */
      cell: cell ? {x: round1(cell.x), y: round1(cell.y), w: round1(cell.w), h: round1(cell.h)} : null,
    };
  }

  // ── Stage cell: the station measures the rect of #stage-region, less any drawer
  // over it, and reports it with setFrame; the scene only reads the region itself
  // before the first report, so the art has a cell even if that never arrives.
  const region = document.getElementById('stage-region');
  function readCell() {
    if (!region) return null;
    const box = region.getBoundingClientRect();
    return box.width > 1 && box.height > 1 ? {x: box.x, y: box.y, w: box.width, h: box.height} : null;
  }
  function applyFrame(box, dim) {
    if (dim !== undefined) state.dimTarget = Math.min(1, Math.max(0, dim));
    if (box && box.w > 1 && box.h > 1) {
      state.frame = {x: box.x, y: box.y, w: box.w, h: box.h};
      state.stageXTarget = (state.frame.x + state.frame.w / 2) / innerWidth;
      state.stageYTarget = (state.frame.y + state.frame.h / 2) / innerHeight;
    }
  }

  // ── Pointer parallax: only over the stage, never while typing ───────────────
  const typing = () => {
    const active = document.activeElement;
    return !!active && typeof active.closest === 'function' && !!active.closest('#chat-form, .terminal-head');
  };
  const easeOut = value => {
    const magnitude = Math.abs(value);
    if (magnitude <= DEAD_ZONE) return 0;
    return Math.sign(value) * (magnitude - DEAD_ZONE) / (.5 - DEAD_ZONE);
  };
  addEventListener('pointermove', event => {
    const box = state.motion ? (state.frame || readCell()) : null;
    if (!box || typing()) { pointer.x = 0; pointer.y = 0; return; }
    const nx = (event.clientX - box.x) / box.w - .5, ny = (event.clientY - box.y) / box.h - .5;
    const inside = Math.abs(nx) <= .5 && Math.abs(ny) <= .5;
    pointer.x = inside ? easeOut(nx) : 0;
    pointer.y = inside ? easeOut(ny) : 0;
  }, {passive: true});

  // ── Motion authority: setMotion wins, otherwise the OS preference is live ────
  const reduced = matchMedia('(prefers-reduced-motion: reduce)');
  let forcedMotion = null;
  state.motion = !reduced.matches;
  const wantsMotion = () => forcedMotion === null ? !reduced.matches : forcedMotion;
  function applyMotion() {
    state.motion = wantsMotion();
    if (!state.motion) { pointer.x = pointer.y = 0; smooth.x = smooth.y = 0; state.glitch = 0; settle(); draw(); }
    wake();
  }
  reduced.addEventListener('change', applyMotion);

  function resize() {
    const ratio = Math.min(window.devicePixelRatio || 1, window.naviDprCap || 2);
    if (renderer.getPixelRatio() !== ratio) renderer.setPixelRatio(ratio);
    renderer.setSize(innerWidth, innerHeight, false);
    composer.setPixelRatio(renderer.getPixelRatio());
    composer.setSize(innerWidth, innerHeight);
    bloom.resolution.set(innerWidth / 2, innerHeight / 2);
    video.uniforms.uRes.value.set(innerWidth * renderer.getPixelRatio(), innerHeight * renderer.getPixelRatio());
    const box = state.frame || readCell();
    if (box) applyFrame(box);
    stage.scale.setScalar(fitArt().scale);
    frame(); draw();
  }

  function paintDim() {
    // Written only when the rounded value changes: #field has no CSS transition,
    // so a write per frame is pure layout churn (BUG-020).
    const value = Math.round((1 - state.dim * .35) * 100) / 100;
    if (value === state.painted) return;
    state.painted = value;
    canvas.style.opacity = String(value);
  }

  function render() {
    const t = state.elapsed, e = state.energy, b = state.boot;
    const introEase = b * b * (3 - 2 * b);
    const {scale, px} = fitArt();
    backdrop.material.uniforms.uTime.value = t;
    backdrop.material.uniforms.uBoot.value = introEase;
    backdrop.material.uniforms.uEnergy.value = e;
    floor.material.uniforms.uTime.value = t;
    floor.material.uniforms.uBoot.value = introEase;
    const fade = introEase * (1 - state.dim * .55);
    for (const [material, opacity] of fadeables) material.opacity = opacity * fade;
    halo.material.opacity = (.3 + .2 * introEase + .12 * Math.sin(t * 1.7) * e) * (1 - state.dim * .55);

    // The pan is measured in pixels, not world units: the fit gives the art the
    // whole cell, so a world-unit pan would swing it back under a panel (BUG-032)
    // — and the parallax budget is half of what the margin leaves, so hovering can
    // reach the edge of the cell but never a straight cut (BUG-020/023).
    camera.position.set(
      smooth.x * PARALLAX_PX / px,
      CAM_Y + introEase * .1 - smooth.y * (PARALLAX_PX * .5) / px,
      LENS_Z - (1 - introEase) * .8);
    camera.lookAt(0, LOOK_Y - (1 - introEase) * .15, 0);
    frame();

    stage.scale.setScalar(scale);
    stage.position.y = (1 - introEase) * .35 + Math.sin(t * .6) * BOB_PX / px;
    emblem.rotation.set(Math.sin(t * .27) * .07 * introEase, Math.sin(t * .33) * .26 * introEase, 0);
    const pulse = 1 + .06 * Math.sin(t * (1.6 + e * 3)) * (.3 + e);
    core.scale.set(pulse, pulse, .42 * pulse);
    coreMat.emissiveIntensity = .9 + e * .5 + .12 * Math.sin(t * 2.2);
    ringMat.emissiveIntensity = .7 + e * .3;
    for (let i = 0; i < dots.length; i++) dots[i].position.z = Math.sin(t * .8 + i * 2.1) * .12;

    orbit.rotation.y = t * (.16 + e * .22);
    for (const [mesh, speed] of arcs) mesh.rotation.z = t * speed * (1 + e);
    for (const wisp of wisps) {
      const a = wisp.phase + t * wisp.speed * (1 + e * 1.2);
      wisp.body.position.set(Math.cos(a) * wisp.radius, Math.sin(a) * wisp.radius, 0);
      wisp.body.visible = b > .6;
    }
    bloom.strength = .55 + e * .2 + (1 - introEase) * .25;
    video.uniforms.uTime.value = t;
    video.uniforms.uGlitch.value = state.glitch;
    paintDim();
    composer.render();
  }

  // ── Degrade: a broken frame must not take the UI down with it (BUG-096) ──────
  let degraded = false, failures = 0, contextLost = false;
  function degrade(reason) {
    if (degraded) return;
    degraded = true;
    cancelAnimationFrame(raf); raf = undefined;
    canvas.removeAttribute('style');
    canvas.style.background = FALLBACK_BACKGROUND;
    canvas.dataset.renderer = 'fallback';
    canvas.dataset.degraded = String(reason || 'draw');
    window.naviScene = {...NOOP, renderer, scene};
    window.dispatchEvent(
      new CustomEvent('navi:degraded', {detail: {reason: String(reason || 'draw')}})
    );
  }

  function draw() {
    if (degraded || contextLost) return;
    try { render(); failures = 0; }
    catch (error) {
      if (++failures >= 3) degrade(error && error.message ? error.message : 'draw');
    }
  }

  function settle() {
    state.energy = state.energyTarget;
    state.stageX = state.stageXTarget;
    state.stageY = state.stageYTarget;
    state.dim = state.dimTarget;
    state.glitch = 0;
    smooth.x = 0; smooth.y = 0;
  }

  let raf, last = 0, carry = 0;
  const STEP = 1 / 30; // the anime's television cadence; also spares the GPU
  function loop(now) {
    raf = requestAnimationFrame(loop);
    const dt = last ? Math.min(.1, (now - last) / 1000) : 0; last = now;
    carry += dt; if (carry < STEP) return;
    const step = carry; carry = 0;
    if (state.motion) state.elapsed += step;
    if (!state.motion) settle();
    const k = 1 - Math.pow(.02, step);
    state.energy += (state.energyTarget - state.energy) * k * .8;
    state.stageX += (state.stageXTarget - state.stageX) * k;
    state.stageY += (state.stageYTarget - state.stageY) * k;
    state.dim += (state.dimTarget - state.dim) * k;
    state.glitch = Math.max(0, state.glitch - step * 8);
    smooth.x += (pointer.x - smooth.x) * k * .12; smooth.y += (pointer.y - smooth.y) * k * .12;
    draw();
    const settled = !state.motion && Math.abs(state.stageXTarget - state.stageX) < .001
      && Math.abs(state.stageYTarget - state.stageY) < .001
      && Math.abs(state.dimTarget - state.dim) < .002 && !bootTween;
    if (settled) { cancelAnimationFrame(raf); raf = undefined; }
  }
  const wake = () => { if (!degraded && !raf && state.visible) { last = 0; raf = requestAnimationFrame(loop); } };

  // ── One glitch per turn: the phase edge is the only implicit trigger ─────────
  const ENERGY = {idle: .1, working: .85, error: .35};
  function pulse() {
    state.glitch = 1;
    video.uniforms.uSeed.value = Math.random() * 100;
    wake();
  }
  function setLevel(level, tear) {
    const value = Number.isFinite(level) ? Math.min(1, Math.max(0, level)) : ENERGY.idle;
    state.energyTarget = value;
    if (tear && state.motion) pulse();
    if (!state.motion) { settle(); draw(); return; }
    wake();
  }

  let bootTween;
  function boot(done, instant) {
    // Production keeps the title for 900 ms while the scene holds, flies in for
    // 1.5 s, then reports ready so the host fades the overlay (BUG-029). `instant`
    // stays for the tests, which must not wait out the entrance. With motion off the
    // flight is skipped but the hold is kept: static, not invisible (BUG-030).
    if (instant) { state.boot = 1; draw(); done?.(); return; }
    const animate = state.motion;
    const start = performance.now(), hold = 900, fly = animate ? 1500 : 0;
    state.boot = animate ? 0 : 1;
    let finished = false;
    const finish = () => {
      if (finished) return;
      finished = true; bootTween = false; state.boot = 1; draw(); done?.(); wake();
    };
    bootTween = true; wake();
    const tick = now => {
      const ms = now - start;
      if (fly) state.boot = Math.min(1, Math.max(0, (ms - hold) / fly));
      if (ms < hold) coreMat.emissiveIntensity = Math.min(.95, (ms / hold) * .7);
      if (ms < hold + fly && bootTween) requestAnimationFrame(tick);
      else finish();
    };
    requestAnimationFrame(tick);
    return finish;
  }

  // Occasional interference, as in NaviOS and the show. Never while still.
  setInterval(() => { if (!degraded && state.motion && state.boot === 1 && Math.random() < .35) pulse(); }, 21000);

  window.naviScene = {
    setMotion(value) { forcedMotion = !!value; applyMotion(); },
    setPhase(value) {
      const next = value === 'working' || value === 'error' ? value : 'idle';
      const entering = state.phase !== 'working' && next === 'working';
      state.phase = next;
      setLevel(ENERGY[next], entering);
    },
    setEnergy(level) { setLevel(level, false); },
    setLayout(stageX, dim = 0) { state.stageXTarget = stageX; state.dimTarget = dim; wake(); },
    setFrame(box, dim) { applyFrame(box, dim); if (!state.motion) { settle(); draw(); } wake(); },
    glitch() { if (state.motion) pulse(); },
    boot, degrade, renderer, scene, bounds,
  };

  canvas.addEventListener('webglcontextlost', event => {
    event.preventDefault();
    contextLost = true;
    cancelAnimationFrame(raf); raf = undefined;
  });
  canvas.addEventListener('webglcontextrestored', () => {
    contextLost = false; failures = 0; resize(); wake();
  });
  document.addEventListener('visibilitychange', () => {
    state.visible = !document.hidden;
    if (state.visible) wake();
    else { cancelAnimationFrame(raf); raf = undefined; }
  });
  addEventListener('resize', resize);
  applyFrame(readCell());
  resize();
  wake();
  canvas.dataset.renderer = 'webgl';
}
