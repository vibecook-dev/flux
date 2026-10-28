/**
 * Perspective quads for the 3D lenses (Flow's covers, Shelf's tapes): the same
 * shading as the flat quads (signed-distance rounded rectangles, poster textures,
 * glyphs), placed with CSS's own 3D maths, so a lens ported from CSS 3D lands
 * where it did:
 *
 * - each quad is `translate3d(pos + origin) rotateZ(rz) rotateY(ry) rotateX(rx)
 *   translate3d(-origin)` applied to its corners (CSS order: X first);
 * - the camera is CSS `perspective: persp` with `perspective-origin: origin`
 *   (screen = origin + (p − origin) · persp / (persp − z)), x right, y down,
 *   z towards the viewer;
 * - texture coordinates interpolate perspective-correctly (the GPU divides by w).
 *
 * Draw order is the batch order (painter's algorithm, back to front): there is no
 * depth buffer, so antialiased edges and fades blend correctly.
 *
 * Materials (opt-in per quad, `MAT3D`): a quad can be lit as card, plastic, wood or
 * paint by a lamp in the room and a second light whose highlight follows the
 * pointer, with grain, worn edges, a bevel that catches the light, shade where a
 * shelf hides it and the room reflected at grazing angles. No extra pass and no
 * extra floats: the material rides in the quad's three spare floats (pos.w, rot.w,
 * origin.w), so quads without one (Flow's, the Gallery's) draw exactly as before.
 */
import { BUFFER, TEXTURE, type Gpu } from './device';

/** Floats per quad (eight vec4s; see `Q` below). */
export const QUAD3D_FLOATS = 32;
/** Floats in the camera uniform (`Cam` below). */
const CAM_FLOATS = 20;
const NONE = [0, 0, 0, 0] as const;

export const KIND3D = { solid: 0, poster: 1, text: 2, ring: 3, shadow: 4, sprite: 6 } as const;
/**
 * `openTop`, `openBottom`: the face goes on past the quad's top (bottom) edge, in
 * another quad printed on the same card (a cover's band above its poster), so that
 * edge is no edge of the box: square, and not bevelled, worn or hairlined.
 */
export const FLAG3D = { gray: 1, hairline: 2, openTop: 4, gradient: 8, mirror: 16, openBottom: 32 } as const;
/**
 * What a quad is made of (`Quad3DBatch.material`). `print`: printed card, satin, with
 * a paper grain and edges that wear white. `spine`: a spine sprite, card or plastic
 * texel by texel (its alpha carries gloss: 1 card, 0.75 plastic or sticker), shaded
 * at its foot and beside its neighbours. `plastic`: a clamshell's faces. `wood`: the
 * shelf's boards. `paint`: bookends.
 */
export const MAT3D = { none: 0, print: 1, spine: 2, plastic: 3, wood: 4, paint: 5 } as const;

