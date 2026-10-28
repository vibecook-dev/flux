import{r as e}from"./react-DHsaPJjs.js";import{kitModule as t}from"./runtime-Caub7c_G.js";var n=e({CORKBOARD:()=>s,EXAMPLES:()=>c,HELIX:()=>i,STRIP:()=>o,TIMELINE:()=>a,TONIGHT:()=>r,exampleLens:()=>l}),r={name:`Tonight`,blurb:`One title at a time, the rest waiting behind it. For nights you can’t decide.`,input:`x`,step:240,css:`
.lens .item { width: min(36vh, 290px); }
.lens .item .art { width: 100%; border-radius: 16px; box-shadow: 0 40px 80px -30px rgb(0 0 0 / .75), 0 0 0 .5px rgb(255 255 255 / .08); }
.lens .cap { position: absolute; left: -40%; right: -40%; top: calc(100% + 22px); text-align: center; opacity: 0; transition: opacity .35s; }
.lens .item.is-focus .cap { opacity: 1; }
.lens .cap b { display: block; font-weight: 400; font-size: 1.375rem; letter-spacing: -.01em; color: var(--text); }
.lens .cap span { display: block; margin-top: 4px; font-family: var(--font-mono); font-size: .75rem; color: var(--text-3); }
`,item:`<div class="art"></div><div class="cap"><b>{{title}}</b><span>{{meta}}</span></div>`,layout:`function layout(items, view) {
  const lift = -view.h * 0.05;
  return items.map((it) => {
    const d = it.i - view.pos;
    if (d < -1.3 || d > 6) return { o: 0 };
    // Passed titles slide away to the left; the next ones wait in a loose stack.
    if (d < 0) return { x: d * view.w * 0.55, y: lift - d * 30, z: 20, rz: d * 12, o: 1 + d * 0.85 };
    return { x: d * 24, y: lift - d * 14, z: -d * 70, s: 1 - d * 0.035, rz: d * 1.5, o: 1 - d * 0.13 };
  });
}`},i={name:`Helix`,blurb:`Your library wound into a slow spiral. Scroll to turn it.`,input:`y`,step:80,css:`
.lens .kit-stage { perspective: 1600px; }
.lens .item { width: clamp(92px, 11vw, 140px); }
.lens .item .art { width: 100%; border-radius: 9px; box-shadow: 0 18px 40px -18px rgb(0 0 0 / .7); }
.lens .cap { position: absolute; left: 50%; top: calc(100% + 14px); translate: -50% 0; width: max-content; max-width: 280px; text-align: center; opacity: 0; transition: opacity .3s; pointer-events: none; }
.lens .item.is-focus .cap { opacity: 1; }
.lens .cap b { display: block; font-weight: 500; font-size: 1rem; color: var(--text); }
.lens .cap span { font-family: var(--font-mono); font-size: .75rem; color: var(--text-3); }
`,item:`<div class="art"></div><div class="cap"><b>{{title}}</b><span>{{meta}}</span></div>`,layout:`function layout(items, view) {
  const R = Math.min(view.w * 0.26, 330);
  const pitch = view.phone ? 34 : 46;
  return items.map((it) => {
    const d = it.i - view.pos;
    const ad = Math.abs(d);
    if (ad > 13) return { o: 0 };
    const a = d * 0.62;
    const front = Math.cos(a);
    // The far side of the spiral recedes into the room; only the near turn is bright.
    const o = clamp((front + 0.35) / 1.35, 0.06, 1) * clamp(1 - (ad - 8) / 5, 0, 1);
    return { x: Math.sin(a) * R, y: d * pitch, z: (front - 1) * R, ry: Math.sin(a) * -30, o, s: ad < 0.5 ? 1.12 : 0.96 };
  });
}`},a={name:`Timeline`,blurb:`Every title on one line of years, gaps included. Titles from the same year pile up.`,input:`x`,step:90,window:120,css:`
.lens .axis { position: absolute; left: 0; right: 0; top: 66%; height: 1px; background: linear-gradient(90deg, transparent, var(--text-3) 10%, var(--text-3) 90%, transparent); }
.lens .now { position: absolute; left: 50%; top: calc(66% - 8px); width: 1px; height: 17px; background: var(--text); }
.lens .item { width: clamp(58px, 6vw, 88px); }
.lens .item .art { width: 100%; border-radius: 5px; box-shadow: 0 12px 26px -12px rgb(0 0 0 / .75), 0 0 0 .5px rgb(255 255 255 / .1); }
.lens .yr { position: absolute; left: 50%; top: calc(100% + 12px); translate: -50% 0; font-family: var(--font-mono); font-size: .6875rem; color: var(--text-3); opacity: 0; }
.lens .item.first .yr { opacity: 1; }
.lens .item.is-focus .yr { opacity: 1; color: var(--text); }
.lens .ttl { position: absolute; left: 50%; bottom: calc(100% + 14px); translate: -50% 0; width: max-content; max-width: 260px; text-align: center; font-size: 1rem; font-weight: 500; color: var(--text); opacity: 0; transition: opacity .3s; }
.lens .item.is-focus .ttl { opacity: 1; }
`,scene:`<div class="axis"></div><div class="now"></div>`,item:`<div class="ttl">{{title}}</div><div class="art"></div><span class="yr">{{year}}</span>`,layout:`function layout(items, view) {
  // The year under the position, eased between the two titles either side of it.
  const at = (i) => items.find((x) => x.i === i);
  const i0 = Math.floor(view.pos);
  const a = at(i0) || at(Math.round(view.pos));
  const b = at(i0 + 1) || a;
  if (!a) return items.map(() => ({ o: 0 }));
  const ya = a.year || 0, yb = (b && b.year) || ya;
  const fy = lerp(ya, yb, view.pos - i0);
  const per = view.phone ? 70 : 104;
  const art = view.phone ? 58 : Math.min(88, Math.max(58, view.w * 0.06));
  const axis = view.h * 0.16;
  const stack = {};
  const focus = Math.round(view.pos);
  return items.map((it) => {
    if (!it.year) return { o: 0 };
    const k = (stack[it.year] = (stack[it.year] === undefined ? -1 : stack[it.year]) + 1);
    const isFocus = it.i === focus;
    if (k > 5 && !isFocus) return { o: 0 };
    const x = (it.year - fy) * per + Math.min(k, 5) * 5;
    if (Math.abs(x) > view.w * 0.6) return { o: 0 };
    // Each year is a pile standing on the axis; the focused title steps forward.
    const y = axis - art * 0.75 - Math.min(k, 5) * art * 0.62 - (isFocus ? 18 : 0);
    return { x, y, z: isFocus ? 90 : -k * 6, s: isFocus ? 1.22 : 1, o: 1 - Math.min(0.7, Math.abs(x) / (view.w * 0.6)), cls: k === 0 ? 'first' : '', zIndex: isFocus ? 4000 : 1000 + k };
  });
}`},o={name:`Film strip`,blurb:`Posters as frames on a reel of 35 mm, edge codes and all.`,input:`x`,step:150,css:`
.lens .item { width: clamp(132px, 15vw, 214px); }
.lens .frame { position: relative; padding: 30px 10px; border-radius: 3px; background-color: #111;
  background-image: repeating-linear-gradient(90deg, transparent 0 9px, rgb(233 228 216 / .82) 9px 19px, transparent 19px 28px), repeating-linear-gradient(90deg, transparent 0 9px, rgb(233 228 216 / .82) 9px 19px, transparent 19px 28px);
  background-size: 100% 12px, 100% 12px; background-position: 0 9px, 0 calc(100% - 9px); background-repeat: repeat-x, repeat-x;
  box-shadow: 0 30px 60px -30px rgb(0 0 0 / .8); }
.lens .frame .art { width: 100%; border-radius: 2px; }
.lens .code { position: absolute; left: 12px; bottom: 30px; translate: 0 100%; padding-top: 2px; font-family: var(--font-mono); font-size: .5625rem; letter-spacing: .12em; color: #e8a33a; white-space: nowrap; }
.lens .ttl { position: absolute; left: -30%; right: -30%; top: calc(100% + 18px); text-align: center; font-size: 1rem; font-weight: 500; color: var(--text); opacity: 0; transition: opacity .3s; }
.lens .item.is-focus .ttl { opacity: 1; }
`,item:`<div class="frame"><div class="art"></div><span class="code">FLUX 5247 ▸ {{number}}A</span></div><div class="ttl">{{title}} · {{year}}</div>`,layout:`function layout(items, view) {
  const fw = view.phone ? 150 : 228;
  const k = 0.115;
  const R = fw / k;
  return items.map((it) => {
    const a = (it.i - view.pos) * k;
    if (Math.abs(a) > 1.35) return { o: 0 };
    // The strip wraps round a drum in front of you.
    return { x: Math.sin(a) * R, z: (Math.cos(a) - 1) * R, ry: a * 57.3, y: -view.h * 0.03 };
  });
}`},s={name:`Corkboard`,blurb:`Posters pinned up a little crooked, the way they were in your first flat.`,input:`x`,step:120,window:60,css:`
.lens .kit-scene { background:
  radial-gradient(circle at 20% 30%, rgb(255 255 255 / .04) 0 1px, transparent 1px) 0 0 / 7px 7px,
  radial-gradient(circle at 70% 60%, rgb(0 0 0 / .12) 0 1px, transparent 1.5px) 0 0 / 5px 5px,
  linear-gradient(160deg, color-mix(in oklab, #a7764a 34%, var(--bg)), color-mix(in oklab, #6f4a2c 30%, var(--bg))); }
.lens .item { width: clamp(110px, 12.5vw, 176px); }
.lens .item .art { width: 100%; border-radius: 2px; box-shadow: 0 2px 3px rgb(0 0 0 / .3), 0 18px 30px -16px rgb(0 0 0 / .6); }
.lens .pin { position: absolute; left: 50%; top: -6px; translate: -50% 0; z-index: 5; width: 13px; height: 13px; border-radius: 50%;
  background: radial-gradient(circle at 35% 30%, #fff8 0 2px, transparent 3px), hsl(calc(var(--i) * 67) 70% 52%); box-shadow: 0 3px 4px rgb(0 0 0 / .45); }
.lens .item.is-focus .art { box-shadow: 0 4px 8px rgb(0 0 0 / .3), 0 40px 60px -24px rgb(0 0 0 / .7); }
`,item:`<i class="pin"></i><div class="art"></div>`,layout:`function layout(items, view) {
  const rows = view.h > 760 ? 3 : 2;
  const cw = view.phone ? 140 : 214, ch = view.phone ? 214 : 286;
  const fcol = view.pos / rows;
  const mid = (view.safe.top - view.safe.bottom) / 2;
  const focus = Math.round(view.pos);
  return items.map((it) => {
    const col = Math.floor(it.i / rows), row = it.i % rows;
    const isFocus = it.i === focus;
    const x = (col - fcol) * cw + (rand(it.i, 1) - 0.5) * 36;
    if (Math.abs(x) > view.w * 0.8) return { o: 0 };
    const y = (row - (rows - 1) / 2) * ch + (rand(it.i, 2) - 0.5) * 28 + mid;
    return { x, y, z: isFocus ? 70 : 0, rz: isFocus ? 0 : (rand(it.i, 3) - 0.5) * 11, s: isFocus ? 1.07 : 1, zIndex: isFocus ? 9000 : 4000 + (it.i - focus) };
  });
}`},c={tonight:r,helix:i,timeline:a,strip:o,corkboard:s};function l(e){let n=c[e];return t(n??null,e)}export{n,r as t};