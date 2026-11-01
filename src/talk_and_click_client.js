// Hex talk and click: while a Hex dictation records, clicks on this page point
// at elements instead of pressing them. Add to a development page only:
//   <script src="http://127.0.0.1:47489/client.js"></script>
(() => {
  if (window.__hexTalkAndClick) return;
  window.__hexTalkAndClick = true;

  const HEX = new URL(document.currentScript.src).origin;
  const POLL_MS = 250;
  const ACCENT = "#3b82f6";
  const OWN = "data-hex-talk-and-click";
  // Agentation's own toolbar keeps working.
  const SKIP = `[${OWN}], [data-feedback-toolbar], [data-annotation-popup]`;

  let recording = false;
  let count = 0;
  let badges = [];

  const highlight = document.createElement("div");
  highlight.setAttribute(OWN, "");
  highlight.style.cssText = `position:fixed;display:none;pointer-events:none;z-index:2147483646;border:2px solid ${ACCENT};background:${ACCENT}22;border-radius:4px;`;

  const cssEscape = (text) => (window.CSS && CSS.escape ? CSS.escape(text) : text);

  const part = (element) => {
    const tag = element.tagName.toLowerCase();
    if (element.id) return `${tag}#${cssEscape(element.id)}`;
    const testId = element.getAttribute("data-testid");
    if (testId) return `${tag}[data-testid="${testId}"]`;
    const classes = Array.from(element.classList).slice(0, 3).map(cssEscape);
    let text = classes.length ? `${tag}.${classes.join(".")}` : tag;
    const same = element.parentElement
      ? Array.from(element.parentElement.children).filter((child) => child.tagName === element.tagName)
      : [];
    if (same.length > 1) text += `:nth-of-type(${same.indexOf(element) + 1})`;
    return text;
  };

  const selector = (element) => {
    const parts = [];
    for (let node = element; node && node.nodeType === 1 && node !== document.documentElement; node = node.parentElement) {
      parts.unshift(part(node));
      if (node.id) break;
    }
    return parts.join(" > ");
  };

  // React's development names, best effort: nothing breaks without them.
  const reactComponents = (element) => {
    const key = Object.keys(element).find((name) => name.startsWith("__reactFiber$"));
    const names = [];
    for (let fiber = key ? element[key] : null; fiber && names.length < 8; fiber = fiber.return) {
      const type = fiber.type;
      const name = typeof type === "function" ? type.displayName || type.name : null;
      if (name && !names.includes(name)) names.unshift(name);
    }
    return names.length ? names.join(" > ") : undefined;
  };

  const sessionId = () => {
    try {
      return localStorage.getItem(`agentation-session-${location.pathname}`) || undefined;
    } catch {
      return undefined;
    }
  };

  const describe = (element, event) => {
    const box = element.getBoundingClientRect();
    return {
      element: element.tagName.toLowerCase(),
      elementPath: selector(element),
      url: location.href,
      timestamp: Date.now(),
      x: (event.clientX / window.innerWidth) * 100,
      y: event.clientY + window.scrollY,
      sessionId: sessionId(),
      cssClasses: element.className && typeof element.className === "string" ? element.className : undefined,
      nearbyText: (element.innerText || element.getAttribute("aria-label") || "").trim().slice(0, 200) || undefined,
      reactComponents: reactComponents(element),
      boundingBox: { x: box.x, y: box.y + window.scrollY, width: box.width, height: box.height },
    };
  };

  const target = (event) => {
    const element = event.target;
    return element && element.nodeType === 1 && !element.closest(SKIP) ? element : null;
  };

  const badge = (event) => {
    const mark = document.createElement("div");
    mark.setAttribute(OWN, "");
    mark.textContent = String(++count);
    mark.style.cssText = `position:fixed;left:${event.clientX - 11}px;top:${event.clientY - 11}px;width:22px;height:22px;border-radius:11px;background:${ACCENT};color:#fff;font:600 12px/22px system-ui;text-align:center;pointer-events:none;z-index:2147483647;`;
    document.documentElement.append(mark);
    badges.push(mark);
  };

  const swallow = (event) => {
    if (!recording || !target(event)) return;
    event.preventDefault();
    event.stopImmediatePropagation();
  };

  const onClick = (event) => {
    const element = target(event);
    if (!recording || !element) return;
    event.preventDefault();
    event.stopImmediatePropagation();
    badge(event);
    // text/plain keeps this a simple request, with no preflight.
    fetch(`${HEX}/click`, {
      method: "POST",
      headers: { "Content-Type": "text/plain" },
      body: JSON.stringify(describe(element, event)),
    }).catch(() => {});
  };

  const onMove = (event) => {
    const element = recording ? target(event) : null;
    if (!element) {
      highlight.style.display = "none";
      return;
    }
    const box = element.getBoundingClientRect();
    Object.assign(highlight.style, {
      display: "block",
      left: `${box.x}px`,
      top: `${box.y}px`,
      width: `${box.width}px`,
      height: `${box.height}px`,
    });
  };

  const setRecording = (next) => {
    if (next === recording) return;
    recording = next;
    document.documentElement.style.cursor = next ? "crosshair" : "";
    if (next) {
      count = 0;
      document.documentElement.append(highlight);
      return;
    }
    highlight.remove();
    highlight.style.display = "none";
    const finished = badges;
    badges = [];
    setTimeout(() => finished.forEach((mark) => mark.remove()), 1500);
  };

  for (const type of ["pointerdown", "mousedown", "pointerup", "mouseup", "auxclick", "dblclick"]) {
    window.addEventListener(type, swallow, true);
  }
  window.addEventListener("click", onClick, true);
  window.addEventListener("mousemove", onMove, true);

  const poll = async () => {
    if (document.visibilityState === "visible") {
      try {
        const response = await fetch(`${HEX}/state`);
        setRecording(response.ok && (await response.json()).recording === true);
      } catch {
        setRecording(false);
      }
    }
    setTimeout(poll, POLL_MS);
  };
  poll();
})();
