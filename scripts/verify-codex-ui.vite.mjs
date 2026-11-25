// Opt-in Vite config for measuring a real desktop WebView, used only by verify:codex.
import { defineConfig } from "vite";

function watchUsageCard(hot) {
  let previous = null;
  function sample() {
    const card = document.querySelector('[data-provider="codex"]');
    if (!card || !card.checkVisibility() || document.visibilityState !== "visible") return;
    const rect = card.getBoundingClientRect();
    if (rect.top >= innerHeight || rect.bottom <= 0) return;
    const values = Array.from(card.querySelectorAll(".metrics strong"), (el) => {
      const digits = el.textContent.replace(/[^0-9]/g, "");
      return digits ? Number(digits) : null;
    });
    if (values.length !== 2 || values.some((n) => n === null)) return;
    const [today, latest] = values;
    const sourceSecond = Number(card.dataset.latestAt);
    if (!sourceSecond) return;
    const key = [sourceSecond, today, latest].join("/");
    if (previous?.key === key) return;
    hot.send("usage-verification:frame", {
      today, latest, sourceSecond, delta: previous ? today - previous.today : null,
      paintedAt: Date.now(),
    });
    previous = {key, today};
  }
  // Wait for a paint opportunity after React's DOM commit. Ignore hidden windows.
  let scheduled = false;
  function queue() {
    if (scheduled) return;
    scheduled = true;
    requestAnimationFrame(() => requestAnimationFrame(() => { scheduled = false; sample(); }));
  }
  new MutationObserver(queue).observe(document.body, {
    subtree: true, childList: true, characterData: true, attributes: true,
  });
  window.addEventListener("scroll", queue);
  document.addEventListener("visibilitychange", queue);
  queue();
}

export default defineConfig({
  plugins: [{
    name: "codex-usage-screen-verification",
    apply: "serve",
    transformIndexHtml() {
      return [{tag: "script", attrs: {type: "module", src: "/@id/virtual:usage-verification"}, injectTo: "body"}];
    },
    resolveId(id) {
      if (id === "virtual:usage-verification") return "\0" + id;
    },
    load(id) {
      if (id === "\0virtual:usage-verification") {
        return `if (import.meta.hot && window.__TAURI_INTERNALS__) (${watchUsageCard})(import.meta.hot);`;
      }
    },
    configureServer(server) {
      server.ws.on("usage-verification:frame", ({today, latest, sourceSecond, delta, paintedAt}) => {
        if (![today, latest, sourceSecond, paintedAt].every(Number.isSafeInteger)) return;
        if (delta === null) {
          console.log(`[用量验收] 基线 ${today} Token。保持卡片可见，在另一终端发起一次请求。`);
        } else if (Number.isSafeInteger(delta)) {
          // SQLite stores the event timestamp rounded down to whole seconds.
          const upperMs = paintedAt - sourceSecond * 1000;
          console.log(`[用量验收] 今日增量=${delta} 最近一次=${latest} 界面时间=${new Date(paintedAt).toISOString()} 事件到显示的延迟上界=${upperMs}ms`);
        }
      });
    },
  }],
});
