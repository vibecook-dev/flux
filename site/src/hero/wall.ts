/**
 * The hero's wall of posters: one WebGL2 draw call of instanced quads on a
 * plane leaning back into the dark, each column drifting at its own pace.
 * The pointer is a light you carry across it, and the plane turns a little
 * towards it. Posters come from one canvas atlas (posters.ts), mipmapped with
 * anisotropic filtering so the far rows stay sharp. It draws only while the
 * hero is on screen, and once (no drift) for reduced motion.
 */
import { onVisibility, reducedMotion } from '../lib/motion';
import { POSTERS, TITLE_FONT } from './posters';

const COLS = 25;
const ROWS = 16;
const ATLAS_COLS = 8;
const ATLAS_ROWS = 4;
const PW = 1;
const PH = 1.5;
const PITCH_X = 1.17;
const PITCH_Y = 1.68;
const BG: [number, number, number] = [7 / 255, 7 / 255, 10 / 255];

const VS = `#version 300 es
in vec2 aCorner;
in vec3 aCell;
uniform mat4 uMvp;
uniform mat4 uView;
uniform float uTime;
uniform float uLift;
out vec2 vUv;
out float vTex;
out float vDepth;
void main() {
  float col = aCell.x;
  float speed = 0.11 + 0.07 * fract(sin(col * 12.9898 + 4.1) * 43758.5453);
  float total = ${ROWS.toFixed(1)} * ${PITCH_Y.toFixed(3)};
  // Every other column starts half a poster lower, like bricks.
  float y0 = aCell.y * ${PITCH_Y.toFixed(3)} + mod(col, 2.0) * ${(PITCH_Y / 2).toFixed(3)};
  float cy = mod(y0 + uTime * speed + uLift, total) - total * 0.5;
  float cx = (col - ${((COLS - 1) / 2).toFixed(1)}) * ${PITCH_X.toFixed(3)};
  // The quad is a hair larger than the poster so its antialiased edge fits inside.
  vec2 pad = vec2(${(PW * 1.04).toFixed(3)}, ${(PH * 1.03).toFixed(3)});
  vec3 p = vec3(cx + (aCorner.x - 0.5) * pad.x, cy + (aCorner.y - 0.5) * pad.y, 0.0);
  gl_Position = uMvp * vec4(p, 1.0);
  vDepth = -(uView * vec4(p, 1.0)).z;
  vUv = (aCorner - 0.5) * pad / vec2(${PW.toFixed(1)}, ${PH.toFixed(1)}) + 0.5;
  vTex = aCell.z;
}`;

const FS = `#version 300 es
precision highp float;
in vec2 vUv;
in float vTex;
in float vDepth;
uniform sampler2D uAtlas;
uniform vec2 uPointer;
uniform float uLightR;
uniform vec2 uFog;
uniform vec3 uBg;
uniform float uFade;
out vec4 outColor;
float box(vec2 p, vec2 b, float r) {
  vec2 q = abs(p) - b + r;
  return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}
void main() {
  vec2 p = (vUv - 0.5) * vec2(1.0, 1.5);
  float d = box(p, vec2(0.5, 0.75), 0.04);
  float aa = fwidth(d) * 0.8;
  float mask = 1.0 - smoothstep(-aa, aa, d);
  if (mask <= 0.0) discard;
  vec2 cell = vec2(mod(vTex, ${ATLAS_COLS.toFixed(1)}), floor(vTex / ${ATLAS_COLS.toFixed(1)}));
  vec2 uv = clamp(vec2(vUv.x, 1.0 - vUv.y), 0.004, 0.996);
  vec3 c = texture(uAtlas, (cell + uv) / vec2(${ATLAS_COLS.toFixed(1)}, ${ATLAS_ROWS.toFixed(1)})).rgb;
  // A sheen across the top of each print, and a rim that catches the light.
  c += 0.05 * smoothstep(0.55, 1.0, vUv.y);
  float rim = 1.0 - smoothstep(0.0, aa * 3.0 + 0.004, abs(d));
  // The room is dark; the pointer carries a pool of light.
  float l = 1.0 - smoothstep(0.0, uLightR, distance(gl_FragCoord.xy, uPointer));
  float lit = 0.3 + 1.0 * l * l;
  c = c * lit + rim * 0.22 * (0.25 + l);
  float fog = smoothstep(uFog.x, uFog.y, vDepth);
  c = mix(c, uBg, fog * 0.92);
  outColor = vec4(mix(uBg, c, mask * uFade), 1.0);
}`;