export const SCENE3D_WGSL = /* wgsl */ `
struct Cam {
  view: vec2f,
  origin: vec2f,
  persp: f32,
  dpr: f32,
  time: f32,
  _p: f32,
  light: vec4f,   // the room's lamp: position (world) and strength (0: no light; materials draw as painted)
  glint: vec4f,   // a light whose highlight lands under the pointer: position, strength
  room: vec4f,    // ambient, specular, reflection, how deep a shelf's shade reaches (CSS px)
};

struct Q {
  pos: vec4f,     // centre x, y, z; material (whole part) and its gloss (fraction)
  size: vec4f,    // width, height, corner radius, kind
  rot: vec4f,     // rotateX, rotateY, rotateZ, radians (X applied first, as CSS lists them right to left); material seed
  origin: vec4f,  // transform origin, relative to the centre; how shaded the face is (materials, 0–1)
  color: vec4f,   // straight-alpha colour (posters: placeholder; text and sprites: tint)
  uv: vec4f,      // texture rect, or a second colour (gradients)
  tex: vec4f,     // texture (posters: class * 4096 + layer; sprites: layer), texture width, ready-at seconds (< 0: none), opacity
  fx: vec4f,      // flags, mirror line y, mirror strength, sheen
};

@group(0) @binding(0) var<uniform> cam: Cam;
@group(0) @binding(1) var<storage, read> quads: array<Q>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var tex0: texture_2d_array<f32>;
@group(0) @binding(4) var tex1: texture_2d_array<f32>;
@group(0) @binding(5) var tex2: texture_2d_array<f32>;
@group(0) @binding(6) var tex3: texture_2d_array<f32>;
@group(0) @binding(7) var glyphs: texture_2d<f32>;
@group(0) @binding(8) var sprites: texture_2d_array<f32>;
// Materials' grain: four channels of independent noise, tiling, mipmapped (fine grain
// fades by itself where a face is seen too steeply or too small to show it).
@group(0) @binding(9) var noise: texture_2d<f32>;
@group(0) @binding(10) var nsamp: sampler;

const K_SOLID: u32 = 0u;
const K_POSTER: u32 = 1u;
const K_TEXT: u32 = 2u;
const K_RING: u32 = 3u;
const K_SHADOW: u32 = 4u;
const K_SPRITE: u32 = 6u;

const F_GRAY: u32 = 1u;
const F_HAIRLINE: u32 = 2u;
const F_OPEN_TOP: u32 = 4u;
const F_GRADIENT: u32 = 8u;
const F_MIRROR: u32 = 16u;
const F_OPEN_BOTTOM: u32 = 32u;
// How far the face goes on past an open edge, for the quad's edges: past where any edge effect reaches.
const OPEN: f32 = 64.0;

const M_PRINT: u32 = 1u;
const M_SPINE: u32 = 2u;
const M_PLASTIC: u32 = 3u;
const M_WOOD: u32 = 4u;
const M_PAINT: u32 = 5u;

struct VOut {
  @builtin(position) pos: vec4f,
  @location(0) local: vec2f,
  @location(1) @interpolate(flat) half: vec2f,
  @location(2) @interpolate(flat) idx: u32,
  // For materials: the point in the world, the quad's own axes (x right, y down), and
  // the lamp's light at the corners (it changes slowly across a face): diffuse, the
  // normal against the half vector, Fresnel, how much of the reflection is ceiling.
  @location(3) wpos: vec3f,
  @location(4) @interpolate(flat) ax: vec3f,
  @location(5) @interpolate(flat) ay: vec3f,
  @location(6) lamp: vec4f,
};

fn rotX(p: vec3f, a: f32) -> vec3f {
  let c = cos(a);
  let s = sin(a);
  return vec3f(p.x, c * p.y - s * p.z, s * p.y + c * p.z);
}

fn rotY(p: vec3f, a: f32) -> vec3f {
  let c = cos(a);
  let s = sin(a);
  return vec3f(c * p.x + s * p.z, p.y, -s * p.x + c * p.z);
}

fn rotZ(p: vec3f, a: f32) -> vec3f {
  let c = cos(a);
  let s = sin(a);
  return vec3f(c * p.x - s * p.y, s * p.x + c * p.y, p.z);
}

@vertex
fn vs(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VOut {
  let q = quads[ii];
  let kind = u32(q.size.w);
  let flags = u32(q.fx.x);
  let half = q.size.xy * 0.5;
  // A little margin so the edges can antialias (not for text or soft shadows, which fill their rect).
  var pad = 1.5;
  if (kind == K_TEXT || kind == K_SHADOW) {
    pad = 0.0;
  }
  let corner = vec2f(f32(vi & 1u), f32(vi >> 1u)) * 2.0 - 1.0;
  let local = corner * (half + vec2f(pad));
  var p = vec3f(local, 0.0) - q.origin.xyz;
  p = rotZ(rotY(rotX(p, q.rot.x), q.rot.y), q.rot.z);
  var v = p + q.origin.xyz + q.pos.xyz;
  if ((flags & F_MIRROR) != 0u) {
    v.y = 2.0 * q.fx.y - v.y;
  }
  var w = 1.0;
  if (cam.persp > 0.0) {
    w = max((cam.persp - v.z) / cam.persp, 0.0001);
  }
  let s = cam.origin + (v.xy - cam.origin) / w;
  var out: VOut;
  out.pos = vec4f((s.x / cam.view.x * 2.0 - 1.0) * w, (1.0 - s.y / cam.view.y * 2.0) * w, 0.5 * w, w);
  out.local = local;
  out.half = half;
  out.idx = ii;
  out.wpos = v;
  out.ax = rotZ(rotY(rotX(vec3f(1.0, 0.0, 0.0), q.rot.x), q.rot.y), q.rot.z);
  out.ay = rotZ(rotY(rotX(vec3f(0.0, 1.0, 0.0), q.rot.x), q.rot.y), q.rot.z);
  out.lamp = vec4f(0.0);
  if (u32(q.pos.w) != 0u && cam.light.w > 0.0) {
    let n = cross(out.ax, out.ay);
    let ve = normalize(vec3f(cam.origin, cam.persp) - v);
    let l = normalize(cam.light.xyz - v);
    let f1 = 1.0 - clamp(dot(n, ve), 0.0, 1.0);
    let f2 = f1 * f1;
    let rr = reflect(-ve, n);
    out.lamp = vec4f(clamp((dot(n, l) + 0.4) / 1.4, 0.0, 1.0), max(dot(n, normalize(l + ve)), 0.0), f2 * f2 * f1, smoothstep(-0.25, 0.9, -rr.y));
  }
  return out;
}

fn roundRect(p: vec2f, b: vec2f, r: f32) -> f32 {
  let d = abs(p) - b + vec2f(r);
  return length(max(d, vec2f(0.0))) + min(max(d.x, d.y), 0.0) - r;
}

// Explicit gradients: anisotropic filtering keeps a cover seen at a steep angle sharp
// along its long axis (an isotropic mip level would blur it).
fn poster(cls: u32, uv: vec2f, layer: i32, gx: vec2f, gy: vec2f) -> vec4f {
  switch cls {
    case 0u: { return textureSampleGrad(tex0, samp, uv, layer, gx, gy); }
    case 1u: { return textureSampleGrad(tex1, samp, uv, layer, gx, gy); }
    case 2u: { return textureSampleGrad(tex2, samp, uv, layer, gx, gy); }
    default: { return textureSampleGrad(tex3, samp, uv, layer, gx, gy); }
  }
}

// The noise texture, k texels per face unit along each axis, offset by o texels.
// dx, dy: how far the face moves per device pixel, for the mip level and anisotropy.
fn grain(p: vec2f, k: vec2f, o: vec2f, dx: vec2f, dy: vec2f) -> vec4f {
  let s = k * (1.0 / 128.0);
  return textureSampleGrad(noise, nsamp, p * s + o * (1.0 / 128.0), dx * s, dy * s);
}

// A material in the room's light: its grain and wear, a bevel at its edges, shade where
// a shelf hides it, the lamp's light, highlights from the lamp and from the light that
// follows the pointer, and the room reflected at grazing angles (Fresnel).
// Noise channels (see noiseTexture): r, g white per texel; b, a smooth (4 and 16 texels).
fn material(q: Q, in: VOut, base: vec3f, glossIn: f32, d0: f32, dx: vec2f, dy: vec2f) -> vec3f {
  let mat = u32(q.pos.w);
  let seed = q.rot.w;
  let shade = q.origin.w;
  let p = in.local;
  let half = in.half;
  // Its edges (an open top or bottom is no edge: see fs).
  let openT = select(0.0, OPEN, (u32(q.fx.x) & F_OPEN_TOP) != 0u);
  let openB = select(0.0, OPEN, (u32(q.fx.x) & F_OPEN_BOTTOM) != 0u);
  let pe = p + vec2f(0.0, openT - openB);
  let he = half + vec2f(0.0, openT + openB);
  var col = base;
  var gloss = glossIn;
  var occ = 1.0;
  var bevel = 1.3;
  if (q.size.z <= 0.0) {
    // A face cut in two where it leaves the shelf: no edge at the cut.
    bevel = 0.0;
  }
  if (mat == M_WOOD) {
    // Veneer along the board: broad warped figure, the grain's fine streaks (filled a
    // shade lighter, like open-pored ash), and pores. The figure's bands fade before
    // they get finer than a few pixels.
    let warp = grain(p, vec2f(0.064, 0.8), vec2f(seed * 61.0, 0.0), dx, dy).a * 2.6;
    let drift = grain(p, vec2f(0.0176, 0.288), vec2f(seed * 23.0, 7.0), dx, dy).a;
    let band = fract(p.y * 0.07 + warp + drift * 1.8);
    let fine = clamp(1.0 - (abs(dx.y) + abs(dy.y)) * 0.25, 0.0, 1.0);
    let figure = (smoothstep(0.0, 0.25, band) * (1.0 - smoothstep(0.45, 1.0, band)) - 0.5) * fine + 0.5;
    let streak = smoothstep(0.62, 0.95, grain(p + vec2f(0.0, warp * 2.7), vec2f(0.048, 4.4), vec2f(seed * 41.0, 3.0), dx, dy).b);
    let pores = grain(p, vec2f(0.25, 2.6), vec2f(0.0, seed * 7.0), dx, dy).r;
    // A pale laminate (a bright room) shows its grain less than dark veneer does.
    let k = clamp((0.75 - cam.room.x) * 4.0, 0.35, 1.0);
    col = col * (1.0 + k * (0.16 * figure - 0.08 + 0.1 * (pores - 0.5))) + vec3f(0.05, 0.047, 0.043) * streak * k;
    bevel = 1.6;
  } else if (mat == M_PRINT || mat == M_PLASTIC) {
    // Card under its print (plastic hardly): fine grain and a mottle. The sheen stays
    // even: gloss that varies in patches reads as smudges once a cover turns to face
    // the lamp. (A spine's grain and wear are painted into its sprite.)
    let n = grain(p, vec2f(1.6), vec2f(seed * 91.0, seed * 37.0), dx, dy);
    var amp = 0.05;
    if (mat == M_PLASTIC) {
      amp = 0.016;
    }
    col = col * (1.0 + (n.r * 0.6 + n.b * 0.4 - 0.5) * amp);
    if (mat == M_PRINT) {
      // Worn card whitens here and there along its edges and at its corners, a few
      // tapes much more than most.
      let edge = 1.0 - smoothstep(0.0, 1.0, -d0);
      let k = max(abs(pe) - he + vec2f(5.0), vec2f(0.0)) / 5.0;
      let corner = min(1.0, k.x * k.y * 1.8);
      if (edge > 0.0 || corner > 0.0) {
        let worn = fract(seed * 7.31);
        let wear = worn * worn;
        let rub = grain(p, vec2f(1.2), vec2f(seed * 211.0, seed * 5.0), dx, dy).b;
        let w = max(edge * smoothstep(0.8 - wear * 0.3, 0.97, rub), corner * smoothstep(0.35, 0.8, rub) * (0.25 + wear));
        col = mix(col, vec3f(0.8, 0.78, 0.74) * (0.55 + 0.45 * cam.room.x), clamp(w, 0.0, 1.0) * (0.25 + 0.35 * wear));
      }
    }
  }
  if (mat == M_SPINE) {
    // Its foot on the board and its edges beside the next tape are in shade.
    let fd = max(1.0 - (half.y - p.y) / max(half.y * 0.18, 3.0), 0.0);
    let sd = max(1.0 - (half.x - abs(p.x)) / 4.2, 0.0);
    occ = 1.0 - shade * (0.32 * fd * fd * fd + 0.24 * sd * sd * sd);
  } else if (mat == M_PRINT || mat == M_PLASTIC) {
    // Deeper into the shelf is darker; beside a neighbour (shade) darker still.
    let into = -in.wpos.z;
    let depth = clamp(into / max(cam.room.w, 1.0), 0.0, 1.0);
    occ = (1.0 - 0.52 * depth) * (1.0 - shade * 0.55 * smoothstep(-1.0, 9.0, into));
  }
  // The bevel: the normal leans out over the last pixel or so of the edge, and an
  // edge that faces up catches the light from above (one that faces down is in shade).
  // The lamp's light comes from the corners, except on a bevel.
  var n = cross(in.ax, in.ay);
  var edgeLight = 0.0;
  var diffuse = in.lamp.x;
  var ndh = in.lamp.y;
  let eye = vec3f(cam.origin, cam.persp);
  let rim = select(0.0, 1.0 - smoothstep(0.0, bevel, -d0), bevel > 0.0);
  if (rim > 0.001) {
    let r = min(q.size.z, min(half.x, half.y));
    let e = abs(pe) - he + vec2f(r);
    var g = select(vec2f(0.0, 1.0), vec2f(1.0, 0.0), e.x > e.y);
    if (e.x > 0.0 && e.y > 0.0) {
      g = normalize(e);
    }
    g = g * sign(pe);
    let outward = in.ax * g.x + in.ay * g.y;
    n = normalize(n + outward * rim * 1.25);
    edgeLight = rim * (0.2 * max(0.0, -outward.y) - 0.12 * max(0.0, outward.y) + 0.05 * max(0.0, outward.x));
    let v = normalize(eye - in.wpos);
    let l = normalize(cam.light.xyz - in.wpos);
    diffuse = clamp((dot(n, l) + 0.4) / 1.4, 0.0, 1.0);
    ndh = max(dot(n, normalize(l + v)), 0.0);
  }
  let amb = cam.room.x;
  let lit = col * (amb + (1.0 - amb) * diffuse * 1.1 + edgeLight * cam.light.w) * occ;
  // How sharp a highlight is comes from the material (card, plastic, a sticker); wear
  // only dims it. (A sharpness that varied with the grain would ripple the highlight
  // into blotches.)
  let s2 = glossIn * glossIn;
  let g2 = gloss * gloss;
  // The lamp: a broad sheen.
  var spec = pow(max(ndh, 0.0001), mix(7.0, 80.0, s2)) * mix(0.03, 0.28, g2) * cam.room.y * cam.light.w;
  // Where the pointer's light lands on the spines: far from there it adds nothing.
  let hp = cam.origin + (cam.glint.xy - cam.origin) * (cam.persp / (cam.persp + cam.glint.z));
  let toHp = in.wpos.xy - hp;
  if (cam.glint.w > 0.0 && dot(toHp, toHp) < 110000.0) {
    let rr = reflect(-normalize(eye - in.wpos), n);
    // The pointer's light is a short strip, tilted like the light across a sleeve: its
    // reflection is a band through the pointer. The point on the strip nearest the
    // reflected ray lights this one; the strip fades towards its ends.
    let dir = vec3f(0.423, 0.906, 0.0);
    let w0 = in.wpos - cam.glint.xyz;
    let b = dot(rr, dir);
    let s = (dot(dir, w0) - b * dot(rr, w0)) / max(1.0 - b * b, 0.0001);
    let at = clamp(s, -420.0, 420.0);
    let lg = normalize(cam.glint.xyz + dir * at - in.wpos);
    let along = 1.0 - smoothstep(140.0, 420.0, abs(s));
    spec = spec + pow(max(dot(rr, lg), 0.0), mix(60.0, 900.0, s2)) * mix(0.05, 0.42, g2) * cam.room.y * cam.glint.w * along;
  }
  // The room reflected at grazing angles.
  let env = mix(0.015, 0.24, in.lamp.w) * (0.04 + 0.96 * in.lamp.z) * mix(0.3, 1.0, gloss) * cam.room.z;
  return lit + vec3f(1.0, 0.97, 0.92) * ((spec + env) * occ);
}

@fragment
fn fs(in: VOut) -> @location(0) vec4f {
  // Screen-space rates first (derivatives need uniform control flow). Antialias with
  // the distance field's own rate, so a face seen nearly edge-on (a tape's side, a
  // cover turned away) keeps crisp edges; a width taken from the quad's coordinates
  // would smear them across the steep axis. The face's rate of change per device
  // pixel (gx, gy) drives anisotropic texture filtering.
  let q = quads[in.idx];
  let flags = u32(q.fx.x);
  let radius = min(q.size.z, min(in.half.x, in.half.y));
  // Distance to the quad's edges (its hairline and material), and to its outline. An
  // open top or bottom is no edge, the face going on past it, but still where the quad ends.
  let openT = select(0.0, OPEN, (flags & F_OPEN_TOP) != 0u);
  let openB = select(0.0, OPEN, (flags & F_OPEN_BOTTOM) != 0u);
  let d0 = roundRect(in.local + vec2f(0.0, openT - openB), in.half + vec2f(0.0, openT + openB), radius);
  let cutT = select(-1e9, -in.local.y - in.half.y, openT > 0.0);
  let cutB = select(-1e9, in.local.y - in.half.y, openB > 0.0);
  let shape = max(d0, max(cutT, cutB));
  let aa = max(fwidth(shape), 0.0001);
  let dlx = dpdx(in.local);
  let dly = dpdy(in.local);
  let gx = dlx / (2.0 * in.half);
  let gy = dly / (2.0 * in.half);
  let kind = u32(q.size.w);
  let mat = u32(q.pos.w);
  var gloss = fract(q.pos.w);
  let f = clamp(in.local / (2.0 * in.half) + 0.5, vec2f(0.0), vec2f(1.0));
  var c = q.color;
  var cover = 1.0;
  if (kind == K_TEXT) {
    cover = textureSampleLevel(glyphs, samp, mix(q.uv.xy, q.uv.zw, f), 0.0).r;
  } else if (kind == K_SHADOW) {
    let blur = max(q.tex.y, 0.5);
    let d = roundRect(in.local, max(in.half - vec2f(blur), vec2f(0.5)), radius);
    cover = 1.0 - smoothstep(-blur * 0.5, blur * 0.5, d);
  } else {
    let d = shape;
    cover = clamp(0.5 - d / aa, 0.0, 1.0);
    if (kind == K_RING) {
      cover = cover - clamp(0.5 - (d + q.tex.y) / aa, 0.0, 1.0);
    } else if (kind == K_SPRITE) {
      let s = textureSampleLevel(sprites, samp, mix(q.uv.xy, q.uv.zw, f), i32(q.tex.x), 0.0);
      // Sprites are stored premultiplied; tint multiplies.
      c = vec4f(s.rgb / max(s.a, 0.0001) * q.color.rgb, s.a * q.color.a);
      if (mat == M_SPINE) {
        // A spine is opaque: its alpha is its gloss (1: card, 0.75: plastic or a sticker).
        gloss = max(gloss, clamp((1.0 - s.a) * 4.0, 0.0, 1.0) * step(0.7, s.a));
        c.a = clamp(s.a / 0.75, 0.0, 1.0) * q.color.a;
      }
    } else if (kind == K_POSTER) {
      if ((flags & F_GRADIENT) != 0u) {
        let dir = vec2f(0.342, 0.940);
        let len = abs(in.half.x * dir.x) + abs(in.half.y * dir.y);
        let g = clamp(dot(in.local, dir) / (2.0 * len) + 0.5, 0.0, 1.0);
        c = mix(q.color, q.uv, g);
      }
      if (q.tex.z >= 0.0) {
        let code = u32(q.tex.x);
        let span = q.uv.zw - q.uv.xy;
        var s = poster(code >> 12u, q.uv.xy + span * f, i32(code & 4095u), gx * span, gy * span);
        if ((flags & F_GRAY) != 0u) {
          let g = dot(s.rgb, vec3f(0.2126, 0.7152, 0.0722)) * 0.6;
          s = vec4f(vec3f(g), s.a);
        }
        let fade = clamp((cam.time - q.tex.z) / 0.28, 0.0, 1.0);
        c = mix(c, vec4f(s.rgb, 1.0), fade);
      }
      if ((flags & F_HAIRLINE) != 0u) {
        let edge = smoothstep(-1.0 - aa, -1.0, d0);
        c = vec4f(mix(c.rgb, vec3f(1.0), 0.07 * edge), c.a);
      }
    }
    // Sheen: a soft highlight across the face (Cover Flow's light on the glass).
    if (q.fx.w != 0.0) {
      let band = 1.0 - smoothstep(0.0, 0.55, abs(f.x - 0.25 - f.y * 0.3));
      c = vec4f(mix(c.rgb, vec3f(1.0), clamp(q.fx.w * band, 0.0, 1.0)), c.a);
    }
    if (mat != 0u && cam.light.w > 0.0) {
      c = vec4f(material(q, in, c.rgb, gloss, d0, dlx, dly), c.a);
    }
  }
  var a = c.a * cover * q.tex.w;
  if ((flags & F_MIRROR) != 0u) {
    // A reflection: strongest at the edge that touches the floor, gone a third of the way down.
    a = a * q.fx.z * smoothstep(0.64, 1.0, f.y);
  }
  return vec4f(c.rgb * a, a);
}
`;

