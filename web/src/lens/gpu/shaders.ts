/**
 * WGSL for GPU lenses. One instanced quad program draws everything a wall needs
 * (posters, rounded rectangles, text, rings, soft shadows, skeletons), so a frame
 * is a single draw call whatever the number of titles. Shapes are signed-distance
 * rounded rectangles, antialiased to the device pixel.
 *
 * Coordinates are CSS pixels in content space (the scroll offset is a uniform, so
 * scrolling changes one number); `start` → `rect` interpolates by `frame.t` for
 * zooms and reflows; a pinch scales everything about `frame.focal`.
 */

export const QUAD_WGSL = /* wgsl */ `
struct Frame {
  view: vec2f,
  scroll: vec2f,
  focal: vec2f,
  scale: f32,
  dpr: f32,
  time: f32,
  t: f32,
  _p0: f32,
  _p1: f32,
};

struct Quad {
  rect: vec4f,   // x, y, w, h (where it ends up)
  start: vec4f,  // where it starts when frame.t < 1 (w <= 0: same as rect)
  color: vec4f,  // straight-alpha colour (posters: placeholder; text: ink)
  uv: vec4f,     // texture rect (posters, text) or a second colour (gradients, skeletons)
  p: vec4f,      // radius, kind, texture (class * 4096 + layer), opacity
  q: vec4f,      // ready-at seconds (< 0: none), unused, extra (texture width / ring width / blur), flags
};

@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var<storage, read> quads: array<Quad>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var tex0: texture_2d_array<f32>;
@group(0) @binding(4) var tex1: texture_2d_array<f32>;
@group(0) @binding(5) var tex2: texture_2d_array<f32>;
@group(0) @binding(6) var tex3: texture_2d_array<f32>;
@group(0) @binding(7) var glyphs: texture_2d<f32>;

const K_SOLID: u32 = 0u;
const K_POSTER: u32 = 1u;
const K_TEXT: u32 = 2u;
const K_RING: u32 = 3u;
const K_SHADOW: u32 = 4u;
const K_SKELETON: u32 = 5u;

const F_GRAY: u32 = 1u;
const F_HAIRLINE: u32 = 2u;
const F_GRADIENT: u32 = 8u;

struct VOut {
  @builtin(position) pos: vec4f,
  @location(0) local: vec2f,
  @location(1) @interpolate(flat) half: vec2f,
  @location(2) @interpolate(flat) idx: u32,
  @location(3) @interpolate(flat) px: f32,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VOut {
  let q = quads[ii];
  let kind = u32(q.p.y);
  var r = q.rect;
  if (q.start.z > 0.0) {
    r = mix(q.start, q.rect, frame.t);
  }
  let scale = frame.scale;
  // One device pixel, in this quad's (unscaled) CSS pixels.
  let px = 1.0 / (frame.dpr * scale);
  var pad = px;
  if (kind == K_TEXT || kind == K_SHADOW) {
    pad = 0.0;
  }
  let half = r.zw * 0.5;
  let corner = vec2f(f32(vi & 1u), f32(vi >> 1u));
  var out: VOut;
  var s: vec2f;
  if (kind == K_TEXT) {
    // Text lands on whole device pixels, so glyphs stay crisp.
    let tl = frame.focal + (r.xy - frame.scroll - frame.focal) * scale;
    s = round(tl * frame.dpr) / frame.dpr + corner * r.zw * scale;
    out.local = (corner * 2.0 - 1.0) * half;
  } else {
    let local = (corner * 2.0 - 1.0) * (half + vec2f(pad));
    s = frame.focal + (r.xy + half + local - frame.scroll - frame.focal) * scale;
    out.local = local;
  }
  out.pos = vec4f(s.x / frame.view.x * 2.0 - 1.0, 1.0 - s.y / frame.view.y * 2.0, 0.0, 1.0);
  out.half = half;
  out.idx = ii;
  out.px = px;
  return out;
}

fn roundRect(p: vec2f, b: vec2f, r: f32) -> f32 {
  let d = abs(p) - b + vec2f(r);
  return length(max(d, vec2f(0.0))) + min(max(d.x, d.y), 0.0) - r;
}

fn poster(cls: u32, uv: vec2f, layer: i32, lod: f32) -> vec4f {
  switch cls {
    case 0u: { return textureSampleLevel(tex0, samp, uv, layer, lod); }
    case 1u: { return textureSampleLevel(tex1, samp, uv, layer, lod); }
    case 2u: { return textureSampleLevel(tex2, samp, uv, layer, lod); }
    default: { return textureSampleLevel(tex3, samp, uv, layer, lod); }
  }
}

@fragment
fn fs(in: VOut) -> @location(0) vec4f {
  let q = quads[in.idx];
  let kind = u32(q.p.y);
  let flags = u32(q.q.w);
  let radius = min(q.p.x, min(in.half.x, in.half.y));
  let px = in.px;
  var c = q.color;
  var cover = 1.0;
  let f = clamp(in.local / (2.0 * in.half) + 0.5, vec2f(0.0), vec2f(1.0));

  if (kind == K_TEXT) {
    let uv = mix(q.uv.xy, q.uv.zw, f);
    cover = textureSampleLevel(glyphs, samp, uv, 0.0).r;
  } else if (kind == K_SHADOW) {
    let blur = max(q.q.z, 0.5);
    let d = roundRect(in.local, max(in.half - vec2f(blur), vec2f(0.5)), radius);
    cover = 1.0 - smoothstep(-blur * 0.5, blur * 0.5, d);
  } else {
    let d = roundRect(in.local, in.half, radius);
    cover = clamp(0.5 - d / px, 0.0, 1.0);
    if (kind == K_RING) {
      cover = cover - clamp(0.5 - (d + q.q.z) / px, 0.0, 1.0);
    } else if (kind == K_SKELETON) {
      // A highlight sweeps across each placeholder, like the DOM's shimmer.
      let u = f.x + f.y * 0.18;
      let sweep = 1.6 - fract(frame.time / 1.6) * 2.2;
      let h = 1.0 - smoothstep(0.0, 0.32, abs(u - sweep));
      c = vec4f(mix(q.color.rgb, q.uv.rgb, h), q.color.a);
    } else if (kind == K_POSTER) {
      if ((flags & F_GRADIENT) != 0u) {
        // linear-gradient(160deg, c1, c2)
        let dir = vec2f(0.342, 0.940);
        let len = abs(in.half.x * dir.x) + abs(in.half.y * dir.y);
        let g = clamp(dot(in.local, dir) / (2.0 * len) + 0.5, 0.0, 1.0);
        c = mix(q.color, q.uv, g);
      }
      if (q.q.x >= 0.0) {
        let code = u32(q.p.z);
        let texW = q.q.z;
        let lod = max(log2(texW * px / (2.0 * in.half.x)), 0.0);
        var s = poster(code >> 12u, q.uv.xy + (q.uv.zw - q.uv.xy) * f, i32(code & 4095u), lod);
        if ((flags & F_GRAY) != 0u) {
          let g = dot(s.rgb, vec3f(0.2126, 0.7152, 0.0722)) * 0.6;
          s = vec4f(vec3f(g), s.a);
        }
        let fade = clamp((frame.time - q.q.x) / 0.28, 0.0, 1.0);
        c = mix(c, vec4f(s.rgb, 1.0), fade);
      }
      if ((flags & F_HAIRLINE) != 0u) {
        let edge = smoothstep(-1.0 - px, -1.0, d);
        c = vec4f(mix(c.rgb, vec3f(1.0), 0.07 * edge), c.a);
      }
    }
  }
  let a = c.a * cover * q.p.w;
  return vec4f(c.rgb * a, a);
}
`;

/** Downsamples one mip level into the next (a 2×2 box filter through the linear sampler). */
export const MIP_WGSL = /* wgsl */ `
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;

struct VO {
  @builtin(position) pos: vec4f,
  @location(0) uv: vec2f,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VO {
  let p = vec2f(f32((i << 1u) & 2u), f32(i & 2u));
  var o: VO;
  o.pos = vec4f(p * 2.0 - 1.0, 0.0, 1.0);
  o.uv = vec2f(p.x, 1.0 - p.y);
  return o;
}

@fragment
fn fs(v: VO) -> @location(0) vec4f {
  return textureSample(src, smp, v.uv);
}
`;
