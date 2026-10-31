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

const canvas = document.getElementById('field');
let renderer;
try { renderer = new THREE.WebGLRenderer({canvas, antialias: true, powerPreference: 'default'}); }
catch { renderer = undefined; }

if (!renderer) {
  canvas.style.background = 'radial-gradient(ellipse at 22% 48%,#1638ff,#030b6a 45%,#01052e 80%)';
  window.naviScene = {setMotion() {}, setEnergy() {}, setLayout() {}, boot(done) { done?.(); }, glitch() {}};
  canvas.dataset.renderer = 'fallback';
} else {
  const color = hex => new THREE.Color(hex);
  renderer.setPixelRatio(Math.min(devicePixelRatio, 1.25));
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
  const halo = new THREE.Sprite(additive(softGlow, 0x1d5cff, .6)); halo.scale.set(4.2, 4.2, 1); halo.position.z = -.6; stage.add(halo);

  // ── Floor: the dark ellipse the emblem hovers over, lit along its rim ────────
  const floor = new THREE.Mesh(new THREE.CircleGeometry(1.75, 160), new THREE.ShaderMaterial({
    transparent: true, depthWrite: false,
    uniforms: {uTime: {value: 0}, uBoot: {value: 0}, uDark: {value: color('#000524')}, uRim: {value: color('#2d5bff')}, uLift: {value: color('#0d34ff')}},
    vertexShader: 'varying vec2 vUv; void main(){ vUv = uv; gl_Position = projectionMatrix * modelViewMatrix * vec4(position,1.0); }',
    fragmentShader: `
      varying vec2 vUv; uniform float uTime, uBoot; uniform vec3 uDark, uRim, uLift;
      void main(){
        vec2 p = vUv * 2.0 - 1.0; float r = length(p);
        vec3 col = mix(uDark, uRim, smoothstep(0.72, 0.99, r));
        col += uLift * 0.55 * pow(max(0.0, 1.0 - r * 1.6), 2.0) * (0.85 + 0.15 * sin(uTime * 1.3));
        col += uRim * 0.6 * smoothstep(0.35, 1.0, -p.y) * smoothstep(0.86, 1.0, r);
        float alpha = (0.93 - 0.2 * smoothstep(0.97, 1.0, r)) * uBoot;
        gl_FragColor = vec4(col, alpha);
      }`,
  }));
  floor.rotation.x = -Math.PI / 2; floor.position.y = -1.5; stage.add(floor);

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
  for (const [angle, radius, height, y] of [[.7, 1.45, 3.1, -.1], [2.0, 2.05, 3.6, .1], [3.6, 1.7, 2.6, .35], [4.8, 2.25, 3.2, -.2], [5.7, 1.35, 2.2, .5]]) {
    const hair = new THREE.Mesh(new THREE.PlaneGeometry(.009, height), lineMat);
    hair.position.set(Math.sin(angle) * radius, y, Math.cos(angle) * radius); orbit.add(hair);
  }

  // ── Sweeping arcs ────────────────────────────────────────────────────────────
  const arcs = [];
  const arcMat = new THREE.MeshBasicMaterial({color: 0x86ecff, transparent: true, opacity: .8, depthWrite: false, blending: THREE.AdditiveBlending});
  fadeables.push([arcMat, .8]);
  for (const [radius, arc, rx, rz, speed] of [[2.35, 1.35, 1.36, .2, .09], [2.9, 1.05, 1.22, -.22, -.06], [1.95, .9, 1.48, .05, .14]]) {
    const holder = new THREE.Group(); holder.rotation.set(rx, 0, rz); stage.add(holder);
    const mesh = new THREE.Mesh(new THREE.TorusGeometry(radius, .0075, 6, 360, Math.PI * arc), arcMat);
    holder.add(mesh); arcs.push([mesh, speed]);
  }

  // ── Wandering lights that circle in front of and behind the emblem ───────────
  const wisps = [];
  for (const [radius, tilt, speed, phase, size] of [[1.32, .5, .42, 0, .075], [1.6, -.35, -.3, 2.2, .06]]) {
    const plane = new THREE.Group(); plane.rotation.set(1.25, 0, tilt); stage.add(plane);
    const body = new THREE.Mesh(new THREE.SphereGeometry(size, 20, 14), new THREE.MeshBasicMaterial({color: 0xdff8ff}));
    const glow = new THREE.Sprite(additive(softGlow, 0x62c8ff, .9)); glow.scale.set(size * 9, size * 9, 1);
    body.add(glow); plane.add(body); wisps.push({body, radius, speed, phase});
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

  // ── Framing: the stage sits in the left third; the lens stays centred on it ──
  const state = {
    motion: true, visible: true, elapsed: 0, energy: 0, energyTarget: 0,
    stageX: .5, stageXTarget: .5, dim: 0, dimTarget: 0, boot: 0, glitch: 0,
  };
  const pointer = {x: 0, y: 0}, smooth = {x: 0, y: 0};
  addEventListener('pointermove', e => { pointer.x = e.clientX / innerWidth - .5; pointer.y = e.clientY / innerHeight - .5; }, {passive: true});

  function frame() {
    const w = innerWidth, h = innerHeight, p = Math.min(.5, Math.max(.05, state.stageX));
    const full = 2 * (1 - p) * w;   // virtual lens width whose centre lands at p·w
    camera.aspect = full / h;
    camera.setViewOffset(full, h, full / 2 - p * w, 0, w, h);
    camera.updateProjectionMatrix();
  }
  function resize() {
    renderer.setSize(innerWidth, innerHeight, false);
    composer.setPixelRatio(renderer.getPixelRatio());
    composer.setSize(innerWidth, innerHeight);
    bloom.resolution.set(innerWidth / 2, innerHeight / 2);
    video.uniforms.uRes.value.set(innerWidth * renderer.getPixelRatio(), innerHeight * renderer.getPixelRatio());
    const compact = innerWidth < 1100 || innerHeight < 700;
    stage.scale.setScalar(compact ? .68 : .8);
    frame(); draw();
  }

  function draw() {
    const t = state.elapsed, e = state.energy, b = state.boot;
    const introEase = b * b * (3 - 2 * b);
    backdrop.material.uniforms.uTime.value = t;
    backdrop.material.uniforms.uBoot.value = introEase;
    backdrop.material.uniforms.uEnergy.value = e;
    floor.material.uniforms.uTime.value = t;
    floor.material.uniforms.uBoot.value = introEase;
    for (const [material, opacity] of fadeables) material.opacity = opacity * introEase * (1 - state.dim * .55);
    halo.material.opacity = .3 + .2 * introEase + .12 * Math.sin(t * 1.7) * e;

    camera.position.set(smooth.x * .9, .55 + introEase * .1 - smooth.y * .5, 7.6 - (1 - introEase) * .8);
    camera.lookAt(0, -.05 - (1 - introEase) * .15, 0);
    frame();

    stage.position.y = (1 - introEase) * .35 + Math.sin(t * .6) * .05;
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
    renderer.domElement.style.opacity = String(1 - state.dim * .35);
    composer.render();
  }

  let raf, last = 0, carry = 0;
  const STEP = 1 / 30; // the anime's television cadence; also spares the GPU
  function loop(now) {
    raf = requestAnimationFrame(loop);
    const dt = last ? Math.min(.1, (now - last) / 1000) : 0; last = now;
    carry += dt; if (carry < STEP) return;
    const step = carry; carry = 0;
    if (state.motion) state.elapsed += step;
    const k = 1 - Math.pow(.02, step);
    state.energy += (state.energyTarget - state.energy) * k * .8;
    state.stageX += (state.stageXTarget - state.stageX) * k;
    state.dim += (state.dimTarget - state.dim) * k;
    state.glitch = Math.max(0, state.glitch - step * 8);
    smooth.x += (pointer.x - smooth.x) * k * .25; smooth.y += (pointer.y - smooth.y) * k * .25;
    draw();
    const settled = !state.motion && Math.abs(state.stageXTarget - state.stageX) < .001 && Math.abs(state.dimTarget - state.dim) < .002 && !bootTween;
    if (settled) { cancelAnimationFrame(raf); raf = undefined; }
  }
  const wake = () => { if (!raf && state.visible) { last = 0; raf = requestAnimationFrame(loop); } };

  let bootTween;
  function boot(done, instant) {
    if (instant) { state.boot = 1; draw(); done?.(); return; }
    const start = performance.now(), hold = 2.6, fly = 1.5;
    bootTween = true; wake();
    const tick = now => {
      const s = (now - start) / 1000;
      state.boot = Math.min(1, Math.max(0, (s - hold) / fly));
      if (s < hold) coreMat.emissiveIntensity = Math.min(.95, s * .7);
      if (state.boot < 1 && bootTween) requestAnimationFrame(tick);
      else { bootTween = false; state.boot = 1; done?.(); wake(); }
    };
    requestAnimationFrame(tick);
    return () => { if (bootTween) { bootTween = false; state.boot = 1; draw(); done?.(); wake(); } };
  }

  // Occasional interference, as in NaviOS and the show. Never while still.
  setInterval(() => { if (state.motion && state.boot === 1 && Math.random() < .35) window.naviScene.glitch(); }, 21000);

  window.naviScene = {
    setMotion(value) { state.motion = value; wake(); if (!value) draw(); },
    setEnergy(level) {
      const surge = level - state.energyTarget > .5;
      state.energyTarget = level;
      if (surge && state.motion) window.naviScene.glitch();
      wake();
    },
    setLayout(stageX, dim = 0) { state.stageXTarget = stageX; state.dimTarget = dim; wake(); },
    glitch() { state.glitch = 1; video.uniforms.uSeed.value = Math.random() * 100; wake(); },
    boot, renderer, scene,
  };
  document.addEventListener('visibilitychange', () => {
    state.visible = !document.hidden;
    if (state.visible) wake();
    else { cancelAnimationFrame(raf); raf = undefined; }
  });
  addEventListener('resize', resize);
  resize();
  wake();
  canvas.dataset.renderer = 'webgl';
}