export class Quad3DBatch {
  data: Float32Array;
  n = 0;

  constructor(capacity = 256) {
    this.data = new Float32Array(capacity * QUAD3D_FLOATS);
  }

  reset(): void {
    this.n = 0;
  }

  /** A new quad centred at (x, y, z), w × h, facing the viewer; returns its float offset. */
  add(x: number, y: number, z: number, w: number, h: number, kind: number, radius = 0): number {
    if ((this.n + 1) * QUAD3D_FLOATS > this.data.length) {
      const grown = new Float32Array(this.data.length * 2);
      grown.set(this.data);
      this.data = grown;
    }
    const o = this.n * QUAD3D_FLOATS;
    const d = this.data;
    d.fill(0, o, o + QUAD3D_FLOATS);
    d[o] = x;
    d[o + 1] = y;
    d[o + 2] = z;
    d[o + 4] = w;
    d[o + 5] = h;
    d[o + 6] = radius;
    d[o + 7] = kind;
    d[o + 27] = 1;
    d[o + 26] = -1;
    this.n++;
    return o;
  }

  /** Radians, applied X then Y then Z (CSS `rotateZ() rotateY() rotateX()`), about the origin. */
  rotate(o: number, rx: number, ry: number, rz = 0): void {
    const d = this.data;
    d[o + 8] = rx;
    d[o + 9] = ry;
    d[o + 10] = rz;
  }

