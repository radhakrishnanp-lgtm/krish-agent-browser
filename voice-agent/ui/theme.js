// Applies the saved colour theme before the page paints. "system" follows the device setting.
(function () {
  var mq = window.matchMedia('(prefers-color-scheme: dark)');
  function saved() { try { return localStorage.getItem('voiceAgentTheme') || 'system'; } catch (e) { return 'system'; } }
  window.applyTheme = function (name) {
    name = name || saved();
    document.documentElement.dataset.theme = name === 'system' ? (mq.matches ? 'dark' : 'light') : name;
  };
  mq.addEventListener('change', function () { if (saved() === 'system') window.applyTheme(); });
  window.applyTheme();
})();
