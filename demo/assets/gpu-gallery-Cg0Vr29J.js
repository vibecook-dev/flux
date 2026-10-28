const __vite__mapDeps=(i,m=__vite__mapDeps,d=(m.f||(m.f=["assets/gallery-DEW-QyPH.js","assets/format-3t8CxBvf.js","assets/dom-D3HwYDBK.js","assets/art-BbkGjp3T.js","assets/client-CC7PKe2z.js","assets/blurhash-HbIpDx78.js","assets/palette-BhehA8KE.js","assets/index-Df3ERQn8.js","assets/react-DHsaPJjs.js","assets/index-DWaKvGH1.css","assets/art-BcRdyiWV.css","assets/core-C50rAEzA.js","assets/core-CogDYhmk.css"])))=>i.map(i=>d[i]);
import{n as e,r as t}from"./dom-D3HwYDBK.js";import{s as n}from"./palette-BhehA8KE.js";import{B as r,H as i,V as a,W as o,Y as s,p as c}from"./index-Df3ERQn8.js";import{n as l}from"./art-BbkGjp3T.js";import{n as u,t as d}from"./device-BqoaPgjv.js";import{a as f,c as p,h as m,i as h,l as g,n as _,o as v,r as y,s as b,t as ee}from"./colors-BZ4uD4aM.js";import{i as te,s as x}from"./scene3d-BREWxzBl.js";import{a as S,c as C,i as w,l as ne,n as T,o as E,r as re,s as ie,t as D,u as O}from"./core-C50rAEzA.js";var k={solid:0,poster:1,text:2,ring:3,shadow:4,linear:7,radial:8,floor:9},A={gray:1,hairline:2,gradient:8,mirror:16,noaa:64,inset:128,cut:256},ae=16,j=`
struct Cam {
  view: vec2f,     // canvas, CSS px
  center: vec2f,   // perspective origin, CSS px
  eye: vec4f,      // added to world points before the turn: (-x, -y, d)
  persp: f32,
  yaw: f32,        // radians, CSS rotateY
  near: f32,       // view distance of the near plane, CSS px
  time: f32,
  dpr: f32,
  _a: f32,
  _b: f32,
  _c: f32,
};

struct Q {
  pos: vec4f,     // centre x, y, z (world)
  size: vec4f,    // width, height, corner radius, kind
  rot: vec4f,     // rotateX, rotateY, rotateZ (radians; X applied first, as CSS lists them right to left)
  origin: vec4f,  // transform origin, relative to the centre
  color: vec4f,   // straight-alpha colour, or a tint (gradients)
  uv: vec4f,      // texture rect; a second colour (poster placeholders); gradient geometry
  tex: vec4f,     // poster code or gradient offset, texture width or ring width / blur, ready-at (< 0: none), opacity
  fx: vec4f,      // flags, mirror line y, mirror strength, brightness (0 = as is)
};

@group(0) @binding(0) var<uniform> cam: Cam;
@group(0) @binding(1) var<storage, read> quads: array<Q>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var tex0: texture_2d_array<f32>;
@group(0) @binding(4) var tex1: texture_2d_array<f32>;
@group(0) @binding(5) var tex2: texture_2d_array<f32>;
@group(0) @binding(6) var tex3: texture_2d_array<f32>;
@group(0) @binding(7) var glyphs: texture_2d<f32>;
// Gradients: at each offset a header (stop count), then per stop (position) and (premultiplied colour).
@group(0) @binding(8) var<storage, read> grads: array<vec4f>;

const K_SOLID: u32 = 0u;
const K_POSTER: u32 = 1u;
const K_TEXT: u32 = 2u;
const K_RING: u32 = 3u;
const K_SHADOW: u32 = 4u;
const K_LINEAR: u32 = 7u;
const K_RADIAL: u32 = 8u;
const K_FLOOR: u32 = 9u;

const F_GRAY: u32 = 1u;
const F_HAIRLINE: u32 = 2u;
const F_GRADIENT: u32 = 8u;
const F_MIRROR: u32 = 16u;
const F_NOAA: u32 = 64u;
const F_INSET: u32 = 128u;
const F_CUT: u32 = 256u;

struct VOut {
  @builtin(position) pos: vec4f,
  @location(0) local: vec2f,
  @location(1) @interpolate(flat) half: vec2f,
  @location(2) @interpolate(flat) idx: u32,
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

/** A quad-local point in world space (CSS: translate, rotate about the origin). */
fn world(q: Q, local: vec2f, flags: u32) -> vec3f {
  var p = vec3f(local, 0.0) - q.origin.xyz;
  p = rotZ(rotY(rotX(p, q.rot.x), q.rot.y), q.rot.z);
  var v = p + q.origin.xyz + q.pos.xyz;
  if ((flags & F_MIRROR) != 0u) {
    v.y = 2.0 * q.fx.y - v.y;
  }
  return v;
}

/** World to the camera's space: the eye at z = persp, looking down -z. */
fn eyeSpace(v: vec3f) -> vec3f {
  var e = rotY(v + cam.eye.xyz, cam.yaw);
  e.z = e.z + cam.persp;
  return e;
}

@vertex
fn vs(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VOut {
  let q = quads[ii];
  let kind = u32(q.size.w);
  let flags = u32(q.fx.x);
  let half = q.size.xy * 0.5;
  let corner = vec2f(f32(vi & 1u), f32(vi >> 1u)) * 2.0 - 1.0;
  // A margin so edges can antialias: two device pixels at this corner's distance
  // (a far quad needs a wider margin in world units than a near one).
  var pad = 0.0;
  if (kind != K_TEXT && kind != K_SHADOW && (flags & F_NOAA) == 0u) {
    let e0 = eyeSpace(world(q, corner * half, flags));
    pad = 2.0 * max(1.0, (cam.persp - e0.z) / cam.persp / max(cam.dpr, 0.5));
  }
  let local = corner * (half + vec2f(pad));
  let e = eyeSpace(world(q, local, flags));
  // CSS perspective, unclamped: w = distance / persp is negative behind the eye, and
  // the hardware clips at the near plane. Depth = near / distance (reversed, infinite far).
  let w = (cam.persp - e.z) / cam.persp;
  let sx = cam.center.x * w + e.x;
  let sy = cam.center.y * w + e.y;
  var out: VOut;
  out.pos = vec4f(sx / cam.view.x * 2.0 - w, w - sy / cam.view.y * 2.0, cam.near / cam.persp, w);
  out.local = local;
  out.half = half;
  out.idx = ii;
  return out;
}

fn roundRect(p: vec2f, b: vec2f, r: f32) -> f32 {
  let d = abs(p) - b + vec2f(r);
  return length(max(d, vec2f(0.0))) + min(max(d.x, d.y), 0.0) - r;
}

fn poster(cls: u32, uv: vec2f, layer: i32, gx: vec2f, gy: vec2f) -> vec4f {
  switch cls {
    case 0u: { return textureSampleGrad(tex0, samp, uv, layer, gx, gy); }
    case 1u: { return textureSampleGrad(tex1, samp, uv, layer, gx, gy); }
    case 2u: { return textureSampleGrad(tex2, samp, uv, layer, gx, gy); }
    default: { return textureSampleGrad(tex3, samp, uv, layer, gx, gy); }
  }
}

/** The premultiplied colour of gradient \`base\` at \`t\` (CSS interpolates premultiplied). */
fn grad(base: u32, t: f32) -> vec4f {
  let n = u32(grads[base].x);
  var pt = grads[base + 1u].x;
  var pc = grads[base + 2u];
  if (t <= pt) {
    return pc;
  }
  for (var i = 1u; i < n; i++) {
    let st = grads[base + 1u + 2u * i].x;
    let sc = grads[base + 2u + 2u * i];
    if (t <= st) {
      return mix(pc, sc, clamp((t - pt) / max(st - pt, 0.00001), 0.0, 1.0));
    }
    pt = st;
    pc = sc;
  }
  return pc;
}

/** CSS linear-gradient(angle): 0 = to top, clockwise; the line spans the box's corners. */
fn linearT(local: vec2f, half: vec2f, angle: f32) -> f32 {
  let dir = vec2f(sin(angle), -cos(angle));
  let len = abs(2.0 * half.x * dir.x) + abs(2.0 * half.y * dir.y);
  return dot(local, dir) / max(len, 0.0001) + 0.5;
}

/** Coverage of the stripes [per - lw, per) of every period along u, box-filtered over [u0, u1]. */
fn stripesIn(u0: f32, u1: f32, per: f32, lw: f32) -> f32 {
  let a = floor(u0 / per) * lw + clamp(u0 - floor(u0 / per) * per - (per - lw), 0.0, lw);
  let b = floor(u1 / per) * lw + clamp(u1 - floor(u1 / per) * per - (per - lw), 0.0, lw);
  return (b - a) / max(u1 - u0, 0.0001);
}

fn tinted(pm: vec4f, tint: vec4f) -> vec4f {
  return vec4f(pm.rgb * tint.rgb, pm.a) * tint.a;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4f {
  // Screen-space rates first (derivatives need uniform control flow).
  let q = quads[in.idx];
  let radius = min(q.size.z, min(in.half.x, in.half.y));
  let d0 = roundRect(in.local, in.half, radius);
  let aa = max(fwidth(d0), 0.0001);
  let gx = dpdx(in.local) / (2.0 * in.half);
  let gy = dpdy(in.local) / (2.0 * in.half);
  let fu = max(fwidth(in.local.x), 0.0001);
  // An opening cut out of a face (a doorway): |x| < uv.y below y = uv.z, open at the bottom.
  // One quad with a hole has no inner seams; two that meet would each antialias the joint.
  let dOpen = max(abs(in.local.x) - q.uv.y, q.uv.z - in.local.y);
  let aOpen = max(fwidth(dOpen), 0.0001);
  let kind = u32(q.size.w);
  let flags = u32(q.fx.x);
  let f = clamp(in.local / (2.0 * in.half) + 0.5, vec2f(0.0), vec2f(1.0));
  var col = vec4f(0.0);
  if (kind == K_TEXT) {
    let cover = textureSampleLevel(glyphs, samp, mix(q.uv.xy, q.uv.zw, f), 0.0).r;
    col = vec4f(q.color.rgb, 1.0) * q.color.a * cover;
  } else if (kind == K_SHADOW) {
    let blur = max(q.tex.y, 0.5);
    var cover = 0.0;
    if ((flags & F_INSET) != 0u) {
      // An inner shadow: darkest at the rect's edge, gone \`blur\` inside it.
      cover = smoothstep(-blur, 0.0, d0) * clamp(0.5 - d0 / aa, 0.0, 1.0);
    } else {
      let d = roundRect(in.local, max(in.half - vec2f(blur), vec2f(0.5)), radius);
      cover = 1.0 - smoothstep(-blur * 0.5, blur * 0.5, d);
    }
    col = vec4f(q.color.rgb, 1.0) * q.color.a * cover;
  } else {
    var cover = 1.0;
    if ((flags & F_NOAA) != 0u) {
      cover = select(1.0, 0.0, d0 > 0.0);
    } else {
      cover = clamp(0.5 - d0 / aa, 0.0, 1.0);
    }
    if ((flags & F_CUT) != 0u) {
      cover = cover * clamp(0.5 + dOpen / aOpen, 0.0, 1.0);
    }
    if (kind == K_RING) {
      cover = cover - clamp(0.5 - (d0 + q.tex.y) / aa, 0.0, 1.0);
      col = vec4f(q.color.rgb, 1.0) * q.color.a * cover;
    } else if (kind == K_LINEAR) {
      col = tinted(grad(u32(q.tex.x), linearT(in.local, in.half, q.uv.x)), q.color) * cover;
    } else if (kind == K_RADIAL) {
      let t = length((f - q.uv.xy) / max(q.uv.zw, vec2f(0.0001)));
      col = tinted(grad(u32(q.tex.x), t), q.color) * cover;
    } else if (kind == K_FLOOR) {
      // The boards: a stripe every uv.x along the floor's width, box-filtered over the
      // pixel's footprint, so they never shimmer into moire at a grazing angle.
      let u = in.local.x + in.half.x;
      let lines = stripesIn(u - fu * 0.5, u + fu * 0.5, q.uv.x, q.uv.y);
      let base = vec4f(q.color.rgb, 1.0) * q.color.a;
      let board = grad(u32(q.tex.y), 0.0);
      let boards = board * lines + base * (1.0 - board.a * lines);
      let sheen = grad(u32(q.tex.x), f.x);
      col = (sheen + boards * (1.0 - sheen.a)) * cover;
    } else if (kind == K_POSTER) {
      var c = q.color;
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
      col = vec4f(c.rgb, 1.0) * c.a * cover;
    } else {
      col = vec4f(q.color.rgb, 1.0) * q.color.a * cover;
    }
    if ((flags & F_HAIRLINE) != 0u) {
      // CSS \`inset 0 0 0 1px rgb(255 255 255 / 0.07)\`.
      let edge = smoothstep(-1.0 - aa, -1.0, d0);
      col = vec4f(mix(col.rgb, vec3f(col.a), 0.07 * edge), col.a);
    }
  }
  col = vec4f(col.rgb * (1.0 + q.fx.w), col.a);
  var a = q.tex.w;
  if ((flags & F_MIRROR) != 0u) {
    a = a * q.fx.z;
  }
  // Nothing here (a cut-out, the clear part of a gradient or a glyph): leave no depth
  // behind, or what is drawn later through it would fail the depth test.
  if (col.a * a <= 0.002) {
    discard;
  }
  return col * a;
}
`;function M(e,t,n,r){let i=e+r.eyeX,a=t+r.eyeY,o=n+r.eyeZ,s=Math.cos(r.yaw),c=Math.sin(r.yaw);return[s*i+c*o,a,-c*i+s*o+r.persp]}function N(e,t,n,r){let[i,a,o]=M(e,t,n,r),s=r.persp-o;if(s<r.near)return null;let c=s/r.persp;return{x:r.centerX+i/c,y:r.centerY+a/c,scale:1/c,dist:s}}function P(e,t){let n=[];for(let[r,i]of[[-1,-1],[1,-1],[-1,1],[1,1]]){let a=r*e.w/2,o=i*e.h/2,s=0,c=e.rx??0,l=e.ry??0,u=e.rz??0;[o,s]=[Math.cos(c)*o-Math.sin(c)*s,Math.sin(c)*o+Math.cos(c)*s],[a,s]=[Math.cos(l)*a+Math.sin(l)*s,-Math.sin(l)*a+Math.cos(l)*s],[a,o]=[Math.cos(u)*a-Math.sin(u)*o,Math.sin(u)*a+Math.cos(u)*o],n.push(N(a+e.x,o+e.y,s+e.z,t))}return n}function F(e){let t=[],n=0;for(let t of e)n+=1+2*t.length;let r=new Float32Array(Math.max(4,n*4)),i=0;for(let n of e){t.push(i),r[i*4]=n.length,i++;for(let[e,t]of n)r[i*4]=e,i++,r.set([t[0]*t[3],t[1]*t[3],t[2]*t[3],t[3]],i*4),i++}return{data:r,offsets:t}}var I=new WeakMap,L=new WeakMap,R=`depth32float`;function z(e){let t=L.get(e.device);return t||(t=(async()=>{try{let t=e.device,n=t.createShaderModule({label:`flux rooms`,code:j}),r=t.createBindGroupLayout({label:`flux rooms`,entries:[{binding:0,visibility:3,buffer:{type:`uniform`}},{binding:1,visibility:3,buffer:{type:`read-only-storage`}},{binding:2,visibility:2,sampler:{type:`filtering`}},{binding:3,visibility:2,texture:{sampleType:`float`,viewDimension:`2d-array`}},{binding:4,visibility:2,texture:{sampleType:`float`,viewDimension:`2d-array`}},{binding:5,visibility:2,texture:{sampleType:`float`,viewDimension:`2d-array`}},{binding:6,visibility:2,texture:{sampleType:`float`,viewDimension:`2d-array`}},{binding:7,visibility:2,texture:{sampleType:`float`,viewDimension:`2d`}},{binding:8,visibility:2,buffer:{type:`read-only-storage`}}]}),i=t.createPipelineLayout({label:`flux rooms`,bindGroupLayouts:[r]}),a={color:{srcFactor:`one`,dstFactor:`one-minus-src-alpha`,operation:`add`},alpha:{srcFactor:`one`,dstFactor:`one-minus-src-alpha`,operation:`add`}},o=(r,o,s)=>t.createRenderPipelineAsync({label:`flux rooms: ${r}`,layout:i,vertex:{module:n,entryPoint:`vs`},fragment:{module:n,entryPoint:`fs`,targets:[{format:e.format,blend:a}]},primitive:{topology:`triangle-strip`},depthStencil:{format:R,depthWriteEnabled:o,depthCompare:s}}),[s,c,l]=await Promise.all([o(`reflections`,!1,`always`),o(`architecture`,!0,`greater`),o(`decals`,!1,`greater`)]),u=t.createSampler({magFilter:`linear`,minFilter:`linear`,mipmapFilter:`linear`,addressModeU:`clamp-to-edge`,addressModeV:`clamp-to-edge`,maxAnisotropy:16});return I.set(t,{layout:r,reflect:s,opaque:c,decal:l,sampler:u}),!0}catch(e){return console.error(`flux: the room pipelines failed to compile; using the DOM lens`,e),!1}})(),L.set(e.device,t)),t}var B=class{canvas;gpu;ctx;uniforms;storage=null;capacity=0;gradients=null;gradientBytes=0;depth=null;bindGroup=null;bindKey=``;u=new Float32Array(ae);constructor(e,t){this.gpu=e,this.canvas=t;let n=t.getContext(`webgpu`);if(!n)throw Error(`This canvas has no WebGPU context.`);this.ctx=n,n.configure({device:e.device,format:e.format,alphaMode:`premultiplied`}),this.uniforms=e.device.createBuffer({label:`flux room camera`,size:64,usage:d.UNIFORM|d.COPY_DST})}resize(e,t){let n=Math.max(1,Math.round(e)),r=Math.max(1,Math.round(t));this.canvas.width!==n&&(this.canvas.width=n),this.canvas.height!==r&&(this.canvas.height=r)}setGradients(e){let t=this.gpu.device;(!this.gradients||e.byteLength>this.gradientBytes)&&(this.gradients?.destroy(),this.gradientBytes=Math.max(256,e.byteLength),this.gradients=t.createBuffer({label:`flux room gradients`,size:this.gradientBytes,usage:d.STORAGE|d.COPY_DST}),this.bindKey=``),t.queue.writeBuffer(this.gradients,0,e.buffer,e.byteOffset,e.byteLength)}depthView(){let{width:e,height:t}=this.canvas;return(!this.depth||this.depth.width!==e||this.depth.height!==t)&&(this.depth?.destroy(),this.depth=this.gpu.device.createTexture({label:`flux room depth`,size:[e,t],format:R,usage:u.RENDER_ATTACHMENT})),this.depth.createView()}draw(e,t,n,r,i=!0){let a=I.get(this.gpu.device);if(!a)throw Error(`prepareRoom3D() first`);this.gradients||this.setGradients(new Float32Array(4));let o=this.gpu.device,s=e.n,c=Math.max(1,s)*32*4;(!this.storage||c>this.capacity)&&(this.storage?.destroy(),this.capacity=Math.max(c,this.capacity*2,32768),this.storage=o.createBuffer({label:`flux room quads`,size:this.capacity,usage:d.STORAGE|d.COPY_DST}),this.bindKey=``,i=!0),s&&i&&o.queue.writeBuffer(this.storage,0,e.data.buffer,e.data.byteOffset,s*32*4);let l=this.u;l[0]=n.width,l[1]=n.height,l[2]=n.centerX,l[3]=n.centerY,l[4]=n.eyeX,l[5]=n.eyeY,l[6]=n.eyeZ,l[7]=0,l[8]=n.persp,l[9]=n.yaw,l[10]=n.near,l[11]=n.time,l[12]=n.dpr,o.queue.writeBuffer(this.uniforms,0,l),(!this.bindGroup||this.bindKey!==r.key)&&(this.bindGroup=o.createBindGroup({label:`flux rooms`,layout:a.layout,entries:[{binding:0,resource:{buffer:this.uniforms}},{binding:1,resource:{buffer:this.storage}},{binding:2,resource:a.sampler},{binding:3,resource:r.posters[0]},{binding:4,resource:r.posters[1]},{binding:5,resource:r.posters[2]},{binding:6,resource:r.posters[3]},{binding:7,resource:r.glyphs},{binding:8,resource:{buffer:this.gradients}}]}),this.bindKey=r.key);let u=o.createCommandEncoder({label:`flux room frame`}),f=u.beginRenderPass({colorAttachments:[{view:this.ctx.getCurrentTexture().createView(),clearValue:{r:0,g:0,b:0,a:0},loadOp:`clear`,storeOp:`store`}],depthStencilAttachment:{view:this.depthView(),depthClearValue:0,depthLoadOp:`clear`,depthStoreOp:`discard`}});f.setBindGroup(0,this.bindGroup);let p=0;for(let[e,n]of[[a.reflect,t.reflections],[a.opaque,t.architecture],[a.decal,t.decals]])n>0&&(f.setPipeline(e),f.draw(4,n,0,p)),p+=n;f.end(),o.queue.submit([u.finish()])}destroy(){try{this.ctx.unconfigure()}catch{}this.storage?.destroy(),this.gradients?.destroy(),this.depth?.destroy(),this.uniforms.destroy()}},V=Math.PI/180,H=8,U=D+2*S,W=[.13,.125,.12,1],G={spot:1,shadow:1.5,frame:2,label:2,mat:2.5,matShade:2.6,bevelLine:2.7,bevel:2.8,art:3,marks:3.2,glaze:3.6,face:.75,door:1},K=640,oe=Math.min(G.spot,G.face),q=[1,1,1,1],J=[0,0,0,0],Y=[.914,.902,.875,1],X=[8/255,8/255,10/255,1],se=[0,0,0,1],Z=[.35,.5,.71,1,1.41,2,2.83,4];function Q(e,t=0){if(t&&e<=t*1.15&&e>=t*.55)return t;for(let t of Z)if(t>=e*.95)return t;return Z[Z.length-1]}function $(e,t,n,r,i,a,o,s){let c=r+a/2,l=e<0?t+c:t+n-c;return{x:e<0?-S/2+s:S/2-s,y:i+o/2,z:-l,w:a,h:o,ry:e<0?Math.PI/2:-Math.PI/2}}var ce=class extends re{canvas;renderer;posters;text;batch=new te(1024);passes={reflections:0,architecture:0,decals:0};theme;grad={};gradKey=``;tint=`#3a3836`;dpr=1;paintRaf=0;lastPaint=0;lastTrim=0;gone=!1;dom=null;hidden=null;hover=-1;spot=new Map;lods=new Map;layouts=new Map;layoutGen=-1;opts=new Map;eye=[0,0,0];needBuild=!0;builtFor=0;spotsMoving=!1;wraps=new Map;wrapGen=-1;layoutCache=new Map;layoutPortrait=null;labelGlyphs=new Map;constructor(e,n,r){super(e,n),this.canvas=t(`canvas`,O.canvas,{"aria-hidden":`true`}),e.append(this.canvas),this.renderer=new B(r,this.canvas),this.posters=g(r),this.text=v(r),this.theme=this.readTheme(),this.cleanup.push(this.posters.retain(),this.posters.onReady(this.dirty),this.text.onChange(this.dirty),r.onLost(()=>this.fallBack())),this.watchTheme(),this.mini||this.on(`pointermove`,e=>this.pointer(e)),this.start()}readTheme(){let e=getComputedStyle(this.root),t=t=>e.getPropertyValue(t),n=(e,n)=>ee(t(e),n),r={wall:n(`--gal-wall`,[.125,.118,.106,1]),hi:n(`--gal-wall-hi`,[.16,.15,.137,1]),lo:n(`--gal-wall-lo`,[.082,.078,.071,1]),skirt:n(`--gal-skirt`,[.035,.031,.027,1]),board:n(`--gal-board`,[1,1,1,.028]),sheen:n(`--gal-sheen`,[1,.91,.78,.07]),ceil:n(`--gal-ceil`,[.047,.043,.043,1]),ceilLight:n(`--gal-ceil-light`,[1,.925,.82,.2]),fixture:n(`--gal-fixture`,[1,.93,.84,.95]),fixtureGlow:n(`--gal-fixture-glow`,[1,.886,.73,.12]),spot:n(`--gal-spot`,[1,.878,.714,.3]),spot2:n(`--gal-spot-2`,[1,.878,.714,.07]),frameA:n(`--gal-frame-a`,[.118,.118,.118,1]),frameB:n(`--gal-frame-b`,[.024,.024,.024,1]),jamb:n(`--gal-jamb`,[.149,.141,.129,1]),jambSide:n(`--gal-jamb-side`,[.078,.075,.071,1]),door:n(`--gal-door`,[1,.953,.878,1])};return{floor:n(`--gal-floor`,[.075,.071,.063,.9]),mat:n(`--gal-mat`,[.94,.93,.9,1]),shadow:n(`--gal-shadow`,[0,0,0,.78]),bevel:n(`--gal-bevel`,[.984,.98,.965,1]),ink:n(`--gal-ink`,[.93,.906,.867,1]),ink2:n(`--gal-ink-2`,[.93,.906,.867,.66]),ink3:n(`--gal-ink-3`,[.93,.906,.867,.42]),away:n(`--gal-away`,[1,.54,.46,1]),jamb:r.jamb,jambSide:r.jambSide,door:r.door,doorGlow:n(`--gal-door-glow`,[1,.894,.745,.32]),body:t(`--font-body`).trim()||`system-ui, sans-serif`,mono:t(`--font-mono`).trim()||`ui-monospace, monospace`,stops:r}}gradients(){let e=`${this.tint}`;if(e===this.gradKey&&this.grad.wall!==void 0)return;this.gradKey=e;let t=this.theme.stops,n=y(this.tint.startsWith(`#`)?this.tint:`#3a3836`),r=[[`wall`,[[0,t.lo],[.16,t.wall],[.44,t.hi],[.78,t.wall],[.972,t.lo],[.972,t.skirt],[1,t.skirt]]],[`ceil`,[[0,t.ceil],[.5,h(t.ceil,t.ceilLight,.18)],[1,t.ceil]]],[`stripe`,[[0,J],[.393,t.ceilLight],[.607,t.ceilLight],[1,J]]],[`sheen`,[[.08,J],[.5,t.sheen],[.92,J]]],[`board`,[[0,t.board]]],[`pool`,[[0,t.spot],[.55,t.spot2],[.82,J]]],[`poolTop`,[[0,h(t.spot,q,.3)],[1,J]]],[`fixture`,[[0,t.fixture],[1,J]]],[`fixtureGlow`,[[0,t.fixtureGlow],[1,J]]],[`frame`,[[0,t.frameA],[.6,t.frameB]]],[`glaze`,[[.3,J],[.41,[1,1,1,.1]],[.52,[1,1,1,.02]],[.6,J]]],[`jamb`,[[0,t.jamb],[.972,t.jamb],[.972,t.skirt],[1,t.skirt]]],[`jambSide`,[[0,t.jambSide],[.972,t.jambSide],[.972,t.skirt],[1,t.skirt]]],[`end`,[[0,t.lo],[.3,t.wall],[.972,t.wall],[.972,t.skirt],[1,t.skirt]]],[`door`,[[0,t.door],[1,h(t.door,n,.12)]]]],i=F(r.map(([,e])=>e));r.forEach(([e],t)=>this.grad[e]=i.offsets[t]),this.renderer.setGradients(i.data)}watchTheme(){let e=()=>{this.theme=this.readTheme(),this.gradKey=``,this.dirty()},t=new MutationObserver(e);t.observe(document.documentElement,{attributes:!0,attributeFilter:[`data-theme`,`class`]});let n=window.matchMedia(`(prefers-color-scheme: dark)`);n.addEventListener(`change`,e),this.cleanup.push(()=>t.disconnect(),()=>n.removeEventListener(`change`,e))}surface(){return this.canvas}resized(e){this.dpr=this.pixelRatio(),this.renderer.resize(this.vw*this.dpr,this.vh*this.dpr),e&&this.layouts.clear()}pixelRatio(){let e=Math.min(window.devicePixelRatio||1,3);if(this.mini){let t=this.root.getBoundingClientRect().width;e=Math.min(e,Math.max(.5,e*t/Math.max(1,this.vw))*1.25)}return e}render(){this.ticking?this.paint():this.invalidate()}showChanged(){this.labelGlyphs.clear(),this.dirty(),this.moving||this.syncOptions()}focused(e){let t=this.works[e];t&&(this.tint=n(t.item).c1),this.dirty()}settled(){this.syncOptions()}dirty=()=>{this.needBuild=!0,this.invalidate()};invalidate=()=>{this.paintRaf||this.gone||this.dom||(this.paintRaf=requestAnimationFrame(()=>{this.paintRaf=0,this.moving||this.paint()}))};camera(e){let t=this.pose;return{width:this.vw,height:this.vh,centerX:this.vw/2,centerY:this.vh/2,persp:this.P,yaw:t.yaw*V,eyeX:-t.x,eyeY:-t.y,eyeZ:t.d,near:8,dpr:this.dpr,time:e}}paint(){if(this.gone||this.dom)return;Math.abs(this.pixelRatio()-this.dpr)>.01&&(this.resized(!1),this.needBuild=!0);let e=m(),t=this.lastPaint?Math.min(.1,e-this.lastPaint):1/60;this.lastPaint=e;let n=this.posters;n.upload(),this.gradients();let r=this.signature(),i=this.needBuild||this.spotsMoving||r!==this.builtFor;if(i){this.needBuild=!1,this.builtFor=r,n.begin();let e=this.text.gen;this.build(t),this.text.gen!==e&&this.build(0),this.text.flush()}this.renderer.draw(this.batch,this.passes,this.camera(e),{posters:n.views(),glyphs:this.text.view,key:String(n.version)},i),n.pump(),performance.now()-this.lastTrim>5e3&&(this.lastTrim=performance.now(),n.trim()),(this.spotsMoving||n.pending||e<n.fadeUntil)&&!this.paused?this.invalidate():this.lastPaint=0}layoutOf(e){(this.layoutPortrait!==this.portrait||this.layoutCache.size>3e3)&&(this.layoutCache.clear(),this.labelGlyphs.clear(),this.layoutPortrait=this.portrait);let t=this.layoutCache.get(e);return t||(t=E(e.item,this.portrait===!0),this.layoutCache.set(e,t)),t}onPieceDist(e,t,n,r){let i=e.side<0?e.d-390+t:e.d+390-t,a=e.side<0?-S/2+r:S/2-r;return Math.max(this.dist(a,n,-i),8)}posterClass(e,t,n){let r=p(t.pw*this.P*this.dpr/n);return Math.abs(e.i-this.focusIdx)>2?Math.min(r,2):Math.max(r,p(t.pw*this.P*this.dpr/K))}artDist(e,t){return this.onPieceDist(e,t.fl+7+30+t.pw/2,w+t.ft+7+30+t.ph/2,G.art)}labelDist(e,t){return this.onPieceDist(e,t.lx+t.lw/2,w+t.ly+40,G.label)}lodStep(e,t){return Math.round(Q(t*this.dpr,this.lods.get(e)?.r)*100)}signature(){let t=this.pose;this.eye=[t.x,t.y,-t.d];let[n,r]=C(this.slots,t.d-520,t.d+(this.mini?3600:D)),i=n*7919+r|0;i=i*31+Math.round(t.d/140)|0,i=i*31+ +!!this.portrait|0;for(let t=n;t<r;t++){let n=this.slots[t];if(n.type===`work`){let t=this.layoutOf(n),r=this.artDist(n,t),a=this.P/this.labelDist(n,t);i=i*31+(n.item.poster?this.posterClass(n,t,r):9)|0,i=i*31+this.lodStep(`m${n.i}`,this.P/r)|0,i=i*31+this.lodStep(`l${n.i}`,a)|0,i=i*31+Math.round(e((17*a-2.5)/2.5,0,1)*8)|0}else if(n.type===`room`){let e=n.d0+230;n.portal&&(i=i*31+this.lodStep(`n${n.d0}`,this.P/Math.max(this.dist(0,T+62,-e),8))|0);let t=this.roomTextAt(n);i=i*31+this.lodStep(`r${n.d0}`,this.P/Math.max(this.dist(-S/2,-65,-(t+270)),8))|0}else i=i*31+this.lodStep(`e${n.d0}`,this.P/Math.max(this.dist(0,-139,-n.d1),8))|0}return i}wrapped(e,t,n,r){(this.wrapGen!==this.text.gen||this.wraps.size>400)&&(this.wraps.clear(),this.wrapGen=this.text.gen);let i=`${r.family}|${r.size}|${r.weight}|${t}|${n}|${e}`,a=this.wraps.get(i);return a||(a=b(e,t,n,e=>this.text.measure(e,r)),this.wraps.set(i,a)),a}dist(e,t,n){let r=this.eye,i=e-r[0],a=t-r[1],o=n-r[2];return Math.sqrt(i*i+a*a+o*o)}build(e){let t=this.batch;t.reset();let n=this.pose;this.eye=[n.x,n.y,-n.d];let[r,i]=C(this.slots,n.d-520,n.d+(this.mini?3600:D));if(r>=i){this.passes={reflections:0,architecture:0,decals:0};return}let a=[];for(let e=r;e<i;e++){let t=this.slots[e];t.type===`work`&&a.push(t)}this.stepSpots(a,e);let o=a.filter(e=>!this.mini||e.i<6);this.sortGroups(o.map(e=>({dist:this.pieceDist(e),draw:()=>this.piece(e,!0)})));let s=t.n,c=this.slots[r].d0-2,l=this.slots[i-1].d1+2;this.shell(c,l);let u=[];for(let e=r;e<i;e++){let t=this.slots[e];t.type===`room`&&t.portal?u.push({dist:this.dist(0,0,-(t.d0+230)),draw:()=>this.doorway(t)}):t.type===`end`&&u.push({dist:this.dist(0,0,-t.d1),draw:()=>this.endWall(t)})}this.sortGroups(u);let d=t.n,f=[];f.push({dist:1/0,draw:()=>this.stripe(c,l)});for(let e of a){f.push({dist:this.pieceDist(e),draw:()=>this.piece(e,!1)});let t=e.side*(S/2-170),n=this.frameD(e);f.push({dist:this.dist(t,T,-n),draw:()=>this.fixture(t,n)})}for(let e=r;e<i;e++){let t=this.slots[e];if(t.type===`room`){t.portal&&t.label&&f.push({dist:this.dist(0,T+62,-(t.d0+230)),draw:()=>this.lintelText(t)});let e=this.roomTextAt(t);t.label&&f.push({dist:this.dist(-S/2,-65,-(e+270)),draw:()=>this.roomText(t,e)})}else t.type===`end`&&f.push({dist:this.dist(0,0,-t.d1),draw:()=>this.endText(t)})}this.sortGroups(f),this.passes={reflections:s,architecture:d-s,decals:t.n-d}}sortGroups(e){e.sort((e,t)=>t.dist-e.dist);for(let t of e)t.draw()}pieceDist(e){let t=$(e.side,e.d-390,780,390,w,0,660,0);return this.dist(t.x,t.y,t.z)}stepSpots(e,t){let n=this.rm?1:1-Math.exp(-Math.max(t,1/120)/.3),r=new Map,i=!1;for(let t of e){let e=t.i===this.focusIdx?1:.72,a=this.spot.get(t.i),o=a===void 0?e:a+(e-a)*n;Math.abs(e-o)<.004?o=e:i=!0,r.set(t.i,o)}this.spot=r,this.spotsMoving=i}shell(e,t){let n=this.batch,r=t-e,i=-(e+t)/2,a=this.theme,o=n.add(0,330,i,S+4,r,k.floor);n.rotate(o,Math.PI/2,0),n.color(o,a.floor),n.uv(o,125,2,2,0),n.texture(o,this.grad.sheen,this.grad.board,-1),n.flags(o,A.noaa),o=n.add(0,T,i,S+4,r,k.linear),n.rotate(o,-Math.PI/2,0),n.color(o,q),n.uv(o,90*V,0,0,0),n.texture(o,this.grad.ceil,0,-1),n.flags(o,A.noaa);for(let t of[-1,1]){let i=$(t,e,r,0,T,r,700,0);o=n.add(i.x,i.y,i.z,i.w,i.h,k.linear),n.rotate(o,0,i.ry),n.color(o,q),n.uv(o,180*V,0,0,0),n.texture(o,this.grad.wall,0,-1),n.flags(o,A.noaa)}}stripe(e,t){let n=this.batch,r=n.add(0,T+G.face,-(e+t)/2,56,t-e,k.linear);n.rotate(r,-Math.PI/2,0),n.color(r,q),n.uv(r,90*V,0,0,0),n.texture(r,this.grad.stripe,0,-1)}fixture(e,t){let n=this.batch;for(let[r,i,a]of[[300,220,this.grad.fixtureGlow],[44,30,this.grad.fixture]]){let o=n.add(e,T+G.face,-t,r,i,k.radial);n.rotate(o,-Math.PI/2,0),n.color(o,q),n.uv(o,.5,.5,.5,.5),n.texture(o,a,0,-1)}}doorway(e){let t=this.batch,n=e.d0+230,r=this.theme;for(let e of[-1,1]){let r=$(e,n,48,0,T,48,700,72),i=t.add(r.x,r.y,r.z,r.w,r.h,k.linear);t.rotate(i,0,r.ry),t.color(i,q),t.uv(i,180*V,0,0,0),t.texture(i,this.grad.jambSide,0,-1),t.flags(i,A.noaa)}let i=t.add(0,T+124,-(n+24),S-144,48,k.solid);t.rotate(i,-Math.PI/2,0),t.color(i,r.jambSide),t.flags(i,A.noaa),i=t.add(0,T+350,-n,S+4,704,k.linear),t.color(i,q),t.uv(i,180*V,S/2-72,-226,0),t.texture(i,this.grad.jamb,0,-1),t.flags(i,A.noaa|A.cut)}endWall(e){let t=this.batch,n=t.add(0,T+350,-e.d1,S+4,704,k.linear);t.color(n,q),t.uv(n,180*V,0,0,0),t.texture(n,this.grad.end,0,-1),t.flags(n,A.noaa)}onPiece(e,t,n,r,i,a,o,s,c){let l=$(e.side,e.d-390,780,t,w+n,r,i,a),u=this.batch,d=u.add(l.x,l.y,l.z,l.w,l.h,o,s);return u.rotate(d,0,l.ry),c&&u.mirror(d,330,.5),d}piece(e,t){let n=this.batch,r=e.item,i=this.layoutOf(e),a=this.theme,o=!t&&e.i===this.hover?.08:0,s=312/780,c=t?.4:this.spot.get(e.i)??.72;for(let[r,i]of[[this.grad.pool,[s,.46,.33,.52]],[this.grad.poolTop,[s,0,.3,.07]]]){let a=this.onPiece(e,0,0,780,660,G.spot,k.radial,0,t);n.color(a,q),n.uv(a,i[0],i[1],i[2],i[3]),n.texture(a,r,0,-1),n.opacity(a,c)}let{fl:l,ft:u,fw:d,fh:f}=i;if(!t)for(let[t,r,i,o]of[[44,46,-34,0],[10,20,-8,.05]]){let s=d+2*i+2*r,c=f+2*i+2*r,p=this.onPiece(e,l+d/2-s/2,u+f/2+t-c/2,s,c,G.shadow+o,k.shadow,1,!1);n.color(p,a.shadow),n.extra(p,r)}let p=this.onPiece(e,l,u,d,f,G.frame,k.linear,1,t);n.color(p,q),n.uv(p,150*V,0,0,0),n.texture(p,this.grad.frame,0,-1),n.flags(p,A.hairline|(t?A.mirror:0)),n.sheen(p,o);let m=l+7,h=u+7,g=d-14,_=f-14;p=this.onPiece(e,m,h,g,_,G.mat,k.solid,0,t),n.color(p,a.mat),n.sheen(p,o),t||(p=this.onPiece(e,m,h+1,g,_,G.matShade,k.shadow,0,!1),n.color(p,[0,0,0,.22]),n.extra(p,4),n.flags(p,A.inset),p=this.onPiece(e,m,h,g,_,G.matShade+.02,k.ring,0,!1),n.color(p,[0,0,0,.05]),n.extra(p,1));let v=m+30,y=h+30;t||(p=this.onPiece(e,v-4,y-4,i.pw+8,i.ph+8,G.bevelLine,k.ring,0,!1),n.color(p,[0,0,0,.1]),n.extra(p,1)),p=this.onPiece(e,v-3,y-3,i.pw+6,i.ph+6,G.bevel,k.solid,0,t),n.color(p,a.bevel),n.sheen(p,o),r.id===this.hidden?(p=this.onPiece(e,v,y,i.pw,i.ph,G.art,k.solid,0,t),n.color(p,W),n.sheen(p,o)):this.print(e,i,v,y,t,o),p=this.onPiece(e,l,u,d,f,G.glaze,k.linear,1,t),n.color(p,q),n.uv(p,116*V,0,0,0),n.texture(p,this.grad.glaze,0,-1),t||this.label(e,i)}print(e,t,n,r,i,a){let o=this.batch,s=e.item,c=$(e.side,e.d-390,780,n,w+r,t.pw,t.ph,G.art),l=this.dist(c.x,c.y,c.z),u=null;s.poster&&(u=this.posters.get(s.poster,this.posterClass(e,t,Math.max(l,8)),l,!this.paused&&(!this.mini||e.i<8)));let d=this.onPiece(e,n,r,t.pw,t.ph,G.art,k.poster,0,i),p=i?A.mirror:0;s.available||(p|=A.gray);let m=s.poster?.blurhash?_(s.poster.blurhash):null;if(m)o.color(d,m);else{let[e,t]=f(s);o.color(d,e),o.uv(d,t[0],t[1],t[2],t[3]),p|=A.gradient}if(u&&(o.uv(d,0,0,1,1),o.texture(d,u.code,u.texWidth,u.readyAt),p&=~A.gradient),o.flags(d,p),o.sheen(d,a),i)return;let h=this.P/Math.max(l,8);this.marks(e,s,n,r,t.pw,t.ph,h),!s.poster&&t.pw>=72&&this.typeset(e,s,n,r,t.pw,t.ph,h)}lodFor(e,t){let n=t*this.dpr,r=this.lods.get(e),i=Q(n,r?.r);return(!r||r.r!==i)&&this.lods.set(e,{r:i}),this.lods.size>600&&this.lods.clear(),{r:i,alpha:1}}glyphOnPiece(e,t,n,r,i,a,o){let s=this.onPiece(e,n,r,t.w,t.h,i,k.text,0,!1);this.batch.color(s,a,o),this.batch.uv(s,t.u0,t.v0,t.u1,t.v1)}marks(e,t,n,o,s,l,u){let d=this.batch,{r:f}=this.lodFor(`m${e.i}`,u),p={family:this.theme.mono,size:9.28,weight:500,spacing:.08,upper:!0},m=15.6,h=G.marks,g=(t,r,i,a,s,c,l,u)=>{h+=.01;let f=this.onPiece(e,n+t,o+r,i,a,h,s,c,!1);return d.color(f,l,u),f},_=(t,r,i,a)=>{h+=.01,this.glyphOnPiece(e,t,n+r,o+i,h,a,1)},v=t.poster?c(t.poster):null;if(v){let e=v.tone===`ai`,t=this.text.get(v.short,p,200,f),n=e?this.text.icon(`spark`,9,f):null;if(t){let r=12+(t.w-t.pad*2)+(n?13:0);g(6,6,r,m,k.solid,4,X,e?.86:.72),e&&d.extra(g(6,6,r,m,k.ring,4,q,.55),1);let i=12;n&&(_(n,i-n.pad,6+(m-n.h)/2,q),i+=13),_(t,i-t.pad,17-t.baseline,e?q:Y)}}let y=i(t);if(y!==null)g(s*.08,l-10,s*.84,3,k.solid,1.5,q,.3),g(s*.08,l-10,s*.84*Math.max(.03,y),3,k.solid,1.5,q,1);else if(a(t)){g(s-26,6,20,20,k.solid,10,X,.6);let e=this.text.icon(`check`,11,f);e&&_(e,s-16-e.w/2,16-e.h/2,q)}else r(t)&&(d.extra(g(s-28,-4,32,32,k.shadow,4,q,.8),12),g(s-19,5,14,14,k.solid,7,se,.25),g(s-16,8,8,8,k.solid,4,q,1));if(!t.available){let e=this.text.get(`Offline`,{...p,weight:400},200,f);if(e){let t=12+e.w-e.pad*2;g(6,l-6-m,t,m,k.solid,4,X,.75),_(e,12-e.pad,l-6-m+m/2+3.2-e.baseline,Y)}}}typeset(t,n,r,i,a,o,s){let c=Math.round(e(a*.11,12.8,25.6)*4)/4,l=a*.11,u=Math.round(a-l*2),d={family:this.theme.body,size:c,weight:500},f=this.wrapped(n.title,u,4,d),{r:p}=this.lodFor(`m${t.i}`,s),m=c*1.05,h=o-l-f.length*m;for(let e of f){let n=this.text.get(e,d,u,p);n&&this.glyphOnPiece(t,n,r+l-n.pad,i+h+m*.82-n.baseline,G.marks,q,1),h+=m}}styles(){let e=this.theme;return{title:{family:e.body,size:17,weight:500,spacing:.004},year:{family:e.body,size:15,weight:400},what:{family:e.body,size:13,weight:400},medium:{family:e.mono,size:10.5,weight:400,spacing:.04},status:{family:e.body,size:11.5,weight:400}}}labelLayout(e,t){this.layoutGen!==this.text.gen&&(this.layouts.clear(),this.layoutGen=this.text.gen);let n=`${e.key}|${t}`,r=this.layouts.get(n);if(r)return r;let i=this.styles(),a=ne(e.item),o=[],s=(e,t)=>this.text.measure(e,t),c=0,l=17*1.4,u=b(a.title,t,3,e=>s(e,i.title));if(u.forEach((e,t)=>{o.push({text:e,style:i.title,color:`ink`,x:0,top:c,lh:l}),t<u.length-1&&(c+=l)}),a.year){let e=`, ${a.year}`,n=s(u[u.length-1]??``,i.title);n+s(e,i.year)<=t?o.push({text:e,style:i.year,color:`ink2`,x:n,top:c,lh:l}):(c+=l,o.push({text:a.year,style:i.year,color:`ink2`,x:0,top:c,lh:l}))}if(c+=l,c+=3,o.push({text:a.what,style:i.what,color:`ink2`,x:0,top:c,lh:18.2}),c+=18.2,a.medium&&(c+=8,o.push({text:a.medium,style:i.medium,color:`ink3`,x:0,top:c,lh:14.7}),c+=14.7),a.status){c+=10;let e=a.tone===`new`?`fresh`:a.tone===`offline`?`away`:`plain`;o.push({text:a.status,style:i.status,color:a.tone===`offline`?`away`:`ink2`,x:13,top:c,lh:11.5*1.4,dot:e})}return this.layouts.size>400&&this.layouts.clear(),this.layouts.set(n,o),o}label(t,n){let r=this.P/this.labelDist(t,n),i=e((17*r-2.5)/2.5,0,1);if(i<=0)return;let{r:a}=this.lodFor(`l${t.i}`,r),o=this.theme,s={ink:o.ink,ink2:o.ink2,ink3:o.ink3,away:o.away},c=this.labelLayout(t,n.lw),l=`${t.i}|${a}|${this.text.gen}`,u=this.labelGlyphs.get(l);(!u||u.length!==c.length)&&(u=c.map(e=>this.text.get(e.text,e.style,n.lw-e.x,a)),u.every(Boolean)&&this.labelGlyphs.set(l,u),this.labelGlyphs.size>600&&this.labelGlyphs.clear());for(let e=0;e<c.length;e++){let r=c[e],a=u[e];if(!a)continue;let l=a.h-2*a.pad,d=r.top+(r.lh-l)/2+(a.baseline-a.pad);if(this.glyphOnPiece(t,a,n.lx+r.x-a.pad,n.ly+d-a.baseline,G.label,s[r.color],i),r.dot){let e=r.dot===`away`?o.away:r.dot===`fresh`?o.ink:o.ink2,a=n.ly+r.top+r.lh/2;if(r.dot===`fresh`){let e=this.onPiece(t,n.lx-8,a-11,22,22,G.label,k.shadow,11,!1);this.batch.color(e,o.ink,.85*i),this.batch.extra(e,8)}let s=this.onPiece(t,n.lx,a-3,6,6,G.label+.01,k.solid,3,!1);this.batch.color(s,e,(r.dot===`plain`?.6:1)*i)}}}roomTextAt(e){return e.portal?e.d0+230+48+210:e.d0+30}wallGlyph(e,t,n,r,i,a,o,s,c=1){let l=$(e,t,n,i,a,r.w,r.h,o),u=this.batch,d=u.add(l.x,l.y,l.z,l.w,l.h,k.text);u.rotate(d,0,l.ry),u.color(d,s,c),u.uv(d,r.u0,r.v0,r.u1,r.v1)}faceGlyph(e,t,n,r,i,a){let o=this.batch,s=o.add(t+e.w/2,n+e.h/2,-r+i,e.w,e.h,k.text);o.color(s,a),o.uv(s,e.u0,e.v0,e.u1,e.v1)}baselineIn(e,t,n){return t+(n-(e.h-2*e.pad))/2+(e.baseline-e.pad)}roomText(e,t){let n=this.theme,r=this.P/Math.max(this.dist(-S/2,-65,-(t+270)),8),{r:i}=this.lodFor(`r${e.d0}`,r),a=Math.min(104,Math.round(1700/Math.max(5,e.label.length))),o={family:n.body,size:a,weight:300,spacing:-.03},s={family:n.mono,size:13,weight:400,spacing:.12,upper:!0},c={family:n.body,size:19,weight:400},l=e.by?this.wrapped(e.by,420,4,c):[],u=[];u.push({g:this.text.get(e.label,o,2e3,i),lh:a*.9,color:n.ink}),u.push({g:this.text.get(e.sub,s,2e3,i),lh:15.6,color:n.ink3});let d=l.map(e=>({g:this.text.get(e,c,420,i),lh:27.55,color:n.ink2})),f=90-(u[0].lh+16+u[1].lh+(d.length?16+d.reduce((e,t)=>e+t.lh,0):0)),p=e=>{e.g&&this.wallGlyph(-1,t,540,e.g,30-e.g.pad,this.baselineIn(e.g,f,e.lh)-e.g.baseline,G.label,e.color),f+=e.lh};if(p(u[0]),f+=16,p(u[1]),d.length){f+=16;for(let e of d)p(e)}}lintelText(e){let t=this.theme,n=e.d0+230,r=this.P/Math.max(this.dist(0,T+62,-n),8),{r:i}=this.lodFor(`n${e.d0}`,r),a=Math.min(64,Math.round(1300/Math.max(6,e.label.length))),o=this.text.get(e.label,{family:t.body,size:a,weight:300,spacing:-.02},2e3,i),s=this.text.get(e.sub,{family:t.mono,size:13,weight:400,spacing:.12,upper:!0},2e3,i),c=a*.95,l=15.6,u=T+(124-(c+12+l))/2;o&&this.faceGlyph(o,-(o.w-2*o.pad)/2-o.pad,this.baselineIn(o,u,c)-o.baseline,n,G.face,t.ink),u+=c+12,s&&this.faceGlyph(s,-(s.w-2*s.pad)/2-s.pad,this.baselineIn(s,u,l)-s.baseline,n,G.face,t.ink3)}endText(e){let t=this.batch,n=this.theme,r=e.d1,i=t.add(0,86,-r+G.face,490,690,k.shadow,0);t.color(i,n.doorGlow),t.extra(i,90),i=t.add(0,86,-r+G.door,250,450,k.linear,0),t.color(i,q),t.uv(i,180*V,0,0,0),t.texture(i,this.grad.door,0,-1);let a=e.pending>0?`More works are being hung`:e.count>0?`End of the exhibition`:``;if(!a)return;let o=this.P/Math.max(this.dist(0,-139,-r),8),{r:s}=this.lodFor(`e${e.d0}`,o),c=e.pending>0?`${e.count.toLocaleString()} of ${(e.count+e.pending).toLocaleString()} so far`:ie(e.count,`work`,`works`),l=this.text.get(a,{family:n.body,size:22,weight:300},S,s),u=this.text.get(c,{family:n.mono,size:12,weight:400,spacing:.12,upper:!0},S,s),d=-221.8;l&&this.faceGlyph(l,-(l.w-2*l.pad)/2-l.pad,this.baselineIn(l,d,26.4)-l.baseline,r,G.face,n.ink2),d+=34.4,u&&this.faceGlyph(u,-(u.w-2*u.pad)/2-u.pad,this.baselineIn(u,d,14.399999999999999)-u.baseline,r,G.face,n.ink3)}cam(){let e=this.pose;return{persp:this.P,yaw:e.yaw*V,eyeX:-e.x,eyeY:-e.y,eyeZ:e.d,centerX:this.vw/2,centerY:this.vh/2,near:8}}corners(e,t,n,r,i,a){let o=$(e.side,e.d-390,780,t,w+n,r,i,a);return P({x:o.x,y:o.y,z:o.z,w:o.w,h:o.h,ry:o.ry},this.cam())}boxOf(e){if(e.some(e=>!e))return null;let t=e,n=t.map(e=>e.x),r=t.map(e=>e.y),i=Math.min(...n),a=Math.min(...r);return new DOMRect(i,a,Math.max(...n)-i,Math.max(...r)-a)}workAtPoint(e,t){let[n,r]=C(this.slots,this.pose.d-520,this.pose.d+D),i=-1,a=1/0;for(let o=n;o<r;o++){let n=this.slots[o];if(n.type!==`work`)continue;let r=this.layoutOf(n),s=this.corners(n,r.fl,r.ft,r.fw,r.fh,G.frame);if(!x(e,t,s))continue;let c=this.pieceDist(n);c<a&&(a=c,i=n.i)}return i}workAt(e){let t=this.root.getBoundingClientRect();return this.workAtPoint(e.clientX-t.left,e.clientY-t.top)}pointer(e){if(e.pointerType!==`mouse`)return;let t=this.dragging?-1:this.workAt(e);t!==this.hover&&(this.hover=t,this.root.classList.toggle(O.overFrame,t>=0),this.dirty())}artRect(e){let t=this.works.find(t=>t.item.id===e);if(!t)return null;let n=this.layoutOf(t),r=this.boxOf(this.corners(t,n.fl+7+30,n.ft+7+30,n.pw,n.ph,G.art));if(!r)return null;let i=this.root.getBoundingClientRect();return new DOMRect(i.left+r.x,i.top+r.y,r.width,r.height)}syncOptions(){if(this.mini||this.gone||this.dom)return;let t=new Set,n=this.works.length,r=e(this.focusIdx-4,0,n),i=e(this.focusIdx+5,0,n);for(let e=r;e<i;e++){let n=this.works[e];this.place(n.item.id)&&t.add(n.item.id)}for(let[e,n]of this.opts)t.has(e)||n===document.activeElement||(n.remove(),this.opts.delete(e))}place(e){let n=this.works.find(t=>t.item.id===e);if(!n)return!1;let r=this.layoutOf(n),i=this.boxOf(this.corners(n,r.fl+7+30,r.ft+7+30,r.pw,r.ph,G.art));if(!i||i.right<0||i.left>this.vw||i.bottom<0||i.top>this.vh)return!1;let a=this.opts.get(e);a||(a=t(`button`,O.option,{type:`button`,tabindex:`-1`,"data-work":e}),a.addEventListener(`click`,()=>this.openAt(this.works.findIndex(t=>t.item.id===e))),this.opts.set(e,a),this.root.append(a));let o=l(n.item);return a.getAttribute(`aria-label`)!==o&&a.setAttribute(`aria-label`,o),a.style.transform=`translate(${i.x.toFixed(1)}px, ${i.y.toFixed(1)}px)`,a.style.width=`${i.width.toFixed(1)}px`,a.style.height=`${i.height.toFixed(1)}px`,!0}rectOf(e){return this.dom?this.dom.rectOf(e):this.artRect(e)}spotOf(e){if(this.dom)return this.dom.spotOf?.(e)??null;let t=this.works.find(t=>t.item.id===e);if(!t)return null;let n=this.layoutOf(t),r=this.root.getBoundingClientRect(),i=s(this.corners(t,n.fl+7+30,n.ft+7+30,n.pw,n.ph,G.art),r.left,r.top);return i?{quad:i,radius:0,src:t.item.poster?this.posters.readyUrl(t.item.poster):null,lift:.15,aspect:n.pw/n.ph}:null}artOf(e){return this.dom?this.dom.artOf?.(e)??null:this.mini?null:this.place(e)?this.opts.get(e)??null:null}hide(e){if(this.dom)return this.dom.hide?.(e);this.hidden=e,this.dirty()}update(e,t){if(this.dom)return this.dom.update(e,t);super.update(e,t)}move(e){return this.dom?this.dom.move(e):super.move(e)}key(e){return this.dom?this.dom.key?.(e)??!1:super.key(e)}reveal(e){if(this.dom)return this.dom.reveal?.(e);super.reveal(e)}pause(e){if(this.dom)return this.dom.pause?.(e);super.pause(e),e&&this.paintRaf?(cancelAnimationFrame(this.paintRaf),this.paintRaf=0):e||this.dirty()}teardown(){this.stop(),this.paintRaf&&cancelAnimationFrame(this.paintRaf),this.paintRaf=0,this.renderer.destroy(),this.canvas.remove();for(let e of this.opts.values())e.remove();this.opts.clear(),this.root.classList.remove(O.overFrame)}fallBack(){this.dom||this.gone||(this.teardown(),this.dom={update:()=>void 0,move:()=>!1,rectOf:()=>null,destroy:()=>void 0},o(async()=>{let{gallery:e}=await import(`./gallery-DEW-QyPH.js`);return{gallery:e}},__vite__mapDeps([0,1,2,3,4,5,6,7,8,9,10,11,12])).then(({gallery:e})=>{this.gone||(this.dom=e.mount(this.root,this.ctx))}))}destroy(){this.gone||(this.gone=!0,this.dom?this.dom.destroy():this.teardown())}};async function le(e){let[t,n]=await Promise.all([z(e),g(e).prepare()]);return!t||!n?null:{mount(t,n){if(e.lost)throw Error(`The GPU device is gone.`);return new ce(t,n,e)}}}export{U as FARTHEST,oe as MIN_GAP,H as NEAR,G as OFF,le as gpuGallery,Q as textLod,$ as wallQuad};