  /** The transform origin, relative to the quad's centre (CSS `transform-origin`). */
  origin(o: number, x: number, y: number, z = 0): void {
    const d = this.data;
    d[o + 12] = x;
    d[o + 13] = y;
    d[o + 14] = z;
  }

  color(o: number, c: readonly [number, number, number, number], alpha = 1): void {
    const d = this.data;
    d[o + 16] = c[0];
    d[o + 17] = c[1];
    d[o + 18] = c[2];
    d[o + 19] = c[3] * alpha;
  }

  uv(o: number, a: number, b: number, c: number, e: number): void {
    const d = this.data;
    d[o + 20] = a;
    d[o + 21] = b;
    d[o + 22] = c;
    d[o + 23] = e;
  }

  /** A poster (code from the poster cache) or a sprite layer, its texture width, and when it appeared. */
  texture(o: number, code: number, texWidth: number, readyAt: number): void {
    const d = this.data;
    d[o + 24] = code;
    d[o + 25] = texWidth;
    d[o + 26] = readyAt;
  }

  /** Ring width or shadow blur. */
  extra(o: number, v: number): void {
    this.data[o + 25] = v;
  }

  opacity(o: number, a: number): void {
    this.data[o + 27] = a;
  }

  flags(o: number, f: number): void {
    this.data[o + 28] = f;
  }

