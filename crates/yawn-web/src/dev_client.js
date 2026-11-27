(function () {
  var boot = window.__yawn || { rev: 0, error: null };
  var overlay = null;
  function hideError() { if (overlay) { overlay.remove(); overlay = null; } }
  function showError(text, title) {
    if (!overlay) {
      overlay = document.createElement('pre');
      overlay.id = '__yawn_error';
      overlay.style.cssText = 'position:fixed;left:0;right:0;bottom:0;margin:0;padding:12px 16px;max-height:40vh;overflow:auto;background:#1b0d0d;color:#ffb4b4;font:13px/1.45 ui-monospace,monospace;border-top:2px solid #ff5c5c;z-index:2147483647;white-space:pre-wrap;cursor:pointer';
      overlay.onclick = hideError;
      document.body.appendChild(overlay);
    }
    overlay.textContent = (title || 'yawn: build failed') + ' (click to hide)\n\n' + text;
  }
  if (boot.error) showError(boot.error);
  var crashed = false;
  window.__yawnPanic = function (text) { if (crashed) return; crashed = true; showError(text, 'yawn: the page crashed'); };
  try {
    var kept = sessionStorage.getItem('__yawnKept');
    if (kept) { window.__yawnKept = kept; sessionStorage.removeItem('__yawnKept'); }
  } catch (e) {}
  function reloadKeeping() {
    try {
      var m = window.__yawnModule;
      if (!window.__yawnKept && m && m.__yawn_keep) m.__yawn_keep();
      if (window.__yawnKept) sessionStorage.setItem('__yawnKept', window.__yawnKept);
    } catch (e) {}
    location.reload();
  }
  var bar = null, fill = null, note = null, progressTimer = null;
  var TITLE_MARK = '\u27f3 ';
  function markTitle(on) {
    var has = document.title.indexOf(TITLE_MARK) === 0;
    if (on && !has) document.title = TITLE_MARK + document.title;
    else if (!on && has) document.title = document.title.slice(TITLE_MARK.length);
  }
  function hideProgress() { clearTimeout(progressTimer); progressTimer = null; markTitle(false); if (bar) { bar.remove(); note.remove(); bar = fill = note = null; } }
  function showProgress(done, total, label) {
    if (!total && !label) { hideProgress(); return; }
    if (bar) { renderProgress(done, total, label); return; }
    clearTimeout(progressTimer);
    progressTimer = setTimeout(function () { renderProgress(done, total, label); }, 250);
  }
  function renderProgress(done, total, label) {
    if (!total && !label) { hideProgress(); return; }
    if (!bar) {
      var style = document.createElement('style');
      style.textContent = '@keyframes __yawn_slide{0%{margin-left:-30%}100%{margin-left:100%}}';
      bar = document.createElement('div');
      bar.id = '__yawn_progress';
      bar.style.cssText = 'position:fixed;top:0;left:0;right:0;height:3px;background:rgba(128,128,128,.25);z-index:2147483647;overflow:hidden;pointer-events:none';
      bar.appendChild(style);
      fill = document.createElement('div');
      bar.appendChild(fill);
      note = document.createElement('div');
      note.style.cssText = 'position:fixed;top:6px;right:8px;max-width:60vw;padding:2px 8px;border-radius:4px;background:rgba(20,20,20,.85);color:#e8e8e8;font:12px/1.4 ui-monospace,monospace;z-index:2147483647;pointer-events:none;white-space:nowrap;overflow:hidden;text-overflow:ellipsis';
      document.body.appendChild(bar);
      document.body.appendChild(note);
    }
    if (total) {
      fill.style.cssText = 'height:100%;background:#6ea0ff;transition:width .15s;width:' + Math.min(100, Math.round(done * 100 / total)) + '%';
    } else {
      fill.style.cssText = 'height:100%;width:30%;background:#6ea0ff;animation:__yawn_slide 1s linear infinite';
    }
    markTitle(true);
    note.textContent = 'building' + (total ? ' ' + done + '/' + total : '') + (label ? ' \u00b7 ' + label : '');
  }
  if (boot.progress) showProgress(boot.progress.done, boot.progress.total, boot.progress.label);
  function beacon(rev, kind) {
    try { fetch('/__yawn/loaded?rev=' + rev + '&kind=' + kind, { keepalive: true }); } catch (e) {}
  }
  window.__yawnLoaded = function () { beacon(boot.rev, 'reload'); };
  var stylesReady = Promise.resolve();
  function swapCss(href, rev, quiet) {
    var old = [].slice.call(document.querySelectorAll('link[rel=stylesheet]'))
      .filter(function (l) { return new URL(l.href).pathname === href; });
    if (!old.length) old = [null];
    var ready = Promise.all(old.map(function (o) {
      return new Promise(function (resolve, reject) {
        var n = o ? o.cloneNode() : document.createElement('link');
        n.rel = 'stylesheet';
        n.href = href + '?v=' + rev;
        n.onload = function () { if (o) o.remove(); resolve(); };
        n.onerror = function () { n.remove(); reject(new Error('Cannot load stylesheet: ' + href)); };
        if (o) o.after(n); else document.head.appendChild(n);
      });
    }));
    stylesReady = Promise.all([stylesReady.catch(function () {}), ready]);
    stylesReady.catch(function () {});
    ready.then(function () { if (!quiet) beacon(rev, 'css'); }).catch(function (e) { showError(e.message); });
  }
  var literals = [];
  function markup(html) {
    var t = document.createElement('template');
    t.innerHTML = html;
    if (t.content.childNodes.length !== 1) throw new Error('Invalid hot template');
    return t.content.firstChild;
  }
  function flatten(root) {
    var nodes = [];
    function walk(n) { nodes.push(n); Array.from(n.childNodes).forEach(walk); }
    walk(root);
    return nodes;
  }
  window.__yawnTrackLiteral = function (kind, original, current, node) {
    literals.push({kind: kind, original: original, current: current,
      nodes: (kind === 'template' ? flatten(node) : [node]).map(function (n) { return new WeakRef(n); })});
  };
  function attribute(node, name, before, after, changes) {
    if (before === after) return;
    changes.push(function () {
      if (name === 'class') {
        var old = (before || '').split(/\s+/).filter(Boolean);
        var next = (after || '').split(/\s+/).filter(Boolean);
        old.filter(function (c) { return !next.includes(c); }).forEach(function (c) { node.classList.remove(c); });
        next.filter(function (c) { return !old.includes(c); }).forEach(function (c) { node.classList.add(c); });
      } else if (name === 'style') {
        var a = document.createElement('span'), b = document.createElement('span');
        a.setAttribute('style', before || ''); b.setAttribute('style', after || '');
        Array.from(a.style).forEach(function (p) { if (!b.style.getPropertyValue(p)) node.style.removeProperty(p); });
        Array.from(b.style).forEach(function (p) {
          if (a.style.getPropertyValue(p) !== b.style.getPropertyValue(p) || a.style.getPropertyPriority(p) !== b.style.getPropertyPriority(p))
            node.style.setProperty(p, b.style.getPropertyValue(p), b.style.getPropertyPriority(p));
        });
      } else if (after === null) node.removeAttribute(name);
      else node.setAttribute(name, after);
    });
  }
  var patchQueue = Promise.resolve(), applied = boot.rev;
  function softReload(rev) {
    patchQueue = patchQueue.then(async function () {
      if (rev <= applied) return;
      var response = await fetch(location.href, {cache: 'no-store'});
      if (!response.ok) throw new Error('Cannot load updated page');
      var page = new DOMParser().parseFromString(await response.text(), 'text/html');
      var script = Array.from(page.querySelectorAll('script[type=module]')).find(function (s) { return s.textContent.includes('import init, * as client'); });
      var match = script && script.textContent.match(/import init, \* as client from ("(?:[^"\\]|\\.)*")/);
      if (!match) throw new Error('Updated page has no client module');
      var entry = new URL(JSON.parse(match[1]), location.href);
      var wasm = new URL('app_bg.wasm?v=' + rev, entry);
      var loaded = await Promise.all([
        import(entry.href),
        fetch(wasm, {cache: 'no-store'}).then(function (r) { if (!r.ok) throw new Error('Cannot load updated WASM'); return r.arrayBuffer(); }).then(function (bytes) { return WebAssembly.compile(bytes); }),
        fetch('/__yawn/literals.json?v=' + rev, {cache: 'no-store'}).then(function (r) { if (!r.ok) throw new Error('Cannot load dev literals'); return r.json(); }),
        Promise.all(Array.from(page.querySelectorAll('link[data-yawn-boot]'), async function (link) {
          var r = await fetch(link.getAttribute('href'));
          if (!r.ok) throw new Error('Cannot load startup data');
          return [link.dataset.yawnBoot, await r.text()];
        }))
      ]);
      Array.from(page.querySelectorAll('link[rel=stylesheet]')).forEach(function (link) { swapCss(new URL(link.getAttribute('href'), location.href).pathname, rev, true); });
      await stylesReady;
      var module = loaded[0], previous = window.__yawnModule;
      if (!previous || !previous.__yawn_dispose || !module.__yawn_dispose) throw new Error('Client cannot be replaced');
      var x = scrollX, y = scrollY, active = document.activeElement;
      var id = active && active.id;
      previous.__yawn_dispose();
      literals = [];
      window.__yawnLiterals = loaded[2];
      window.__yawnBoot = Object.fromEntries(loaded[3]);
      window.__yawnReplaced = 1;
      await module.default({module_or_path: loaded[1]});
      window.__yawnModule = module;
      boot.rev = applied = rev;
      scrollTo(x, y);
      if (id) { var focus = document.getElementById(id); if (focus) focus.focus({preventScroll: true}); }
      hideError(); hideProgress(); beacon(rev, 'client');
    }).catch(function (error) { console.warn('yawn: client replacement failed', error); reloadKeeping(); });
  }
  function patch(rev) {
    patchQueue = patchQueue.then(async function () {
      var r = await fetch('/__yawn/literals.json?v=' + rev, {cache: 'no-store'});
      if (!r.ok) throw new Error('Cannot load dev literals');
      var values = await r.json(), changes = [], updates = [];
      literals = literals.filter(function (entry) { return entry.nodes.some(function (n) { return n.deref(); }); });
      literals.forEach(function (entry) {
        var key = entry.kind + ':' + entry.original;
        var next = Object.prototype.hasOwnProperty.call(values, key) ? values[key] : entry.original;
        if (entry.current === next) return;
        if (entry.kind === 'template') {
          var before = flatten(markup(entry.current)), after = flatten(markup(next));
          if (before.length !== after.length || before.length !== entry.nodes.length) throw new Error('Hot template structure changed');
          before.forEach(function (old, i) {
            var fresh = after[i], node = entry.nodes[i].deref();
            if (old.nodeType !== fresh.nodeType || old.nodeName !== fresh.nodeName || old.childNodes.length !== fresh.childNodes.length)
              throw new Error('Hot template structure changed');
            if (!node) return;
            if (old.nodeType === Node.TEXT_NODE && old.nodeValue !== fresh.nodeValue)
              changes.push(function () { node.nodeValue = fresh.nodeValue; });
            if (old.nodeType === Node.ELEMENT_NODE) {
              var names = new Set(Array.from(old.attributes, function (a) { return a.name; }).concat(Array.from(fresh.attributes, function (a) { return a.name; })));
              names.forEach(function (name) { attribute(node, name, old.getAttribute(name), fresh.getAttribute(name), changes); });
            }
          });
        } else {
          var node = entry.nodes[0].deref();
          if (node && entry.kind === 'text') changes.push(function () { node.nodeValue = next; });
          else if (node && entry.kind.startsWith('attr:')) attribute(node, entry.kind.slice(5), entry.current, next, changes);
        }
        updates.push(function () { entry.current = next; });
      });
      await stylesReady;
      var all = window.__yawnLiterals;
      Object.keys(all).forEach(function (key) { delete all[key]; });
      Object.assign(all, values);
      changes.forEach(function (change) { change(); });
      updates.forEach(function (update) { update(); });
      applied = rev;
      hideError(); hideProgress(); beacon(rev, 'patch');
    }).catch(function (error) { console.warn('yawn: hot update requires client replacement', error); setTimeout(function () { softReload(rev); }, 0); });
  }
  var seen = boot.rev, down = false;
  function poll() {
    fetch('/__yawn/rev', { cache: 'no-store' }).then(function (r) { return r.ok ? r.text() : null; }).then(function (t) {
      if (t === null) { down = true; return; }
      var rev = parseInt(t, 10);
      if (rev > seen || (down && rev !== seen)) { seen = rev; softReload(rev); }
      down = false;
    }).catch(function () { down = true; });
  }
  setInterval(function () { if (!document.hidden) poll(); }, 1000);
  document.addEventListener('visibilitychange', function () { if (!document.hidden) poll(); });
  function connect() {
    var es = new EventSource('/__yawn/events');
    es.onopen = function () { if (down) poll(); };
    es.onmessage = function (e) {
      var m = JSON.parse(e.data);
      if (m.kind !== 'progress' && m.rev > seen) seen = m.rev;
      if (m.kind === 'reload') { softReload(m.rev); }
      else if (m.kind === 'patch') { patch(m.rev); }
      else if (m.kind === 'css') { hideProgress(); swapCss(m.href, m.rev); }
      else if (m.kind === 'error') { hideProgress(); showError(m.text); }
      else if (m.kind === 'progress') showProgress(m.done, m.total, m.label);
    };
    es.onerror = function () { down = true; es.close(); setTimeout(connect, 50); };
  }
  connect();
})();
