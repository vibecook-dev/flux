import{n as e,t}from"./device-BqoaPgjv.js";var n=20,r=[0,0,0,0],i={solid:0,poster:1,text:2,ring:3,shadow:4,sprite:6},a={gray:1,hairline:2,openTop:4,gradient:8,mirror:16,openBottom:32},o={none:0,print:1,spine:2,plastic:3,wood:4,paint:5},s=`
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
`,c=class{data;n=0;constructor(e=256){this.data=new Float32Array(e*32)}reset(){this.n=0}add(e,t,n,r,i,a,o=0){if((this.n+1)*32>this.data.length){let e=new Float32Array(this.data.length*2);e.set(this.data),this.data=e}let s=this.n*32,c=this.data;return c.fill(0,s,s+32),c[s]=e,c[s+1]=t,c[s+2]=n,c[s+4]=r,c[s+5]=i,c[s+6]=o,c[s+7]=a,c[s+27]=1,c[s+26]=-1,this.n++,s}rotate(e,t,n,r=0){let i=this.data;i[e+8]=t,i[e+9]=n,i[e+10]=r}origin(e,t,n,r=0){let i=this.data;i[e+12]=t,i[e+13]=n,i[e+14]=r}color(e,t,n=1){let r=this.data;r[e+16]=t[0],r[e+17]=t[1],r[e+18]=t[2],r[e+19]=t[3]*n}uv(e,t,n,r,i){let a=this.data;a[e+20]=t,a[e+21]=n,a[e+22]=r,a[e+23]=i}texture(e,t,n,r){let i=this.data;i[e+24]=t,i[e+25]=n,i[e+26]=r}extra(e,t){this.data[e+25]=t}opacity(e,t){this.data[e+27]=t}flags(e,t){this.data[e+28]=t}mirror(e,t,n){let r=this.data;r[e+28]=r[e+28]|a.mirror,r[e+29]=t,r[e+30]=n}sheen(e,t){this.data[e+31]=t}material(e,t,n=0,r=0,i=0){let a=this.data;a[e+3]=t+Math.min(.999,Math.max(0,n)),a[e+11]=r,a[e+15]=i}};function l(e,t,n,r){if(r.persp<=0)return{x:e,y:t,scale:1};let i=(r.persp-n)/r.persp;return i<=1e-4?null:{x:r.originX+(e-r.originX)/i,y:r.originY+(t-r.originY)/i,scale:1/i}}function u(e,t){let n=[];for(let[r,i]of[[-1,-1],[1,-1],[-1,1],[1,1]]){let a=r*e.w/2-(e.ox??0),o=i*e.h/2-(e.oy??0),s=-(e.oz??0),c=e.rx??0,u=e.ry??0,d=e.rz??0;[o,s]=[Math.cos(c)*o-Math.sin(c)*s,Math.sin(c)*o+Math.cos(c)*s],[a,s]=[Math.cos(u)*a+Math.sin(u)*s,-Math.sin(u)*a+Math.cos(u)*s],[a,o]=[Math.cos(d)*a-Math.sin(d)*o,Math.sin(d)*a+Math.cos(d)*o],n.push(l(a+(e.ox??0)+e.x,o+(e.oy??0)+e.y,s+(e.oz??0)+e.z,t))}return n}function d(e,t,n){if(n.some(e=>!e))return!1;let[r,i,a,o]=n,s=[r,i,o,a],c=0;for(let n=0;n<4;n++){let r=s[n],i=s[(n+1)%4],a=(i.x-r.x)*(t-r.y)-(i.y-r.y)*(e-r.x);if(a===0)continue;let o=Math.sign(a);if(c===0)c=o;else if(o!==c)return!1}return!0}var f=128;function p(){let e=f,t=625341585,n=()=>(t^=t<<13,t^=t>>>17,t^=t<<5,(t>>>0)/4294967296),r=t=>{let r=e/t,i=Array.from({length:r*r},n),a=(e,t)=>i[t%r*r+e%r],o=new Float32Array(16384);for(let n=0;n<e;n++)for(let r=0;r<e;r++){let i=Math.floor(r/t),s=Math.floor(n/t),c=r/t-i,l=n/t-s;c=c*c*(3-2*c),l=l*l*(3-2*l);let u=a(i,s)+(a(i+1,s)-a(i,s))*c,d=a(i,s+1)+(a(i+1,s+1)-a(i,s+1))*c;o[n*e+r]=u+(d-u)*l}return o},i=r(4),a=r(16),o=new Uint8Array(65536);for(let e=0;e<16384;e++)o[e*4]=Math.floor(n()*256),o[e*4+1]=Math.floor(n()*256),o[e*4+2]=Math.round(i[e]*255),o[e*4+3]=Math.round(a[e]*255);let s=[o];for(let t=e;t>1;t>>=1){let e=t>>1,n=new Uint8Array(e*e*4);for(let r=0;r<e;r++)for(let i=0;i<e;i++)for(let a=0;a<4;a++){let s=(2*r*t+2*i)*4+a;n[(r*e+i)*4+a]=o[s]+o[s+4]+o[s+t*4]+o[s+t*4+4]+2>>2}s.push(n),o=n}return s}function m(t){let n=p(),r=t.createTexture({label:`flux material grain`,size:[f,f],format:`rgba8unorm`,mipLevelCount:n.length,usage:e.TEXTURE_BINDING|e.COPY_DST});return n.forEach((e,n)=>{let i=f>>n;t.queue.writeTexture({texture:r,mipLevel:n},e,{bytesPerRow:i*4},[i,i])}),r}var h=new WeakMap,g=new WeakMap;function _(e){let t=g.get(e.device);return t||(t=(async()=>{try{let t=e.device,n=t.createShaderModule({label:`flux 3d quads`,code:s}),r=await t.createRenderPipelineAsync({label:`flux 3d quads`,layout:`auto`,vertex:{module:n,entryPoint:`vs`},fragment:{module:n,entryPoint:`fs`,targets:[{format:e.format,blend:{color:{srcFactor:`one`,dstFactor:`one-minus-src-alpha`,operation:`add`},alpha:{srcFactor:`one`,dstFactor:`one-minus-src-alpha`,operation:`add`}}}]},primitive:{topology:`triangle-strip`}}),i=t.createSampler({magFilter:`linear`,minFilter:`linear`,mipmapFilter:`linear`,addressModeU:`clamp-to-edge`,addressModeV:`clamp-to-edge`,maxAnisotropy:16}),a=t.createSampler({magFilter:`linear`,minFilter:`linear`,mipmapFilter:`linear`,addressModeU:`repeat`,addressModeV:`repeat`});return h.set(t,{pipeline:r,sampler:i,noise:m(t).createView(),nsamp:a}),!0}catch(e){return console.error(`flux: the 3D lens pipeline failed to compile; using the DOM lens`,e),!1}})(),g.set(e.device,t)),t}var v=class{canvas;gpu;ctx;uniforms;storage=null;capacity=0;bindGroup=null;bindKey=``;stored=null;u=new Float32Array(n);constructor(e,n){this.gpu=e,this.canvas=n;let r=n.getContext(`webgpu`);if(!r)throw Error(`This canvas has no WebGPU context.`);this.ctx=r,r.configure({device:e.device,format:e.format,alphaMode:`premultiplied`}),this.uniforms=e.device.createBuffer({label:`flux camera`,size:80,usage:t.UNIFORM|t.COPY_DST})}resize(e,t){let n=Math.max(1,Math.round(e)),r=Math.max(1,Math.round(t));this.canvas.width!==n&&(this.canvas.width=n),this.canvas.height!==r&&(this.canvas.height=r)}draw(e,n,i,a=!0){let o=h.get(this.gpu.device);if(!o)throw Error(`prepareScene3D() first`);let s=this.gpu.device,c=e.n,l=Math.max(1,c)*32*4;(!this.storage||l>this.capacity)&&(this.storage?.destroy(),this.capacity=Math.max(l,this.capacity*2,8192),this.storage=s.createBuffer({label:`flux 3d quads`,size:this.capacity,usage:t.STORAGE|t.COPY_DST}),this.bindKey=``,this.stored=null),c&&(a||this.stored!==e)&&s.queue.writeBuffer(this.storage,0,e.data.buffer,e.data.byteOffset,c*32*4),this.stored=e;let u=this.u;u[0]=n.width,u[1]=n.height,u[2]=n.originX,u[3]=n.originY,u[4]=n.persp,u[5]=n.dpr,u[6]=n.time,u.set(n.light??r,8),u.set(n.glint??r,12),u.set(n.room??r,16),s.queue.writeBuffer(this.uniforms,0,u),(!this.bindGroup||this.bindKey!==i.key)&&(this.bindGroup=s.createBindGroup({label:`flux 3d quads`,layout:o.pipeline.getBindGroupLayout(0),entries:[{binding:0,resource:{buffer:this.uniforms}},{binding:1,resource:{buffer:this.storage}},{binding:2,resource:o.sampler},{binding:3,resource:i.posters[0]},{binding:4,resource:i.posters[1]},{binding:5,resource:i.posters[2]},{binding:6,resource:i.posters[3]},{binding:7,resource:i.glyphs},{binding:8,resource:i.sprites},{binding:9,resource:o.noise},{binding:10,resource:o.nsamp}]}),this.bindKey=i.key);let d=s.createCommandEncoder({label:`flux 3d frame`}),f=d.beginRenderPass({colorAttachments:[{view:this.ctx.getCurrentTexture().createView(),clearValue:{r:0,g:0,b:0,a:0},loadOp:`clear`,storeOp:`store`}]});c&&(f.setPipeline(o.pipeline),f.setBindGroup(0,this.bindGroup),f.draw(4,c)),f.end(),s.queue.submit([d.finish()])}destroy(){try{this.ctx.unconfigure()}catch{}this.storage?.destroy(),this.uniforms.destroy()}};export{v as a,_ as c,c as i,i as n,u as o,o as r,d as s,a as t};