  /** Draw this quad mirrored about the horizontal line `y` (a floor), fading with `strength`. */
  mirror(o: number, y: number, strength: number): void {
    const d = this.data;
    d[o + 28] = d[o + 28] | FLAG3D.mirror;
    d[o + 29] = y;
    d[o + 30] = strength;
  }

  sheen(o: number, amount: number): void {
    this.data[o + 31] = amount;
  }

  /**
   * Light this quad as a material (`MAT3D`): how glossy (0–1), a seed (0–1) for its
   * grain and wear, and how shaded it is where a shelf hides it (0–1).
   */
  material(o: number, mat: number, gloss = 0, seed = 0, shade = 0): void {
    const d = this.data;
    d[o + 3] = mat + Math.min(0.999, Math.max(0, gloss));
    d[o + 11] = seed;
    d[o + 15] = shade;
  }
}

export interface Camera {
  /** Canvas size, CSS pixels. */
  width: number;
  height: number;
  /** CSS `perspective` (0: none) and `perspective-origin`, CSS pixels. */
  persp: number;
  originX: number;
  originY: number;
  dpr: number;
  /** Seconds on the shared clock (quads.ts `clock`): poster fade-ins are timed against it. */
  time: number;
  /**
   * Materials only: the room's lamp (x, y, z, strength; strength 0 draws materials as
   * painted), a light whose highlight follows the pointer (x, y, z, strength), and the
   * room (ambient, specular, reflection, how deep a shelf's shade reaches in CSS px).
   */
  light?: readonly [number, number, number, number];
  glint?: readonly [number, number, number, number];
  room?: readonly [number, number, number, number];
}

