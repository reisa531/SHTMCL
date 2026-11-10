export interface MotionPoint { x: number; y: number }

export function reduceMotion(): boolean {
  return document.documentElement.dataset.motion === "reduced"
    || window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

/** Animation is decoration only; accepting/starting a task never awaits it. */
export function downloadFlight(origin?: MotionPoint): () => void {
  const target = document.querySelector<HTMLElement>('.nav__item[data-page="tasks"]');
  if (!target || document.hidden) return () => {};
  target.dataset.downloadFeedback = "true";
  let ball: HTMLElement | undefined;
  let animation: Animation | undefined;
  const cleanup = () => {
    animation?.cancel();
    ball?.remove();
    delete target.dataset.downloadFeedback;
    window.clearTimeout(timer);
    window.removeEventListener("resize", cleanup);
    document.removeEventListener("visibilitychange", cleanup);
  };
  const timer = window.setTimeout(cleanup, 700);
  window.addEventListener("resize", cleanup, { once: true });
  document.addEventListener("visibilitychange", cleanup, { once: true });
  if (origin && !reduceMotion()) {
    const rect = target.getBoundingClientRect();
    const end = { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };
    if (origin.x >= 0 && origin.y >= 0 && origin.x <= window.innerWidth && origin.y <= window.innerHeight && rect.width > 0) {
      ball = document.createElement("span");
      ball.className = "download-flight";
      ball.setAttribute("aria-hidden", "true");
      document.body.append(ball);
      const position = (x: number, y: number, scale: number) => `translate(${x - 8}px, ${y - 8}px) scale(${scale})`;
      animation = ball.animate([
        { transform: position(origin.x, origin.y, 1), opacity: 1 },
        { transform: position((origin.x + end.x) / 2, Math.max(12, Math.min(origin.y, end.y) - 90), 0.85), opacity: 1, offset: 0.5 },
        { transform: position(end.x, end.y, 0.3), opacity: 0 },
      ], { duration: 540, easing: "cubic-bezier(.25,.1,.25,1)", fill: "forwards" });
      void animation.finished.then(cleanup, () => {});
    }
  }
  return cleanup;
}
