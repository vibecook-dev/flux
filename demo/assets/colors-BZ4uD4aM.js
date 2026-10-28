import{a as e}from"./client-CC7PKe2z.js";import{s as t}from"./palette-BhehA8KE.js";import{n,t as r}from"./device-BqoaPgjv.js";var i=`
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
`,a=`
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
`,o={solid:0,poster:1,text:2,ring:3,shadow:4,skeleton:5},s={gray:1,hairline:2,gradient:8},c=performance.now();function l(){return(performance.now()-c)/1e3}var u=class{data;n=0;constructor(e=1024){this.data=new Float32Array(e*24)}reset(){this.n=0}add(e,t,n,r,i,a=0){if((this.n+1)*24>this.data.length){let e=new Float32Array(this.data.length*2);e.set(this.data),this.data=e}let o=this.n*24,s=this.data;return s[o]=e,s[o+1]=t,s[o+2]=n,s[o+3]=r,s.fill(0,o+4,o+24),s[o+16]=a,s[o+17]=i,s[o+19]=1,s[o+20]=-1,this.n++,o}from(e,t,n,r,i){let a=this.data;a[e+4]=t,a[e+5]=n,a[e+6]=r,a[e+7]=i}color(e,t,n=1){let r=this.data;r[e+8]=t[0],r[e+9]=t[1],r[e+10]=t[2],r[e+11]=t[3]*n}uv(e,t,n,r,i){let a=this.data;a[e+12]=t,a[e+13]=n,a[e+14]=r,a[e+15]=i}texture(e,t,n,r){let i=this.data;i[e+18]=t,i[e+20]=n,i[e+22]=r}opacity(e,t){this.data[e+19]=t}extra(e,t){this.data[e+22]=t}flags(e,t){this.data[e+23]=t}},d=new WeakMap,f=new WeakMap;function p(e){let t=f.get(e.device);return t||(t=m(e),f.set(e.device,t)),t}async function m(e){try{let t=e.device,n=t.createShaderModule({label:`flux quads`,code:i}),r=await t.createRenderPipelineAsync({label:`flux quads`,layout:`auto`,vertex:{module:n,entryPoint:`vs`},fragment:{module:n,entryPoint:`fs`,targets:[{format:e.format,blend:{color:{srcFactor:`one`,dstFactor:`one-minus-src-alpha`,operation:`add`},alpha:{srcFactor:`one`,dstFactor:`one-minus-src-alpha`,operation:`add`}}}]},primitive:{topology:`triangle-strip`}}),a=t.createSampler({magFilter:`linear`,minFilter:`linear`,mipmapFilter:`linear`,addressModeU:`clamp-to-edge`,addressModeV:`clamp-to-edge`});return d.set(t,{pipeline:r,sampler:a}),!0}catch(e){return console.error(`flux: the GPU lens pipeline failed to compile; using the DOM lens`,e),!1}}function h(e){let t=d.get(e.device);if(!t)throw Error(`prepareQuads() first`);return t}var g=class{canvas;gpu;ctx;uniforms;storage=null;capacity=0;bindGroup=null;bindKey=``;u=new Float32Array(12);constructor(e,t){this.gpu=e,this.canvas=t;let n=t.getContext(`webgpu`);if(!n)throw Error(`This canvas has no WebGPU context.`);this.ctx=n,n.configure({device:e.device,format:e.format,alphaMode:`premultiplied`}),this.uniforms=e.device.createBuffer({label:`flux frame`,size:48,usage:r.UNIFORM|r.COPY_DST})}resize(e,t){let n=Math.max(1,Math.round(e)),r=Math.max(1,Math.round(t));this.canvas.width!==n&&(this.canvas.width=n),this.canvas.height!==r&&(this.canvas.height=r)}draw(e,t,n){let i=this.gpu.device,{pipeline:a,sampler:o}=h(this.gpu),s=e.n,c=Math.max(1,s)*24*4;(!this.storage||c>this.capacity)&&(this.storage?.destroy(),this.capacity=Math.max(c,this.capacity*2,6144),this.storage=i.createBuffer({label:`flux quads`,size:this.capacity,usage:r.STORAGE|r.COPY_DST}),this.bindKey=``),s&&i.queue.writeBuffer(this.storage,0,e.data.buffer,e.data.byteOffset,s*24*4);let l=this.u;l[0]=t.width,l[1]=t.height,l[2]=t.scrollX,l[3]=t.scrollY,l[4]=t.focalX,l[5]=t.focalY,l[6]=t.scale,l[7]=t.dpr,l[8]=t.time,l[9]=t.t,i.queue.writeBuffer(this.uniforms,0,l),(!this.bindGroup||this.bindKey!==n.key)&&(this.bindGroup=i.createBindGroup({label:`flux quads`,layout:a.getBindGroupLayout(0),entries:[{binding:0,resource:{buffer:this.uniforms}},{binding:1,resource:{buffer:this.storage}},{binding:2,resource:o},{binding:3,resource:n.posters[0]},{binding:4,resource:n.posters[1]},{binding:5,resource:n.posters[2]},{binding:6,resource:n.posters[3]},{binding:7,resource:n.glyphs}]}),this.bindKey=n.key);let u=i.createCommandEncoder({label:`flux frame`}),d=u.beginRenderPass({colorAttachments:[{view:this.ctx.getCurrentTexture().createView(),clearValue:{r:0,g:0,b:0,a:0},loadOp:`clear`,storeOp:`store`}]});s&&(d.setPipeline(a),d.setBindGroup(0,this.bindGroup),d.draw(4,s)),d.end(),i.queue.submit([u.finish()])}destroy(){try{this.ctx.unconfigure()}catch{}this.storage?.destroy(),this.uniforms.destroy()}},_=class{capacity;keys=[];used=[];index=new Map;constructor(e){this.capacity=e}get size(){return this.index.size}find(e){return this.index.get(e)??-1}touch(e,t){this.used[e]=t}grow(e){this.capacity=Math.max(this.capacity,e)}claim(e,t,n=t){let r=this.index.get(e);if(r!==void 0)return this.used[r]=t,{slot:r,evicted:null};let i=-1;if(this.keys.length<this.capacity)i=this.keys.length;else{let e=1/0;for(let t=0;t<this.keys.length;t++){if(this.keys[t]===null){i=t;break}this.used[t]<n&&this.used[t]<e&&(e=this.used[t],i=t)}}if(i<0)return{slot:-1,evicted:null};let a=this.keys[i]??null;return a!==null&&this.index.delete(a),this.keys[i]=e,this.used[i]=t,this.index.set(e,i),{slot:i,evicted:a}}release(e){let t=this.index.get(e);t!==void 0&&(this.index.delete(e),this.keys[t]=null)}clear(){this.keys=[],this.used=[],this.index.clear()}},v=[{w:128,h:192,mips:3,bucket:160,max:1024},{w:256,h:384,mips:4,bucket:320,max:384},{w:384,h:576,mips:4,bucket:480,max:192},{w:640,h:960,mips:5,bucket:640,max:72}];function y(e){for(let t=0;t<v.length;t++)if(v[t].w>=e*.92)return t;return v.length-1}var b=8,x=3,S=2e4;function C(e,t,n,r){let i=n/r,a=e/t;if(a>i*1.02){let n=Math.max(1,Math.round(t*i));return{x:Math.round((e-n)/2),y:0,w:n,h:t}}if(a<i/1.02){let n=Math.max(1,Math.round(e/i));return{x:0,y:Math.round((t-n)/2),w:e,h:n}}return{x:0,y:0,w:e,h:t}}async function w(e,t,n,r){let i=await fetch(e,{signal:r});if(!i.ok)throw Error(`art ${i.status}`);let a=await i.blob();if(!a.type.includes(`svg`))try{let e=await createImageBitmap(a);return{bitmap:e,crop:C(e.width,e.height,t,n)}}catch{}let o=URL.createObjectURL(a);try{let e=new Image;e.decoding=`async`,e.src=o,await e.decode();let r={resizeWidth:t,resizeHeight:n,resizeQuality:`low`},i=e.naturalWidth,a=e.naturalHeight,s=i&&a?C(i,a,t,n):null,c=s?await createImageBitmap(e,s.x,s.y,s.w,s.h,r):await createImageBitmap(e,r);return{bitmap:c,crop:{x:0,y:0,w:c.width,h:c.height}}}finally{URL.revokeObjectURL(o)}}var T=class{spec;index;tex=null;view=null;layers=0;slots;demand=0;lastUsed=0;constructor(e,t){this.spec=e,this.index=t,this.slots=new _(0)}},E=class{gpu;stores;entries=new Map;shown=new Map;decoded=[];loading=0;dummy;dummyView;mipPipeline=null;mipSampler;listeners=new Set;demandNow=[0,0,0,0];demandSeen=[new Set,new Set,new Set,new Set];frame=0;version=1;fadeUntil=0;constructor(e){this.gpu=e;let t=e.device.limits.maxTextureArrayLayers;this.stores=v.map((e,n)=>new T({...e,max:Math.min(e.max,t)},n)),this.dummy=e.device.createTexture({label:`flux poster placeholder`,size:[1,1,1],format:`rgba8unorm`,usage:n.TEXTURE_BINDING,dimension:`2d`}),this.dummyView=this.dummy.createView({dimension:`2d-array`}),this.mipSampler=e.device.createSampler({minFilter:`linear`,magFilter:`linear`,mipmapFilter:`linear`})}onReady(e){return this.listeners.add(e),()=>this.listeners.delete(e)}begin(){this.frame++;for(let e=0;e<this.stores.length;e++)this.stores[e].demand=Math.max(this.demandNow[e],Math.round(this.stores[e].demand*.98)),this.demandNow[e]=0,this.demandSeen[e].clear()}count(e,t){let n=this.demandSeen[e];n.has(t)||(n.add(t),this.demandNow[e]++)}get(e,t,n,r){let i=e.url,a=this.stores[t];a.lastUsed=performance.now(),this.count(t,i);let o=this.entries.get(`${t}|${i}`);if(o?.state===`ready`)return this.hit(o);r?this.ask(i,t,n):o&&(o.seen=this.frame);for(let e=1;e<v.length;e++){let n=t+e<v.length?this.entries.get(`${t+e}|${i}`):void 0;if(n?.state===`ready`)return this.hit(n);let r=t-e>=0?this.entries.get(`${t-e}|${i}`):void 0;if(r?.state===`ready`)return this.hit(r)}return null}readyUrl(e){for(let t=v.length-1;t>=0;t--){let n=this.entries.get(`${t}|${e.url}`);if(n?.state===`ready`)return n.url}return null}want(e,t,n){this.count(t,e.url),this.ask(e.url,t,n)}ask(t,n,r){let i=`${n}|${t}`,a=this.entries.get(i);a?(a.seen=this.frame,a.prioFrame===this.frame?r<a.prio&&(a.prio=r):(a.prio=r,a.prioFrame=this.frame),a.state===`failed`&&performance.now()>a.retryAt&&(a.state=`want`)):this.entries.set(i,{key:i,url:e({url:t},v[n].bucket),art:t,cls:n,state:`want`,prio:r,prioFrame:this.frame,seen:this.frame,slot:-1,ctl:null,bitmap:null,crop:null,retryAt:0})}hit(e){this.stores[e.cls].slots.touch(e.slot,this.frame),e.seen=this.frame;let t=this.shown.get(e.art)?.at??0;return{code:e.cls*4096+e.slot,texWidth:v[e.cls].w,readyAt:t}}pump(){let e=this.frame-2,t=[];for(let n of this.entries.values())n.state===`want`?n.seen<e?this.entries.delete(n.key):t.push(n):n.state===`loading`&&n.seen<this.frame-30&&n.ctl?.abort();if(!(!t.length||this.loading>=b)){t.sort((e,t)=>e.prio-t.prio);for(let e of t){if(this.loading>=b)break;this.start(e)}}}start(e){let t=v[e.cls];e.state=`loading`,e.ctl=new AbortController,this.loading++,w(e.url,t.w,t.h,e.ctl.signal).then(({bitmap:t,crop:n})=>{if(this.loading--,e.ctl=null,this.entries.get(e.key)!==e)t.close();else{e.bitmap=t,e.crop=n,e.state=`decoded`,this.decoded.push(e);for(let e of this.listeners)e()}},()=>{this.loading--,e.ctl=null,this.entries.get(e.key)===e&&(e.seen<this.frame-30?this.entries.delete(e.key):(e.state=`failed`,e.retryAt=performance.now()+3e4))})}get pending(){return this.decoded.length>0}upload(){if(!this.decoded.length)return!1;let e=this.gpu.device,t=performance.now(),n=l();this.decoded.sort((e,t)=>e.prio-t.prio);let r=e.createCommandEncoder({label:`flux poster mips`}),i=[],a=!1,o=[];for(let s=0;s<this.decoded.length;s++){let c=this.decoded[s];if(performance.now()-t>x){o.push(...this.decoded.slice(s));break}let l=c.bitmap;if(!l||this.entries.get(c.key)!==c){l?.close();continue}if(c.seen<this.frame-60){l.close(),this.entries.delete(c.key);continue}let u=this.stores[c.cls];this.reserve(u,r);let{slot:d,evicted:f}=u.slots.claim(c.key,this.frame,this.frame-3);if(d<0){u.layers<u.spec.max?o.push(c):(l.close(),c.bitmap=null,c.state=`failed`,c.retryAt=performance.now()+5e3);continue}f&&this.evict(f);let p=c.crop??{x:0,y:0,w:l.width,h:l.height};p.x===0&&p.y===0&&p.w===u.spec.w&&p.h===u.spec.h&&l.width===p.w&&l.height===p.h?e.queue.copyExternalImageToTexture({source:l},{texture:u.tex,origin:[0,0,d],premultipliedAlpha:!0},[u.spec.w,u.spec.h,1]):this.fit(r,u,d,l,p,i),l.close(),c.bitmap=null,c.crop=null,this.mips(r,u,d),c.slot=d,c.state=`ready`;let m=this.shown.get(c.art);m?m.count++:this.shown.set(c.art,{count:1,at:n}),this.fadeUntil=Math.max(this.fadeUntil,n+.32),a=!0}return e.queue.submit([r.finish()]),i.length&&e.queue.onSubmittedWorkDone().then(()=>i.forEach(e=>e.destroy())),this.decoded=o,a}fit(e,t,r,i,a,o){let s=this.gpu.device,c=Math.max(a.w/t.spec.w,a.h/t.spec.h),l=c>=2?Math.min(1+Math.floor(Math.log2(c)),8):1,u=s.createTexture({label:`flux poster staging`,size:[a.w,a.h],mipLevelCount:l,format:`rgba8unorm`,usage:n.TEXTURE_BINDING|n.COPY_DST|n.RENDER_ATTACHMENT});o.push(u),s.queue.copyExternalImageToTexture({source:i,origin:[a.x,a.y]},{texture:u,premultipliedAlpha:!0},[a.w,a.h]);for(let t=1;t<l;t++)this.blit(e,u.createView({baseMipLevel:t-1,mipLevelCount:1}),u.createView({baseMipLevel:t,mipLevelCount:1}));let d=t.tex.createView({dimension:`2d`,baseMipLevel:0,mipLevelCount:1,baseArrayLayer:r,arrayLayerCount:1});this.blit(e,u.createView(),d)}blit(e,t,n){if(!this.mipPipeline)return;let r=this.gpu.device.createBindGroup({layout:this.mipPipeline.getBindGroupLayout(0),entries:[{binding:0,resource:t},{binding:1,resource:this.mipSampler}]}),i=e.beginRenderPass({colorAttachments:[{view:n,loadOp:`clear`,storeOp:`store`,clearValue:{r:0,g:0,b:0,a:0}}]});i.setPipeline(this.mipPipeline),i.setBindGroup(0,r),i.draw(3),i.end()}evict(e){let t=this.entries.get(e);if(!t)return;this.entries.delete(e);let n=this.shown.get(t.art);n&&--n.count<=0&&this.shown.delete(t.art)}reserve(e,t){let r=Math.min(e.spec.max,Math.max(16,Math.ceil((e.demand+8)*1.25)));if(e.tex&&e.layers>=r||e.tex&&e.slots.size<e.layers)return;let i=Math.max(16,e.layers);for(;i<r;)i*=2;if(i=Math.min(i,e.spec.max),e.tex&&i<=e.layers)return;let a=this.gpu.device,o=a.createTexture({label:`flux posters ${e.spec.w}`,size:[e.spec.w,e.spec.h,i],mipLevelCount:e.spec.mips,format:`rgba8unorm`,usage:n.TEXTURE_BINDING|n.COPY_DST|n.COPY_SRC|n.RENDER_ATTACHMENT});if(e.tex){for(let n=0;n<e.spec.mips;n++)t.copyTextureToTexture({texture:e.tex,mipLevel:n},{texture:o,mipLevel:n},[e.spec.w>>n,e.spec.h>>n,e.layers]);let n=e.tex;queueMicrotask(()=>void a.queue.onSubmittedWorkDone().then(()=>n.destroy()))}e.tex=o,e.view=o.createView({dimension:`2d-array`}),e.layers=i,e.slots.grow(i),this.version++}preparing=null;prepare(){return this.preparing??=this.compileMips(),this.preparing}async compileMips(){try{let e=this.gpu.device,t=e.createShaderModule({label:`flux mips`,code:a});return this.mipPipeline=await e.createRenderPipelineAsync({label:`flux mips`,layout:`auto`,vertex:{module:t,entryPoint:`vs`},fragment:{module:t,entryPoint:`fs`,targets:[{format:`rgba8unorm`}]},primitive:{topology:`triangle-list`}}),!0}catch(e){return console.error(`flux: the poster mip pipeline failed to compile`,e),!1}}mips(e,t,n){let r=t.tex;for(let i=1;i<t.spec.mips;i++){let t=r.createView({dimension:`2d`,baseMipLevel:i-1,mipLevelCount:1,baseArrayLayer:n,arrayLayerCount:1}),a=r.createView({dimension:`2d`,baseMipLevel:i,mipLevelCount:1,baseArrayLayer:n,arrayLayerCount:1});this.blit(e,t,a)}}views(){return this.stores.map(e=>e.view??this.dummyView)}holders=0;releaseT=null;retain(){this.holders++,this.releaseT&&=(clearTimeout(this.releaseT),null);let e=!1;return()=>{e||(e=!0,!(--this.holders>0)&&(this.releaseT=setTimeout(()=>{this.releaseT=null,this.holders===0&&this.trim(!0)},3e4)))}}trim(e=!1){let t=performance.now();for(let n of this.stores){if(!n.tex||!e&&t-n.lastUsed<S)continue;let r=n.tex;this.gpu.device.queue.onSubmittedWorkDone().then(()=>r.destroy()),n.tex=null,n.view=null,n.layers=0,n.slots=new _(0),n.demand=0;for(let[e,t]of this.entries)if(t.cls===n.index&&(t.state===`ready`||t.state===`decoded`)){t.bitmap?.close(),this.entries.delete(e);let n=this.shown.get(t.art);n&&t.state===`ready`&&--n.count<=0&&this.shown.delete(t.art)}this.decoded=this.decoded.filter(e=>e.cls!==n.index),this.version++}}},D=new WeakMap;function O(e){let t=D.get(e.device);return t||(t=new E(e),D.set(e.device,t)),t}var k=2048,A=2;function j(e,t,n){if(n(e)<=t)return e;let r=Array.from(e),i=0,a=r.length;for(;i<a;){let e=i+a+1>>1;n(r.slice(0,e).join(``).trimEnd()+`…`)<=t?i=e:a=e-1}return i===0?`…`:r.slice(0,i).join(``).trimEnd()+`…`}function M(e,t,n,r){let i=e.split(/\s+/).filter(Boolean),a=[],o=``;for(let e=0;e<i.length;e++){let s=o?`${o} ${i[e]}`:i[e];if(r(s)<=t||!o)o=s;else if(a.push(o),o=i[e],a.length===n-1){o=i.slice(e).join(` `);break}}return o&&a.push(o),a.slice(0,n).map((e,n,i)=>n===i.length-1?j(e,t,r):e)}var N=class{view;gen=0;gpu;tex;canvas;ctx;entries=new Map;shelves=[];nextY=0;dirty=null;listeners=new Set;constructor(e){this.gpu=e,this.tex=e.device.createTexture({label:`flux glyphs`,size:[k,k],format:`r8unorm`,usage:n.TEXTURE_BINDING|n.COPY_DST|n.RENDER_ATTACHMENT}),this.view=this.tex.createView(),this.canvas=document.createElement(`canvas`),this.canvas.width=k,this.canvas.height=k,this.canvas.hidden=!0,this.canvas.style.display=`none`,this.canvas.setAttribute(`aria-hidden`,`true`),document.body.append(this.canvas);let t=this.canvas.getContext(`2d`);if(!t)throw Error(`No 2D canvas for text.`);this.ctx=t,document.fonts?.addEventListener(`loadingdone`,()=>this.clear())}onChange(e){return this.listeners.add(e),()=>this.listeners.delete(e)}font(e,t){let n=e.size*t;this.ctx.font=`${e.weight} ${n}px ${e.family}`,this.ctx.letterSpacing=e.spacing?`${e.spacing*n}px`:`0px`}measure(e,t){return this.font(t,1),this.ctx.measureText(t.upper?e.toUpperCase():e).width}get(e,t,n,r){let i=`${t.family}|${t.size}|${t.weight}|${t.spacing??0}|${+!!t.upper}|${r}|${Math.round(n)}|${e}`,a=this.entries.get(i);if(a)return a;let o=this.ctx;this.font(t,r);let s=Math.max(1,n*r),c=t.upper?e.toUpperCase():e,l=o.measureText(c);l.width>s&&(c=j(c,s,e=>o.measureText(e).width),l=o.measureText(c));let u=t.size*r,d=Math.ceil(l.fontBoundingBoxAscent||u*.8),f=Math.ceil(l.fontBoundingBoxDescent||u*.25),p=Math.ceil(Math.min(l.width,s))+4,m=d+f+4,h=this.place(p,m);if(!h)return null;o.clearRect(h.x,h.y,p,m),o.fillStyle=`#fff`,o.textBaseline=`alphabetic`,o.fillText(c,h.x+A,h.y+A+d),this.mark(h.x,h.y,p,m);let g=this.glyph(h.x,h.y,p,m,r,(A+d)/r);return this.entries.set(i,g),g}icon(e,t,n){let r=`icon|${e}|${t}|${n}`,i=this.entries.get(r);if(i)return i;let a=Math.ceil(t*n),o=a+4,s=this.place(o,o);if(!s)return null;let c=this.ctx;c.clearRect(s.x,s.y,o,o),c.save(),c.translate(s.x+A,s.y+A),c.fillStyle=`#fff`,c.strokeStyle=`#fff`,e===`check`?(c.scale(a/16,a/16),c.lineWidth=2,c.lineCap=`round`,c.lineJoin=`round`,c.stroke(new Path2D(`M3.5 8.5l3 3 6-7`))):e===`flux`?(c.scale(a/35,a/35),c.translate(0,3.5),c.fill(new Path2D(`M2 26 L20 2 L27 2 L9 26 Z`)),c.globalAlpha=.45,c.fill(new Path2D(`M16 26 L28 10 L33 10 L21 26 Z`))):(c.scale(a/12,a/12),c.fill(new Path2D(`M6 1l1.1 3.4L10.5 5.5 7.1 6.6 6 10 4.9 6.6 1.5 5.5l3.4-1.1z`))),c.restore(),this.mark(s.x,s.y,o,o);let l=this.glyph(s.x,s.y,o,o,n,o/n);return this.entries.set(r,l),l}glyph(e,t,n,r,i,a){return{u0:e/k,v0:t/k,u1:(e+n)/k,v1:(t+r)/k,w:n/i,h:r/i,baseline:a,pad:A/i}}place(e,t){return this.fit(e,t)||(this.clear(),this.fit(e,t))}fit(e,t){if(e>k||t>k)return null;let n=Math.ceil(t/4)*4;for(let t of this.shelves)if(t.h>=n&&t.h<=n*1.35&&k-t.x>=e){let n={x:t.x,y:t.y};return t.x+=e+1,n}if(this.nextY+n>k)return null;let r={y:this.nextY,h:n,x:e+1};return this.shelves.push(r),this.nextY+=n+1,{x:0,y:r.y}}mark(e,t,n,r){let i=this.dirty;i?(i[0]=Math.min(i[0],e),i[1]=Math.min(i[1],t),i[2]=Math.max(i[2],e+n),i[3]=Math.max(i[3],t+r)):this.dirty=[e,t,e+n,t+r]}clear(){this.entries.clear(),this.shelves=[],this.nextY=0,this.ctx.clearRect(0,0,k,k),this.dirty=[0,0,k,k],this.gen++;for(let e of this.listeners)e()}flush(){let e=this.dirty;if(!e)return;this.dirty=null;let t=Math.min(k,e[2])-e[0],n=Math.min(k,e[3])-e[1];t<=0||n<=0||this.gpu.device.queue.copyExternalImageToTexture({source:this.canvas,origin:[e[0],e[1]]},{texture:this.tex,origin:[e[0],e[1]],premultipliedAlpha:!0},[t,n])}},P=new WeakMap;function F(e){let t=P.get(e.device);return t||(t=new N(e),P.set(e.device,t)),t}function I(e){let t=parseInt(e.slice(1,7),16);return[(t>>16&255)/255,(t>>8&255)/255,(t&255)/255,1]}var L=null;function R(e,t){if(L??=document.createElement(`canvas`).getContext(`2d`),!L||!e.trim())return t;L.fillStyle=`#000`,L.fillStyle=e.trim();let n=String(L.fillStyle);if(n.startsWith(`#`))return I(n);let r=/rgba?\(([^)]+)\)/.exec(n);if(!r)return t;let i=r[1].split(`,`).map(e=>Number(e.trim()));return[i[0]/255,i[1]/255,i[2]/255,Number.isFinite(i[3])?i[3]:1]}function z(e,t,n){let r=e[3]*(1-n)+t[3]*n;if(r<=0)return[0,0,0,0];let i=i=>(e[i]*e[3]*(1-n)+t[i]*t[3]*n)/r;return[i(0),i(1),i(2),r]}var B=`0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz#$%*+,-.:;=?@[]^_{|}~`,V=new Map;function H(e){let t=V.get(e);if(t!==void 0)return t;if(t=null,e.length>=6){let n=0;for(let t of e.slice(2,6)){let e=B.indexOf(t);if(e<0){n=-1;break}n=n*83+e}n>=0&&(t=[(n>>16&255)/255,(n>>8&255)/255,(n&255)/255,1])}return V.size>2e4&&V.clear(),V.set(e,t),t}var U=new Map;function W(e){let n=U.get(e.title);if(!n){let r=t(e);n=[I(r.c1),I(r.c2)],U.size>2e4&&U.clear(),U.set(e.title,n)}return n}export{W as a,y as c,s as d,o as f,p as g,l as h,z as i,O as l,g as m,H as n,F as o,u as p,I as r,M as s,R as t,_ as u};