export interface Textures3D {
  posters: readonly GPUTextureView[];
  glyphs: GPUTextureView;
  sprites: GPUTextureView;
  key: string;
}

/** Where a point lands on screen under `cam` (CSS pixels), or null behind the camera. Matches the shader. */
export function project(x: number, y: number, z: number, cam: Pick<Camera, 'persp' | 'originX' | 'originY'>): { x: number; y: number; scale: number } | null {
  if (cam.persp <= 0) return { x, y, scale: 1 };
  const w = (cam.persp - z) / cam.persp;
  if (w <= 0.0001) return null;
  return { x: cam.originX + (x - cam.originX) / w, y: cam.originY + (y - cam.originY) / w, scale: 1 / w };
}

/** The four corners of a quad as the shader places them (top-left, top-right, bottom-left, bottom-right), for hit tests. */
export function corners(
  q: { x: number; y: number; z: number; w: number; h: number; rx?: number; ry?: number; rz?: number; ox?: number; oy?: number; oz?: number },
  cam: Pick<Camera, 'persp' | 'originX' | 'originY'>,
): ({ x: number; y: number } | null)[] {
  const out: ({ x: number; y: number } | null)[] = [];
  for (const [cx, cy] of [
    [-1, -1],
    [1, -1],
    [-1, 1],
    [1, 1],
  ]) {
    let px = (cx * q.w) / 2 - (q.ox ?? 0);
    let py = (cy * q.h) / 2 - (q.oy ?? 0);
    let pz = -(q.oz ?? 0);
    const rx = q.rx ?? 0;
    const ry = q.ry ?? 0;
    const rz = q.rz ?? 0;
    [py, pz] = [Math.cos(rx) * py - Math.sin(rx) * pz, Math.sin(rx) * py + Math.cos(rx) * pz];
    [px, pz] = [Math.cos(ry) * px + Math.sin(ry) * pz, -Math.sin(ry) * px + Math.cos(ry) * pz];
    [px, py] = [Math.cos(rz) * px - Math.sin(rz) * py, Math.sin(rz) * px + Math.cos(rz) * py];
    out.push(project(px + (q.ox ?? 0) + q.x, py + (q.oy ?? 0) + q.y, pz + (q.oz ?? 0) + q.z, cam));
  }
  return out;
}

