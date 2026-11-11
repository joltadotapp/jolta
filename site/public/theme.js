// Loaded in <head> on every page, before the first paint. The page follows the
// system's light or dark appearance unless the visitor picked one in the footer.
(function () {
  var root = document.documentElement;
  var KEY = "jolta-theme";
  function stored() {
    try { return localStorage.getItem(KEY) || "system"; } catch (e) { return "system"; }
  }
  function apply(choice) {
    if (choice === "light" || choice === "dark") root.dataset.theme = choice;
    else delete root.dataset.theme;
  }
  root.classList.add("js");
  apply(stored());

  window.joltaTheme = {
    isDark: function () {
      var t = root.dataset.theme;
      return t ? t === "dark" : matchMedia("(prefers-color-scheme: dark)").matches;
    },
  };

  function sync() {
    var choice = stored();
    var buttons = document.querySelectorAll("[data-theme-choice]");
    for (var i = 0; i < buttons.length; i++) {
      buttons[i].setAttribute("aria-pressed", String(buttons[i].dataset.themeChoice === choice));
    }
  }
  function changed() { document.dispatchEvent(new CustomEvent("jolta-theme")); }

  document.addEventListener("DOMContentLoaded", function () {
    sync();
    document.addEventListener("click", function (e) {
      var b = e.target.closest && e.target.closest("[data-theme-choice]");
      if (!b) return;
      var choice = b.dataset.themeChoice;
      try { if (choice === "system") localStorage.removeItem(KEY); else localStorage.setItem(KEY, choice); } catch (err) {}
      apply(choice); sync(); changed();
    });
  });
  matchMedia("(prefers-color-scheme: dark)").addEventListener("change", changed);
})();
