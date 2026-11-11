// The features page: one stage that plays a recording of the real app for the
// chosen feature, in the visitor's theme. The keycap tabs, or the same keys on
// the visitor's keyboard, switch the recording.
(function () {
  "use strict";

  var CLIPS = {
    triage: {
      title: "Triage",
      body: "Move with J and K. E archives on your provider's server, S stars, and Z undoes the last action, so a wrong key costs nothing.",
      keys: [["J", "K", "move"], ["E", "archive"], ["S", "star"], ["Z", "undo"]],
    },
    read: {
      title: "Read",
      body: "Return opens a conversation and J steps to the next message in it. Opening one took 11 to 85 ms on a Mac mini with a release build. Remote images stay blocked until you allow them.",
      keys: [["↩", "open"], ["J", "K", "next and previous"], ["esc", "back to the list"]],
    },
    search: {
      title: "Search",
      body: "Slash searches every account at once, newest first. Operators narrow it down: from:, to:, subject:, has:attachment, is:unread, in:, label:, before:, after: and quoted phrases. A real mailbox of 84,000 messages answered in about 1 ms.",
      keys: [["/", "search"], ["esc", "clear"]],
    },
    palette: {
      title: "Commands",
      body: "⌘K opens one palette for every command: go to a mailbox, move a conversation, snooze, send later, insert a snippet or change the theme. Type a few letters and press Return.",
      keys: [["⌘", "K", "palette"], ["G", "I", "go to Inbox"]],
    },
    snooze: {
      title: "Snooze",
      body: "H snoozes the conversation. Type a time in your own words, like \"tomorrow 9am\". It waits in a Snoozed mailbox on your provider's server, shows under Next to wake in the sidebar, and comes back on time.",
      keys: [["H", "snooze"], ["G", "H", "go to Snoozed"]],
    },
    compose: {
      title: "Write",
      body: "C starts a new message and R replies. ⌘↩ sends, and every message waits ten seconds so you can undo it. ⌘J asks AI for a draft, if you turned AI on.",
      keys: [["C", "new message"], ["R", "reply"], ["⌘", "↩", "send"]],
    },
  };
  var ORDER = ["triage", "read", "search", "palette", "snooze", "compose"];

  var video = document.getElementById("clip");
  if (!video) return;
  var tabs = Array.prototype.slice.call(document.querySelectorAll('[role="tab"]'));
  var panel = document.getElementById("panel"), play = document.getElementById("play"), toggle = document.getElementById("toggle");
  var still = matchMedia("(prefers-reduced-motion: reduce)");
  // Phones get a crop of each recording around the part the feature acts on,
  // so the app's text stays readable at that width.
  var phone = matchMedia("(max-width: 640px)");
  var paused = still.matches;
  var current = "triage", inView = false;

  function isDark() { return window.joltaTheme ? window.joltaTheme.isDark() : matchMedia("(prefers-color-scheme: dark)").matches; }
  function src(name, ext) { return "/media/features/" + name + "-" + (isDark() ? "dark" : "light") + (phone.matches ? "-phone" : "") + "." + ext; }

  // With reduced motion the recording waits behind a play button; otherwise it
  // plays while the stage is on screen.
  function start() {
    var p = video.play();
    // Only a browser that refuses autoplay pauses the stage for good; a play()
    // cut short by scrolling away (AbortError) just tries again next time.
    if (p && p.catch) p.catch(function (e) { if (e && e.name === "NotAllowedError") setPaused(true); });
  }
  // The visitor can pause the looping recording at any time; the choice holds
  // across tabs until they press play again.
  function setPaused(value) {
    paused = value;
    play.hidden = !paused;
    toggle.textContent = paused ? "Play" : "Pause";
    toggle.setAttribute("aria-label", paused ? "Play the recording" : "Pause the recording");
    if (paused) video.pause(); else if (inView) start();
  }
  function load() {
    video.poster = src(current, "webp");
    video.src = src(current, "mp4");
    setPaused(paused);
  }

  function show(name, focus) {
    current = name;
    var clip = CLIPS[name];
    tabs.forEach(function (t) {
      var on = t.dataset.clip === name;
      t.setAttribute("aria-selected", String(on));
      t.tabIndex = on ? 0 : -1;
      if (on) { panel.setAttribute("aria-labelledby", t.id); if (focus) t.focus(); }
    });
    document.getElementById("clip-title").textContent = clip.title;
    document.getElementById("clip-body").textContent = clip.body;
    var list = document.getElementById("clip-keys");
    list.innerHTML = "";
    clip.keys.forEach(function (k) {
      var li = document.createElement("li");
      k.slice(0, -1).forEach(function (key) { var kbd = document.createElement("kbd"); kbd.textContent = key; li.appendChild(kbd); });
      var span = document.createElement("span"); span.textContent = k[k.length - 1]; li.appendChild(span);
      list.appendChild(li);
    });
    load();
  }

  tabs.forEach(function (t, i) {
    t.addEventListener("click", function () { show(t.dataset.clip, false); });
    t.addEventListener("keydown", function (e) {
      var d = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
      if (e.key === "Home") d = -i; else if (e.key === "End") d = tabs.length - 1 - i;
      if (!d) return;
      e.preventDefault();
      show(ORDER[(i + d + tabs.length) % tabs.length], true);
    });
  });
  play.addEventListener("click", function () { setPaused(false); });
  toggle.addEventListener("click", function () { setPaused(!paused); });
  phone.addEventListener("change", load);

  // The same keys as in Jolta pick the recording, unless the visitor is typing.
  var BY_KEY = { e: "triage", enter: "read", "/": "search", k: "palette", h: "snooze", c: "compose" };
  document.addEventListener("keydown", function (e) {
    if (e.ctrlKey || e.altKey) return;
    var a = document.activeElement;
    if (a && a.matches("input, textarea, select, [contenteditable]")) return;
    var key = e.key.toLowerCase();
    if (e.metaKey && key !== "k") return;
    if (key === "enter" && a && a !== document.body) return;
    var name = BY_KEY[key];
    if (!name || name === current) return;
    e.preventDefault();
    show(name, false);
    panel.scrollIntoView({ block: "nearest", behavior: still.matches ? "auto" : "smooth" });
  });

  // Play only while the stage is on screen.
  new IntersectionObserver(function (es) {
    inView = es[0].isIntersecting;
    if (inView && !paused) start();
    else if (!inView) video.pause();
  }, { threshold: 0.35 }).observe(video);

  document.addEventListener("jolta-theme", load);
  show("triage", false);
})();