/** Whether a screen point is inside a projected quad (its corners from `corners`). */
export function inside(px: number, py: number, c: ({ x: number; y: number } | null)[]): boolean {
  if (c.some((p) => !p)) return false;
  const [a, b, d, e] = c as { x: number; y: number }[];
  // Corners in order around the edge: top-left, top-right, bottom-right, bottom-left.
  const ring = [a, b, e, d];
  let sign = 0;
  for (let i = 0; i < 4; i++) {
    const p = ring[i];
    const q = ring[(i + 1) % 4];
    const cross = (q.x - p.x) * (py - p.y) - (q.y - p.y) * (px - p.x);
    if (cross === 0) continue;
    const s = Math.sign(cross);
    if (sign === 0) sign = s;
    else if (s !== sign) return false;
  }
  return true;
}

interface Shared3D {
  pipeline: GPURenderPipeline;
  sampler: GPUSampler;
  noise: GPUTextureView;
  nsamp: GPUSampler;
}

/** Side of the materials' noise texture, texels (it tiles). */
const NOISE = 128;

/**
 * Four channels of tiling noise for the materials' grain, mipmapped (so grain too
 * fine for the pixels it lands on fades to its average instead of shimmering): r and
 * g are white noise, one value a texel (the sampler's filtering makes it value
 * noise); b and a are smooth noise over 4 and 16 texels. Deterministic; 64 KB.
 */
export function noiseLevels(): Uint8Array[] {
  const N = NOISE;
  let s = 0x2545f491;
  const rnd = () => {
    s ^= s << 13;
    s ^= s >>> 17;
    s ^= s << 5;
    return (s >>> 0) / 4294967296;
  };
  const smooth = (cell: number) => {
    const g = N / cell;
    const grid = Array.from({ length: g * g }, rnd);
    const at = (i: number, j: number) => grid[(j % g) * g + (i % g)];
    const out = new Float32Array(N * N);
    for (let y = 0; y < N; y++) {
      for (let x = 0; x < N; x++) {
        const ix = Math.floor(x / cell);
        const iy = Math.floor(y / cell);
        let tx = x / cell - ix;
        let ty = y / cell - iy;
        tx = tx * tx * (3 - 2 * tx);
        ty = ty * ty * (3 - 2 * ty);
        const top = at(ix, iy) + (at(ix + 1, iy) - at(ix, iy)) * tx;
        const bottom = at(ix, iy + 1) + (at(ix + 1, iy + 1) - at(ix, iy + 1)) * tx;
        out[y * N + x] = top + (bottom - top) * ty;
      }
    }
    return out;
  };
  const b = smooth(4);
  const a = smooth(16);
  let level = new Uint8Array(N * N * 4);
  for (let i = 0; i < N * N; i++) {
    level[i * 4] = Math.floor(rnd() * 256);
    level[i * 4 + 1] = Math.floor(rnd() * 256);
    level[i * 4 + 2] = Math.round(b[i] * 255);
    level[i * 4 + 3] = Math.round(a[i] * 255);
  }
  const levels = [level];
  // Each smaller level averages 2×2 of the one above.
  for (let size = N; size > 1; size >>= 1) {
    const half = size >> 1;
    const next = new Uint8Array(half * half * 4);
    for (let y = 0; y < half; y++) {
      for (let x = 0; x < half; x++) {
        for (let c = 0; c < 4; c++) {
          const o = (2 * y * size + 2 * x) * 4 + c;
          next[(y * half + x) * 4 + c] = (level[o] + level[o + 4] + level[o + size * 4] + level[o + size * 4 + 4] + 2) >> 2;
        }
      }
    }
    levels.push(next);
    level = next;
  }
  return levels;
}

function noiseTexture(device: GPUDevice): GPUTexture {
  const levels = noiseLevels();
  const tex = device.createTexture({ label: 'flux material grain', size: [NOISE, NOISE], format: 'rgba8unorm', mipLevelCount: levels.length, usage: TEXTURE.TEXTURE_BINDING | TEXTURE.COPY_DST });
  levels.forEach((data, m) => {
    const size = NOISE >> m;
    device.queue.writeTexture({ texture: tex, mipLevel: m }, data, { bytesPerRow: size * 4 }, [size, size]);
  });
  return tex;
}

const shared = new WeakMap<GPUDevice, Shared3D>();
const preparing = new WeakMap<GPUDevice, Promise<boolean>>();

/** Compile the 3D pipeline once per device. False if it doesn't compile here (use the DOM lens). */
export function prepareScene3D(gpu: Gpu): Promise<boolean> {
  let p = preparing.get(gpu.device);
  if (!p) {
    p = (async () => {
      try {
        const device = gpu.device;
        const module = device.createShaderModule({ label: 'flux 3d quads', code: SCENE3D_WGSL });
        const pipeline = await device.createRenderPipelineAsync({
          label: 'flux 3d quads',
          layout: 'auto',
          vertex: { module, entryPoint: 'vs' },
          fragment: {
            module,
            entryPoint: 'fs',
            targets: [{ format: gpu.format, blend: { color: { srcFactor: 'one', dstFactor: 'one-minus-src-alpha', operation: 'add' }, alpha: { srcFactor: 'one', dstFactor: 'one-minus-src-alpha', operation: 'add' } } }],
          },
          primitive: { topology: 'triangle-strip' },
        });
        const sampler = device.createSampler({ magFilter: 'linear', minFilter: 'linear', mipmapFilter: 'linear', addressModeU: 'clamp-to-edge', addressModeV: 'clamp-to-edge', maxAnisotropy: 16 });
        const nsamp = device.createSampler({ magFilter: 'linear', minFilter: 'linear', mipmapFilter: 'linear', addressModeU: 'repeat', addressModeV: 'repeat' });
        shared.set(device, { pipeline, sampler, noise: noiseTexture(device).createView(), nsamp });
        return true;
      } catch (err) {
        console.error('flux: the 3D lens pipeline failed to compile; using the DOM lens', err);
        return false;
      }
    })();
    preparing.set(gpu.device, p);
  }
  return p;
}

