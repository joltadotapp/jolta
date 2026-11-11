// The homepage: a sky that follows the visitor's clock and theme, and a live
// miniature of Jolta's main window. All mail in the window is invented.
(function () {
  "use strict";

  // ---- Colour, from apple/Jolta/Scenery.swift ------------------------------

  var HUES = { blue: "#5FA8E8", cyan: "#8FD0F0", green: "#8DBF8A", magenta: "#8E6AA8", orange: "#F08A5D" };
  var LIGHT = {
    dawn: { top: "#5B6EA8", bottom: "#F2B48A", far: "#6E6A8E", near: "#48466A", ink: "#FFFFFF" },
    day: { top: "#5FA8E8", bottom: "#CFE8F7", far: "#8DBF8A", near: "#5F9A63", ink: "#17304A" },
    dusk: { top: "#2E3A78", bottom: "#F08A5D", far: "#6B4F7A", near: "#3B2E52", ink: "#FFFFFF" },
    night: { top: "#0B1026", bottom: "#26305A", far: "#1C2244", near: "#121630", ink: "#FFFFFF" },
  };
  function hex(h) { return [1, 3, 5].map(function (i) { return parseInt(h.slice(i, i + 2), 16); }); }
  function mix(a, b, t) {
    var x = hex(a), y = hex(b);
    return "#" + x.map(function (v, i) { return Math.round(v + (y[i] - v) * t).toString(16).padStart(2, "0"); }).join("");
  }
  // Graphite Dark keeps the sky dark all day: each hue is mixed only a little
  // into the theme's background (#121214) and its deepest tone (#0C0C0E).
  var DARK = (function () {
    var bg = "#121214", deep = "#0C0C0E";
    function sky(top, t, bottom, b, hill, h) {
      var sky = { top: mix(deep, top, t), bottom: mix(bg, bottom, b), far: mix(bg, hill, h), near: mix(deep, hill, h * 0.45), ink: "#ECECEE" };
      // The page's hill planes need steps you can see in the dark: the far
      // ridge lifts into the haze of the sky, the near ridge stays darkest.
      sky.planeFar = mix(sky.bottom, hill, 0.32);
      sky.planeNear = mix(deep, hill, h * 0.45);
      sky.planeMid = mix(sky.planeFar, sky.planeNear, 0.5);
      return sky;
    }
    return {
      dawn: sky(HUES.blue, 0.12, HUES.orange, 0.24, HUES.magenta, 0.2),
      day: sky(HUES.blue, 0.22, HUES.cyan, 0.2, HUES.green, 0.22),
      dusk: sky(HUES.magenta, 0.12, HUES.orange, 0.3, HUES.magenta, 0.18),
      night: sky(HUES.blue, 0.04, HUES.blue, 0.13, HUES.blue, 0.12),
    };
  })();
  function phaseFor(hour) {
    if (hour >= 5 && hour < 8) return "dawn";
    if (hour >= 8 && hour < 17) return "day";
    if (hour >= 17 && hour < 20) return "dusk";
    return "night";
  }
  function luminance(h) {
    return hex(h).map(function (v) { v /= 255; return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4); })
      .reduce(function (sum, c, i) { return sum + c * [0.2126, 0.7152, 0.0722][i]; }, 0);
  }
  function contrast(a, b) { var x = luminance(a), y = luminance(b); return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05); }
  // Moves colour c lighter or darker, whichever it already leans, until it
  // stands at least min:1 from the colour behind it.
  function apart(c, from, min) {
    var toward = luminance(c) >= luminance(from) ? "#FFFFFF" : "#000000";
    for (var k = 0; k <= 1; k += 0.02) { var m = mix(c, toward, k); if (contrast(m, from) >= min) return m; }
    return toward;
  }
  // Moves colour c toward `toward` until it stands at least min:1 from c.
  function step(c, toward, min) {
    for (var k = 0; k <= 1; k += 0.02) { var m = mix(c, toward, k); if (contrast(m, c) >= min) return m; }
    return toward;
  }
  function isDark() { return window.joltaTheme ? window.joltaTheme.isDark() : matchMedia("(prefers-color-scheme: dark)").matches; }

  // Stars scattered at random, kept apart so they never clump or line up.
  // A fixed seed keeps the same sky on every visit.
  function scatter(count, seed, minGap) {
    var s = seed >>> 0;
    function rnd() { s = (s + 0x6D2B79F5) >>> 0; var t = s; t = Math.imul(t ^ (t >>> 15), t | 1); t ^= t + Math.imul(t ^ (t >>> 7), t | 61); return ((t ^ (t >>> 14)) >>> 0) / 4294967296; }
    var out = [], tries = 0;
    while (out.length < count && tries < count * 40) {
      tries++;
      var x = rnd(), y = Math.pow(rnd(), 1.35);
      if (out.some(function (p) { return Math.hypot(p.x - x, (p.y - y) * 0.6) < minGap; })) continue;
      var big = rnd();
      out.push({ x: x, y: y, r: big > 0.93 ? 1.6 : big > 0.7 ? 1.1 : 0.75, o: 0.35 + rnd() * 0.6 });
    }
    return out;
  }
  var STARS = scatter(110, 20261111, 0.035);

  // ---- The miniature window ------------------------------------------------

  var MAIL = [
    { from: "Ines Marchetti", n: 3, subj: "Harbour lighting: revised quote", line: "Attached the v3 numbers. The fixture count drops from 58 to 46", time: "09:32", tag: "Clients", acct: "Google Workspace", unread: true },
    { from: "Tomás Okafor", n: 2, subj: "Offsite in Ghent: both floors confirmed", line: "The venue holds the second floor until Friday", time: "09:10", tag: "Team", acct: "Microsoft 365", unread: true },
    { from: "Relay CI", subj: "main failed: build #4821", line: "test_sync_resume timed out after 30 s", time: "08:57", tag: "CI", acct: "your IMAP server", unread: true },
    { from: "Hana Sato", n: 4, subj: "Contract redlines, section 7", line: "I accepted your changes except 7.3", time: "08:41", tag: "Legal", acct: "Google Workspace" },
    { from: "Jonas Lindqvist", subj: "Question about API rate limits", line: "We're seeing 429s around 09:00 every day", time: "08:15", acct: "Microsoft 365", unread: true },
    { from: "Northwind Finance", subj: "Invoice NW-2026-1043 due Friday", line: "Please find the invoice for October attached", time: "07:58", tag: "Invoices", acct: "Google Workspace" },
    { from: "The Weekly Pour", subj: "Issue 212: cold brew and the quiet return of the index card", line: "This week we visit three roasters", time: "07:30", tag: "Newsletter", acct: "iCloud" },
    { from: "Bram De Smet", n: 2, subj: "Lunch Thursday?", line: "There's a new ramen place near the station", time: "18:22", day: "Yesterday", acct: "iCloud" },
  ];
  var BOLT_PATH = "M25 1 5 37h14l-6 26 23-38H22l8-24z";
  var uid = 0;
  function esc(s) { return String(s).replace(/[&<>"]/g, function (c) { return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]; }); }
  function boltSVG(h) {
    var id = "jb" + uid++;
    return '<svg width="' + h * 0.62 + '" height="' + h + '" viewBox="0 0 40 64" aria-hidden="true"><defs><linearGradient id="' + id + '" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#FFE066"/><stop offset=".55" stop-color="#FFC020"/><stop offset="1" stop-color="#FF8A00"/></linearGradient></defs><path d="' + BOLT_PATH + '" fill="url(#' + id + ')" stroke="#C86A00" stroke-width="1.2" stroke-linejoin="round"/></svg>';
  }
  function sceneSVG(sky, showStars) {
    var id = "js" + uid++;
    var stars = showStars ? STARS.slice(0, 50).map(function (p) {
      return '<circle cx="' + (p.x * 1000).toFixed(1) + '" cy="' + (p.y * 330).toFixed(1) + '" r="' + p.r * 1.3 + '" fill="#fff" opacity="' + p.o.toFixed(2) + '"/>';
    }).join("") : "";
    return '<svg class="jw-scene" viewBox="0 0 1000 600" preserveAspectRatio="xMidYMax slice" aria-hidden="true"><defs><linearGradient id="' + id + '" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="' + sky.top + '"/><stop offset="1" stop-color="' + sky.bottom + '"/></linearGradient></defs><rect width="1000" height="600" fill="url(#' + id + ')"/>' + stars +
      '<path d="M0 470C160 400 330 380 520 420S860 470 1000 430V600H0z" fill="' + sky.far + '"/><path d="M0 540C210 480 420 470 640 515S900 560 1000 540V600H0z" fill="' + sky.near + '"/></svg>';
  }

  function mountWindow(root, opts) {
    var rows = MAIL.slice(), cur = 0, query = "", undo = [], snoozed = [], toastTimer = 0, onScreen = false;
    // Snoozing in the demo wakes the message tomorrow at 09:00, shown the way
    // the app's "Next to wake" list does: a short weekday and the time.
    var wakeAt = new Date(); wakeAt.setDate(wakeAt.getDate() + 1); wakeAt.setHours(9, 0, 0, 0);
    var wakeLabel = wakeAt.toLocaleDateString("en-GB", { weekday: "short" }) + " 09:00";
    root.classList.add("jw");
    root.tabIndex = 0;
    root.setAttribute("role", "group");
    root.setAttribute("aria-roledescription", "Jolta demo");
    root.setAttribute("aria-label", "Jolta inbox demo with invented mail. J and K move, E archives, H snoozes, Z undoes, slash searches.");
    root.innerHTML =
      '<aside class="jw-side" aria-hidden="true"><div class="jw-lights"><i></i><i></i><i></i></div>' +
      '<div class="jw-nav jw-on"><b>Inbox</b><span class="jw-n"></span></div><div class="jw-nav">Starred</div>' +
      '<div class="jw-group">Mailboxes</div><div class="jw-nav">Snoozed<span class="jw-sn"></span></div><div class="jw-nav">Drafts</div><div class="jw-nav">Sent</div>' +
      '<div class="jw-wake"></div>' +
      '<div class="jw-group jw-accts-label">Accounts<span class="jw-fold"></span></div><div class="jw-accts"><div class="jw-acct">Work<small>Google</small></div><div class="jw-acct">Studio<small>Microsoft 365</small></div><div class="jw-acct">Personal<small>iCloud</small></div><div class="jw-acct">Server<small>IMAP</small></div></div>' +
      '<div class="jw-sync">All accounts synced</div></aside>' +
      '<section class="jw-main"><div class="jw-head"><h3>Inbox</h3><span class="jw-count"></span><label class="jw-search"><input type="text" placeholder="Search" aria-label="Search the demo mail" tabindex="-1"><kbd>/</kbd></label></div>' +
      '<ul class="jw-list" aria-label="Messages"><li class="jw-zero" aria-hidden="true"></li></ul></section>' +
      '<div class="jw-foot" aria-hidden="true"><span><kbd>J</kbd><kbd>K</kbd> move</span><span><kbd>E</kbd> archive</span><span class="jw-opt"><kbd>H</kbd> snooze</span><span class="jw-opt"><kbd>Z</kbd> undo</span><span class="jw-cmd"><kbd>⌘K</kbd> commands</span></div>' +
      '<div class="jw-toast" role="status" aria-live="polite"></div>';
    var list = root.querySelector(".jw-list"), zero = root.querySelector(".jw-zero");
    var input = root.querySelector("input"), toast = root.querySelector(".jw-toast");

    function say(html, ms) {
      toast.innerHTML = html; toast.classList.add("jw-show");
      clearTimeout(toastTimer);
      toastTimer = setTimeout(function () { toast.classList.remove("jw-show"); }, ms || 1800);
    }
    function visible() {
      var q = query.trim().toLowerCase();
      if (!q) return rows;
      if (q.indexOf("from:") === 0) return rows.filter(function (m) { return m.from.toLowerCase().indexOf(q.slice(5)) >= 0; });
      return rows.filter(function (m) { return (m.from + " " + m.subj + " " + m.line + " " + (m.tag || "")).toLowerCase().indexOf(q) >= 0; });
    }
    function render() {
      var vis = visible();
      if (cur >= vis.length) cur = Math.max(0, vis.length - 1);
      Array.prototype.slice.call(list.querySelectorAll(".jw-row, .jw-day")).forEach(function (n) { n.remove(); });
      var dayShown = false;
      vis.forEach(function (m, i) {
        if (m.day && !dayShown) {
          dayShown = true;
          var d = document.createElement("li"); d.className = "jw-day"; d.textContent = m.day; d.setAttribute("aria-hidden", "true");
          list.insertBefore(d, zero);
        }
        var li = document.createElement("li");
        li.className = "jw-row" + (m.unread ? " jw-unread" : "") + (i === cur ? " jw-cur" : "");
        if (i === cur) li.setAttribute("aria-current", "true");
        li.innerHTML = '<span class="jw-dot"></span><span class="jw-from">' + esc(m.from) + (m.n ? ' <span class="jw-num">' + m.n + "</span>" : "") +
          '</span><span class="jw-line"><span class="jw-subj">' + esc(m.subj) + "</span>&ensp;" + esc(m.line) + '</span><span class="jw-meta">' +
          (m.tag ? '<span class="jw-tag">' + esc(m.tag) + "</span>" : "") + '<span class="jw-time">' + m.time + '</span><span class="jw-keys"><kbd>E</kbd><kbd>H</kbd><kbd>R</kbd></span></span>';
        li.addEventListener("click", function () { cur = i; render(); root.focus({ preventScroll: true }); });
        list.insertBefore(li, zero);
      });
      var unread = rows.filter(function (m) { return m.unread; }).length;
      root.querySelector(".jw-count").textContent = query ? vis.length + " found" : unread ? unread + " unread" : "";
      root.querySelector(".jw-n").textContent = rows.length || "";
      list.classList.toggle("jw-empty", rows.length === 0);
      var wake = root.querySelector(".jw-wake");
      wake.innerHTML = snoozed.length ? '<div class="jw-group">Next to wake</div>' + snoozed.slice(0, 5).map(function (m) {
        return '<div class="jw-woke"><span class="jw-at">' + wakeLabel + "</span><span>" + esc(m.subj) + "</span></div>";
      }).join("") : "";
      root.querySelector(".jw-sn").textContent = snoozed.length || "";
      // Like the app's collapsible Accounts group, it folds away when the
      // snoozed list needs the room.
      var folded = snoozed.length > 1;
      root.querySelector(".jw-accts").hidden = folded;
      root.querySelector(".jw-fold").textContent = folded && unread ? unread : "";
      if (rows.length === 0) {
        var sky = opts.sky();
        zero.innerHTML = sceneSVG(sky, isDark() || opts.phase() !== "day") +
          '<div class="jw-words">' + boltSVG(44) + "<strong>Inbox zero</strong><span>Nothing left. Close the lid.</span></div>";
        // Set through the style object: the site's CSP blocks inline style attributes.
        zero.querySelector(".jw-words").style.color = sky.ink;
      }
    }
    function act(type) {
      if (type === "undo") {
        var last = undo.pop(); if (!last) return;
        if (last.type === "snooze") snoozed.splice(snoozed.indexOf(last.m), 1);
        rows.splice(last.at, 0, last.m); query = ""; input.value = "";
        cur = rows.indexOf(last.m); render(); say("Moved back to Inbox");
        opts.onAction && opts.onAction("undo"); return;
      }
      var m = visible()[cur]; if (!m) return;
      var at = rows.indexOf(m);
      rows.splice(at, 1); undo.push({ m: m, at: at, type: type });
      if (type === "snooze") snoozed.unshift(m);
      render();
      say(type === "archive" ? "Archived on " + m.acct + " &middot; <kbd>Z</kbd> undo" : "Snoozed until " + wakeLabel + " &middot; <kbd>Z</kbd> undo");
      opts.onAction && opts.onAction(type);
      if (rows.length === 0 && opts.onZero) opts.onZero();
    }
    function onKey(e) {
      if (e.metaKey || e.ctrlKey || e.altKey) return;
      if (document.activeElement === input) {
        if (e.key === "Escape" || e.key === "Enter") { input.blur(); root.focus({ preventScroll: true }); e.preventDefault(); }
        return;
      }
      var k = e.key.toLowerCase(), n = visible().length;
      if (k === "j" || e.key === "ArrowDown") { cur = Math.min(n - 1, cur + 1); render(); }
      else if (k === "k" || e.key === "ArrowUp") { cur = Math.max(0, cur - 1); render(); }
      else if (k === "e") act("archive");
      else if (k === "h") act("snooze");
      else if (k === "z") act("undo");
      else if (k === "/") { input.tabIndex = 0; input.focus(); }
      else if (e.key === "Escape" && query) { query = ""; input.value = ""; render(); }
      else return;
      e.preventDefault();
    }
    input.addEventListener("input", function () { query = input.value; cur = 0; render(); });
    root.addEventListener("keydown", onKey);
    // The keys also work while the window is on screen and nothing else has
    // focus, so a visitor can press J, K and E without clicking first.
    new IntersectionObserver(function (es) { onScreen = es[0].intersectionRatio > 0.5; }, { threshold: [0, 0.5, 1] }).observe(root);
    document.addEventListener("keydown", function (e) {
      var a = document.activeElement;
      if (!onScreen || root.contains(a) || (a && a !== document.body && a.matches("input, textarea, select, [contenteditable]"))) return;
      if ("jkehz/".indexOf(e.key.toLowerCase()) >= 0 || e.key === "ArrowDown" || e.key === "ArrowUp") onKey(e);
    });
    render();
    return { render: render, act: act };
  }

  // ---- The page --------------------------------------------------------------

  var root = document.documentElement, skyEl = document.getElementById("sky");
  if (!skyEl) return;
  var phase = phaseFor(new Date().getHours()), chosen = false;
  function currentSky() { return (isDark() ? DARK : LIGHT)[phase]; }

  Array.prototype.forEach.call(document.querySelectorAll(".stars"), function (starLayer) { STARS.forEach(function (p) {
    var i = document.createElement("i");
    i.style.left = (p.x * 100).toFixed(2) + "%";
    i.style.top = (p.y * 100).toFixed(2) + "%";
    i.style.width = i.style.height = p.r * 2 + "px";
    i.style.opacity = p.o.toFixed(2);
    starLayer.appendChild(i);
  }); });

  var win = null;
  function paint() {
    var dark = isDark(), s = currentSky();
    function set(k, v) { root.style.setProperty(k, v); }
    set("--top", s.top); set("--bottom", s.bottom);
    var far = s.planeFar || s.far, mid = s.planeMid || mix(s.far, s.near, 0.5);
    if (!dark) {
      // At night Graphite Light's hills sit too close to the sky to read as
      // layers. The far ridge lifts into the haze above the sky's bottom, and
      // each nearer ridge is darker than the one behind it by at least 1.3:1.
      var lifted = contrast(far, s.bottom) < 1.3;
      if (lifted) far = step(s.bottom, "#FFFFFF", 1.3);
      if (lifted || contrast(mid, far) < 1.3 || luminance(mid) > luminance(far)) mid = step(far, "#000000", 1.3);
    }
    set("--far", far);
    set("--mid", mid);
    // The near ridge carries the key hints, so in Graphite Light's daytime it
    // is darkened a little to keep white text readable.
    var near = !dark && phase === "day" ? mix(s.near, "#000000", 0.22) : s.near;
    if (!dark && (contrast(near, mid) < 1.3 || luminance(near) > luminance(mid))) near = step(mid, "#000000", 1.3);
    set("--near", near);
    set("--ground-fade", !dark && luminance(near) < 0.05 ? "2.5rem" : "6rem");
    var page = dark ? mix(s.near, "#0C0C0E", 0.3) : mix(s.near, "#FFFFFF", 0.9);
    set("--page", page);
    set("--sky-ink", s.ink);
    var lightInk = s.ink.toUpperCase() !== "#17304A";
    set("--sky-ink-soft", lightInk ? "rgba(255,255,255,.84)" : "rgba(23,48,74,.82)");
    set("--hero-btn", lightInk ? "#F2F2F0" : "#17181A");
    set("--hero-btn-ink", lightInk ? "#121214" : "#FFFFFF");
    skyEl.dataset.phase = phase;
    var starState = dark || phase !== "day" ? "on" : "off";
    skyEl.dataset.stars = starState;
    var close = document.getElementById("download");
    if (close) { close.dataset.stars = starState; close.dataset.phase = phase; }
    // The closing band is one tone of the current sky, eased in from the page,
    // so the fade runs one way and shows no stripe. The tone is the lightest
    // mix of the sky on which white or near-black text holds at least 4.8:1.
    var closeSky = s.top, lightText = true;
    for (var t = 0.65; t >= 0; t -= 0.05) {
      var c = mix(s.top, s.bottom, t), w = contrast("#FFFFFF", c), d = contrast("#17181A", c);
      if (Math.max(w, d) >= 4.8) { closeSky = c; lightText = w >= d; break; }
    }
    set("--close-sky", closeSky);
    // The sky lightens toward the horizon below the text. The ridges keep the
    // scene's own hill colours, moved lighter or darker only as far as needed
    // to stand 1.3:1 from what sits behind them.
    set("--close-glow", mix(closeSky, s.bottom, 0.8));
    var closeFar = apart(s.planeFar || s.far, closeSky, 1.3);
    var closeNear = apart(s.planeNear || s.near, closeFar, 1.3);
    if (dark && contrast(closeNear, page) < 1.3) closeNear = apart(closeNear, page, 1.3);
    set("--close-far", closeFar);
    set("--close-near", closeNear);
    set("--close-ink", lightText ? "#FFFFFF" : "#17181A");
    set("--close-ink-soft", lightText ? "#FFFFFF" : "#17181A");
    set("--close-btn", lightText ? "#F2F2F0" : "#17181A");
    set("--close-btn-ink", lightText ? "#121214" : "#FFFFFF");
    var when = document.getElementById("when");
    if (when) when.textContent = { dawn: "this morning", day: "today", dusk: "this evening", night: "tonight" }[phase];
    var meta = document.querySelector('meta[name="theme-color"]');
    if (meta) meta.setAttribute("content", s.top);
    Array.prototype.forEach.call(document.querySelectorAll("[data-phase-choice]"), function (b) {
      b.setAttribute("aria-pressed", String(b.dataset.phaseChoice === phase));
    });
    if (win) win.render();
  }

  var winEl = document.getElementById("win");
  if (winEl) win = mountWindow(winEl, {
    sky: currentSky,
    phase: function () { return phase; },
    onZero: function () { skyEl.classList.add("zero"); },
    onAction: function (type) { if (type === "undo") skyEl.classList.remove("zero"); },
  });
  document.addEventListener("click", function (e) {
    var b = e.target.closest && e.target.closest("[data-phase-choice]");
    if (!b) return;
    phase = b.dataset.phaseChoice; chosen = true; paint();
  });
  document.addEventListener("jolta-theme", paint);
  // Follow the clock while the page stays open, until the visitor picks an hour.
  setInterval(function () { if (!chosen) { var p = phaseFor(new Date().getHours()); if (p !== phase) { phase = p; paint(); } } }, 60000);
  paint();

  // Painted planes: the near ridge moves with the page, and the farther a
  // ridge is, the more it lags behind and sinks as the page scrolls. Sinking
  // planes are clipped by the sky, so they never open a gap below the ridge
  // or slide over the window.
  var planes = Array.prototype.slice.call(document.querySelectorAll("[data-speed]"));
  var still = matchMedia("(prefers-reduced-motion: reduce)"), ticking = false;
  addEventListener("scroll", function () {
    if (still.matches || ticking) return;
    ticking = true;
    requestAnimationFrame(function () {
      var y = Math.min(scrollY, innerHeight * 1.5);
      planes.forEach(function (p) { p.style.transform = "translate3d(0," + (y * p.dataset.speed).toFixed(1) + "px,0)"; });
      ticking = false;
    });
  }, { passive: true });
})();