type Mat4 = Float32Array;

function perspective(fovY: number, aspect: number, near: number, far: number): Mat4 {
  const f = 1 / Math.tan(fovY / 2);
  const m = new Float32Array(16);
  m[0] = f / aspect;
  m[5] = f;
  m[10] = (far + near) / (near - far);
  m[11] = -1;
  m[14] = (2 * far * near) / (near - far);
  return m;
}

function mul(a: Mat4, b: Mat4): Mat4 {
  const o = new Float32Array(16);
  for (let i = 0; i < 4; i++)
    for (let j = 0; j < 4; j++) {
      let s = 0;
      for (let k = 0; k < 4; k++) s += a[k * 4 + j] * b[i * 4 + k];
      o[i * 4 + j] = s;
    }
  return o;
}

function rotX(t: number): Mat4 {
  const c = Math.cos(t);
  const s = Math.sin(t);
  return new Float32Array([1, 0, 0, 0, 0, c, s, 0, 0, -s, c, 0, 0, 0, 0, 1]);
}

function rotY(t: number): Mat4 {
  const c = Math.cos(t);
  const s = Math.sin(t);
  return new Float32Array([c, 0, -s, 0, 0, 1, 0, 0, s, 0, c, 0, 0, 0, 0, 1]);
}

function rotZ(t: number): Mat4 {
  const c = Math.cos(t);
  const s = Math.sin(t);
  return new Float32Array([c, s, 0, 0, -s, c, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1]);
}

function translate(x: number, y: number, z: number): Mat4 {
  return new Float32Array([1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, x, y, z, 1]);
}

function compile(gl: WebGL2RenderingContext, type: number, src: string): WebGLShader {
  const s = gl.createShader(type)!;
  gl.shaderSource(s, src);
  gl.compileShader(s);
  if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s) ?? 'shader');
  return s;
}

const idle = () => new Promise<void>((r) => ('requestIdleCallback' in window ? requestIdleCallback(() => r(), { timeout: 120 }) : setTimeout(r, 16)));

/** Paint every poster into one atlas canvas, a few at a time so nothing stutters. */
async function paintAtlas(cellW: number): Promise<HTMLCanvasElement> {
  const cellH = Math.round(cellW * 1.5);
  const atlas = document.createElement('canvas');
  atlas.width = cellW * ATLAS_COLS;
  atlas.height = cellH * ATLAS_ROWS;
  const ctx = atlas.getContext('2d')!;
  try {
    await Promise.race([
      Promise.all([document.fonts.load(`800 condensed 40px ${TITLE_FONT}`), document.fonts.load(`600 20px ${TITLE_FONT}`)]),
      new Promise((r) => setTimeout(r, 2500)),
    ]);
  } catch {
    // The fallback faces will do.
  }
  for (let i = 0; i < ATLAS_COLS * ATLAS_ROWS; i++) {
    const x = (i % ATLAS_COLS) * cellW;
    const y = Math.floor(i / ATLAS_COLS) * cellH;
    ctx.save();
    ctx.translate(x, y);
    ctx.beginPath();
    ctx.rect(0, 0, cellW, cellH);
    ctx.clip();
    POSTERS[i % POSTERS.length].paint(ctx, cellW, cellH);
    ctx.restore();
    if (i % 4 === 3) await idle();
  }
  return atlas;
}