export class Scene3DRenderer {
  readonly canvas: HTMLCanvasElement;
  private gpu: Gpu;
  private ctx: GPUCanvasContext;
  private uniforms: GPUBuffer;
  private storage: GPUBuffer | null = null;
  private capacity = 0;
  private bindGroup: GPUBindGroup | null = null;
  private bindKey = '';
  /** The batch whose quads are in `storage` (a redraw of the same scene uploads only the camera). */
  private stored: Quad3DBatch | null = null;
  private u = new Float32Array(CAM_FLOATS);

  constructor(gpu: Gpu, canvas: HTMLCanvasElement) {
    this.gpu = gpu;
    this.canvas = canvas;
    const ctx = canvas.getContext('webgpu') as GPUCanvasContext | null;
    if (!ctx) throw new Error('This canvas has no WebGPU context.');
    this.ctx = ctx;
    ctx.configure({ device: gpu.device, format: gpu.format, alphaMode: 'premultiplied' });
    this.uniforms = gpu.device.createBuffer({ label: 'flux camera', size: CAM_FLOATS * 4, usage: BUFFER.UNIFORM | BUFFER.COPY_DST });
  }

  resize(width: number, height: number): void {
    const w = Math.max(1, Math.round(width));
    const h = Math.max(1, Math.round(height));
    if (this.canvas.width !== w) this.canvas.width = w;
    if (this.canvas.height !== h) this.canvas.height = h;
  }

  /** `fresh`: false when the batch holds the same quads as last drawn (only the camera, the time or the lights moved). */
  draw(batch: Quad3DBatch, cam: Camera, tex: Textures3D, fresh = true): void {
    const s = shared.get(this.gpu.device);
    if (!s) throw new Error('prepareScene3D() first');
    const device = this.gpu.device;
    const n = batch.n;
    const bytes = Math.max(1, n) * QUAD3D_FLOATS * 4;
    if (!this.storage || bytes > this.capacity) {
      this.storage?.destroy();
      this.capacity = Math.max(bytes, this.capacity * 2, 64 * QUAD3D_FLOATS * 4);
      this.storage = device.createBuffer({ label: 'flux 3d quads', size: this.capacity, usage: BUFFER.STORAGE | BUFFER.COPY_DST });
      this.bindKey = '';
      this.stored = null;
    }
    if (n && (fresh || this.stored !== batch)) device.queue.writeBuffer(this.storage, 0, batch.data.buffer, batch.data.byteOffset, n * QUAD3D_FLOATS * 4);
    this.stored = batch;
    const u = this.u;
    u[0] = cam.width;
    u[1] = cam.height;
    u[2] = cam.originX;
    u[3] = cam.originY;
    u[4] = cam.persp;
    u[5] = cam.dpr;
    u[6] = cam.time;
    u.set(cam.light ?? NONE, 8);
    u.set(cam.glint ?? NONE, 12);
    u.set(cam.room ?? NONE, 16);
    device.queue.writeBuffer(this.uniforms, 0, u);
    if (!this.bindGroup || this.bindKey !== tex.key) {
      this.bindGroup = device.createBindGroup({
        label: 'flux 3d quads',
        layout: s.pipeline.getBindGroupLayout(0),
        entries: [
          { binding: 0, resource: { buffer: this.uniforms } },
          { binding: 1, resource: { buffer: this.storage } },
          { binding: 2, resource: s.sampler },
          { binding: 3, resource: tex.posters[0] },
          { binding: 4, resource: tex.posters[1] },
          { binding: 5, resource: tex.posters[2] },
          { binding: 6, resource: tex.posters[3] },
          { binding: 7, resource: tex.glyphs },
          { binding: 8, resource: tex.sprites },
          { binding: 9, resource: s.noise },
          { binding: 10, resource: s.nsamp },
        ],
      });
      this.bindKey = tex.key;
    }
    const encoder = device.createCommandEncoder({ label: 'flux 3d frame' });
    const pass = encoder.beginRenderPass({ colorAttachments: [{ view: this.ctx.getCurrentTexture().createView(), clearValue: { r: 0, g: 0, b: 0, a: 0 }, loadOp: 'clear', storeOp: 'store' }] });
    if (n) {
      pass.setPipeline(s.pipeline);
      pass.setBindGroup(0, this.bindGroup);
      pass.draw(4, n);
    }
    pass.end();
    device.queue.submit([encoder.finish()]);
  }

  destroy(): void {
    try {
      this.ctx.unconfigure();
    } catch {
      // already gone with the device
    }
    this.storage?.destroy();
    this.uniforms.destroy();
  }
}
