// Loads the side modules `yawn dev` built for routes (see `[dev] dynamic`). The app module is an
// ordinary build; a side module is position independent and refers to the app's own functions and
// data by name, so everything it needs is taken from the app's exports.

const STACK = 1 << 20;

function leb(d, i) {
  let r = 0, s = 0;
  for (;;) {
    const b = d[i++];
    r |= (b & 0x7f) << s;
    s += 7;
    if (!(b & 0x80)) return [r >>> 0, i];
  }
}

function dylinkInfo(bytes) {
  let i = 8;
  while (i < bytes.length) {
    const id = bytes[i++];
    let n;
    [n, i] = leb(bytes, i);
    const end = i + n;
    if (id === 0) {
      let l, j;
      [l, j] = leb(bytes, i);
      if (new TextDecoder().decode(bytes.subarray(j, j + l)) === 'dylink.0') {
        j += l;
        while (j < end) {
          const sub = bytes[j++];
          let size;
          [size, j] = leb(bytes, j);
          if (sub === 1) {
            let k = j, a, b, c, e;
            [a, k] = leb(bytes, k);
            [b, k] = leb(bytes, k);
            [c, k] = leb(bytes, k);
            [e, k] = leb(bytes, k);
            return { memorySize: a, memoryAlign: b, tableSize: c, tableAlign: e };
          }
          j += size;
        }
      }
    }
    i = end;
  }
  throw new Error('not a position-independent module (no dylink.0)');
}

const align = (n, a) => Math.ceil(n / a) * a;

function globals(module, name) {
  const out = {};
  for (const im of WebAssembly.Module.imports(module)) {
    if (im.module === name) out[im.name] = new WebAssembly.Global({ value: 'i32', mutable: true }, 0);
  }
  return out;
}

async function loadSide(main, url, wbg) {
  const ex = main.exports;
  const bytes = new Uint8Array(await (await fetch(url, { cache: 'no-store' })).arrayBuffer());
  const module = await WebAssembly.compile(bytes);
  const info = dylinkInfo(bytes);
  const table = ex.__indirect_function_table;
  const memoryBase = align(ex.__yawn_alloc(info.memorySize + (1 << info.memoryAlign), 16), 1 << info.memoryAlign);
  const stackTop = ex.__yawn_alloc(STACK, 16) + STACK;
  const tableBase = info.tableSize ? table.grow(info.tableSize) : table.length;
  const env = {
    memory: ex.memory,
    __stack_pointer: new WebAssembly.Global({ value: 'i32', mutable: true }, stackTop),
    __indirect_function_table: table,
    __memory_base: new WebAssembly.Global({ value: 'i32', mutable: false }, memoryBase),
    __table_base: new WebAssembly.Global({ value: 'i32', mutable: false }, tableBase),
  };
  const funcs = globals(module, 'GOT.func');
  const mems = globals(module, 'GOT.mem');
  for (const im of WebAssembly.Module.imports(module)) {
    if (im.module === 'env' && im.kind === 'function') {
      const f = ex[im.name];
      if (typeof f !== 'function') throw new Error('the app has no `' + im.name + '`');
      env[im.name] = f;
    }
  }
  const placeholder = {};
  for (const im of WebAssembly.Module.imports(module)) {
    if (im.module !== '__wbindgen_placeholder__') continue;
    const f = wbg && wbg[im.name];
    if (typeof f === 'function') placeholder[im.name] = f;
    else if (im.name.startsWith('__wbindgen_describe')) placeholder[im.name] = () => {};
    else throw new Error('the app has no `' + im.name + '`');
  }
  const instance = await WebAssembly.instantiate(module, { env, 'GOT.func': funcs, 'GOT.mem': mems, __wbindgen_placeholder__: placeholder });
  const own = instance.exports;
  for (const [name, g] of Object.entries(funcs)) {
    const f = own[name] ?? ex[name];
    if (typeof f !== 'function') throw new Error('unresolved function ' + name);
    g.value = table.grow(1);
    table.set(g.value, f);
  }
  for (const [name, g] of Object.entries(mems)) {
    if (own[name] instanceof WebAssembly.Global) g.value = memoryBase + own[name].value;
    else if (ex[name] instanceof WebAssembly.Global) g.value = ex[name].value;
    else throw new Error('unresolved data ' + name);
  }
  own.__wasm_apply_data_relocs?.();
  own.__wasm_call_ctors?.();
  for (const [name, f] of Object.entries(own)) {
    if (typeof f === 'function' && name.includes('__yawn_register')) f();
  }
}

globalThis.__yawn_sides = async (instance, baseUrl, imports) => {
  let wbg = null;
  for (const v of Object.values(imports ?? {})) {
    if (v && typeof v === 'object' && (!wbg || Object.keys(v).length > Object.keys(wbg).length)) wbg = v;
  }
  let manifest;
  try {
    const res = await fetch(new URL('./dyn/manifest.json', baseUrl), { cache: 'no-store' });
    if (!res.ok) return;
    manifest = await res.json();
  } catch {
    return;
  }
  for (const entry of manifest.modules ?? []) {
    try {
      await loadSide(instance, new URL('./dyn/' + entry.url, baseUrl), wbg);
    } catch (e) {
      console.error('yawn: could not load', entry.url, e);
    }
  }
};
