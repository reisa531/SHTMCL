import { reduceMotion } from "./motion";

const easing = "cubic-bezier(.16,1,.3,1)";

/** Own only decoration: never intercept, defer or replay a business action. */
export function installSoftRipples(root: HTMLElement): () => void {
  const active = new Map<HTMLElement, () => void>();
  const clear = () => { for (const dispose of active.values()) dispose(); };
  const show = (event: PointerEvent | MouseEvent) => {
    if (document.hidden || reduceMotion() || !(event.target instanceof Element)) return;
    const button = event.target.closest<HTMLButtonElement>("button.btn, button.nav__item, button.tab, button.icon-btn, button.account-button");
    if (!button || !root.contains(button) || button.disabled || button.getAttribute("aria-disabled") === "true") return;
    if (event instanceof PointerEvent && (!event.isPrimary || event.button !== 0)) return;
    if (event.type === "click" && event.detail !== 0) return;
    const rect = button.getBoundingClientRect();
    if (!rect.width || !rect.height || typeof button.animate !== "function") return;
    active.get(button)?.();
    if (active.size >= 12) active.values().next().value?.();
    const keyboard = event.type === "click";
    const x = keyboard ? rect.width / 2 : event.clientX - rect.left;
    const y = keyboard ? rect.height / 2 : event.clientY - rect.top;
    const diameter = 2 * Math.hypot(Math.max(x, rect.width - x), Math.max(y, rect.height - y));
    const layer = document.createElement("span");
    layer.className = "soft-ripple-layer";
    layer.setAttribute("aria-hidden", "true");
    const wave = document.createElement("span");
    wave.className = "soft-ripple";
    Object.assign(wave.style, { width: `${diameter}px`, height: `${diameter}px`, left: `${x - diameter / 2}px`, top: `${y - diameter / 2}px` });
    layer.append(wave);
    button.append(layer);
    const animation = wave.animate([
      { transform: "scale(.12)", opacity: .08 },
      { transform: "scale(.65)", opacity: .16, offset: .35 },
      { transform: "scale(1)", opacity: 0 },
    ], { duration: 620, easing });
    const dispose = () => {
      animation.cancel();
      layer.remove();
      if (active.get(button) === dispose) active.delete(button);
    };
    active.set(button, dispose);
    void animation.finished.then(dispose, dispose);
  };
  const media = window.matchMedia("(prefers-reduced-motion: reduce)");
  const observer = new MutationObserver(() => { if (reduceMotion()) clear(); });
  observer.observe(document.documentElement, { attributes: true, attributeFilter: ["data-motion"] });
  root.addEventListener("pointerdown", show);
  root.addEventListener("click", show);
  document.addEventListener("visibilitychange", clear);
  media.addEventListener("change", clear);
  return () => {
    root.removeEventListener("pointerdown", show);
    root.removeEventListener("click", show);
    document.removeEventListener("visibilitychange", clear);
    media.removeEventListener("change", clear);
    observer.disconnect();
    clear();
  };
}

/** Animate existing content without remounting forms or resetting their state. */
export function enterSurface(element: HTMLElement | null): () => void {
  if (!element || document.hidden || reduceMotion() || typeof element.animate !== "function") return () => {};
  const animation = element.animate([
    { opacity: .35, transform: "translateY(6px)" },
    { opacity: 1, transform: "translateY(0)" },
  ], { duration: 240, easing });
  const cancel = () => animation.cancel();
  const media = window.matchMedia("(prefers-reduced-motion: reduce)");
  const observer = new MutationObserver(() => { if (reduceMotion()) cancel(); });
  observer.observe(document.documentElement, { attributes: true, attributeFilter: ["data-motion"] });
  media.addEventListener("change", cancel);
  document.addEventListener("visibilitychange", cancel);
  const clean = () => {
    cancel();
    observer.disconnect();
    media.removeEventListener("change", cancel);
    document.removeEventListener("visibilitychange", cancel);
  };
  void animation.finished.then(clean, clean);
  return clean;
}