/** Which poster each place on the wall shows: never the same as its neighbours. */
function layout(): Float32Array {
  const data = new Float32Array(COLS * ROWS * 3);
  const pick: number[][] = [];
  let seed = 7;
  const rand = () => {
    seed = (seed * 16807) % 2147483647;
    return seed / 2147483647;
  };
  const n = ATLAS_COLS * ATLAS_ROWS;
  for (let c = 0; c < COLS; c++) {
    pick[c] = [];
    for (let r = 0; r < ROWS; r++) {
      let t = 0;
      for (let tries = 0; tries < 20; tries++) {
        t = Math.floor(rand() * n);
        const near = [pick[c][r - 1], pick[c - 1]?.[r], pick[c - 1]?.[r - 1], pick[c - 1]?.[r + 1], pick[c][r - 2]];
        if (!near.includes(t)) break;
      }
      pick[c][r] = t;
      const i = (c * ROWS + r) * 3;
      data[i] = c;
      data[i + 1] = r;
      data[i + 2] = t;
    }
  }
  return data;
}

export interface Wall {
  /** 0 at the top of the page, 1 once the hero has scrolled away. */
  setScroll(p: number): void;
}

export async function startWall(host: HTMLElement, canvas: HTMLCanvasElement): Promise<Wall | null> {
  const gl = canvas.getContext('webgl2', { alpha: false, antialias: false, depth: false, stencil: false, powerPreference: 'default' });
  if (!gl) return null;

  const prog = gl.createProgram()!;
  gl.attachShader(prog, compile(gl, gl.VERTEX_SHADER, VS));
  gl.attachShader(prog, compile(gl, gl.FRAGMENT_SHADER, FS));
  gl.linkProgram(prog);
  if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) return null;
  gl.useProgram(prog);
  const u = (name: string) => gl.getUniformLocation(prog, name);
  const U = {
    mvp: u('uMvp'),
    view: u('uView'),
    time: u('uTime'),
    lift: u('uLift'),
    pointer: u('uPointer'),
    lightR: u('uLightR'),
    fog: u('uFog'),
    bg: u('uBg'),
    fade: u('uFade'),
    atlas: u('uAtlas'),
  };

  const vao = gl.createVertexArray();
  gl.bindVertexArray(vao);
  const corners = gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER, corners);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([0, 0, 1, 0, 0, 1, 1, 1]), gl.STATIC_DRAW);
  const aCorner = gl.getAttribLocation(prog, 'aCorner');
  gl.enableVertexAttribArray(aCorner);
  gl.vertexAttribPointer(aCorner, 2, gl.FLOAT, false, 0, 0);
  const cells = gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER, cells);
  gl.bufferData(gl.ARRAY_BUFFER, layout(), gl.STATIC_DRAW);
  const aCell = gl.getAttribLocation(prog, 'aCell');
  gl.enableVertexAttribArray(aCell);
  gl.vertexAttribPointer(aCell, 3, gl.FLOAT, false, 0, 0);
  gl.vertexAttribDivisor(aCell, 1);

  // Posters: sharp at their size on this screen, never more than 320 px wide.
  const dpr = Math.min(window.devicePixelRatio || 1, 2);
  const cellW = Math.round(Math.min(320, Math.max(160, (Math.min(window.innerWidth, 1800) / 9) * dpr)));
  const atlas = await paintAtlas(cellW);
  const tex = gl.createTexture();
  gl.activeTexture(gl.TEXTURE0);
  gl.bindTexture(gl.TEXTURE_2D, tex);
  gl.pixelStorei(gl.UNPACK_COLORSPACE_CONVERSION_WEBGL, gl.NONE);
  gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, atlas);
  gl.generateMipmap(gl.TEXTURE_2D);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR_MIPMAP_LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  const aniso = gl.getExtension('EXT_texture_filter_anisotropic');
  if (aniso) gl.texParameterf(gl.TEXTURE_2D, aniso.TEXTURE_MAX_ANISOTROPY_EXT, Math.min(8, gl.getParameter(aniso.MAX_TEXTURE_MAX_ANISOTROPY_EXT)));
  gl.uniform1i(U.atlas, 0);
  gl.uniform3f(U.bg, BG[0], BG[1], BG[2]);
  gl.clearColor(BG[0], BG[1], BG[2], 1);

  // ── state ──
  let width = 0;
  let height = 0;
  let pointer = { x: 0.5, y: 0.32 }; // where the light is aimed (fractions of the canvas)
  let light = { x: 0.5, y: 0.32 }; // where it is, easing towards the aim
  let lift = 0;
  let scroll = 0;
  // Until the pointer moves (and always on touch screens), the light wanders by itself.
  let wander = true;
  const still = reducedMotion();
  let visible = true;
  let raf = 0;
  const t0 = performance.now();

  const resize = () => {
    const r = canvas.getBoundingClientRect();
    width = Math.max(1, Math.round(r.width * dpr));
    height = Math.max(1, Math.round(r.height * dpr));
    if (canvas.width !== width || canvas.height !== height) {
      canvas.width = width;
      canvas.height = height;
    }
    gl.viewport(0, 0, width, height);
    request();
  };

  const draw = (now: number) => {
    const time = still ? 8 : (now - t0) / 1000;
    if (wander && !still) pointer = { x: 0.5 + 0.32 * Math.sin(time * 0.21), y: 0.36 + 0.16 * Math.cos(time * 0.17) };
    const k = still ? 1 : 0.055;
    light.x += (pointer.x - light.x) * k;
    light.y += (pointer.y - light.y) * k;
    lift += ((scroll * 2.4) - lift) * (still ? 1 : 0.12);

    const aspect = width / height;
    const portrait = aspect < 0.9;
    const fov = (portrait ? 54 : 34) * (Math.PI / 180);
    const proj = perspective(fov, aspect, 0.5, 80);
    // The wall leans back and turns a little, and a little more towards the light.
    const view = mul(
      translate(0, portrait ? 0.4 : 0.6, portrait ? -14 : -14),
      mul(rotX(-0.7 - (light.y - 0.4) * 0.06 - scroll * 0.12), mul(rotY((light.x - 0.5) * 0.08), rotZ(0.2))),
    );
    gl.uniformMatrix4fv(U.mvp, false, mul(proj, view));
    gl.uniformMatrix4fv(U.view, false, view);
    gl.uniform1f(U.time, time);
    gl.uniform1f(U.lift, lift);
    gl.uniform2f(U.pointer, light.x * width, (1 - light.y) * height);
    gl.uniform1f(U.lightR, Math.max(width, height) * 0.42);
    gl.uniform2f(U.fog, 12, 34);
    gl.uniform1f(U.fade, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.drawArraysInstanced(gl.TRIANGLE_STRIP, 0, 4, COLS * ROWS);
  };

  const loop = (now: number) => {
    raf = 0;
    draw(now);
    const settling = Math.abs(pointer.x - light.x) + Math.abs(pointer.y - light.y) > 0.001 || Math.abs(scroll * 2.4 - lift) > 0.001;
    if (visible && !document.hidden && (!still || settling)) raf = requestAnimationFrame(loop);
  };

  function request() {
    if (!raf && visible && !document.hidden) raf = requestAnimationFrame(loop);
  }

  new ResizeObserver(resize).observe(canvas);
  resize();
  onVisibility(host, (v) => {
    visible = v;
    if (v) request();
  });
  document.addEventListener('visibilitychange', request);
  window.addEventListener(
    'pointermove',
    (e) => {
      if (e.pointerType === 'touch') return;
      const r = canvas.getBoundingClientRect();
      wander = false;
      pointer = { x: (e.clientX - r.left) / r.width, y: (e.clientY - r.top) / r.height };
      request();
    },
    { passive: true },
  );

  host.setAttribute('data-on', '');
  request();

  return {
    setScroll(p: number) {
      scroll = p;
      request();
    },
  };
}
