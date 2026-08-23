/**
 * Motion presets for Svelte transitions.
 *
 * CSS transitions inherit the `prefers-reduced-motion` override in
 * app.css, but JS-driven Svelte transitions do not — they compute their own
 * timing. So every transition in the app goes through a preset here, which
 * collapses its duration when the user asked for less motion. Durations and
 * easings mirror the `--dur-*` / `--ease-*` tokens in app.css; keep the two
 * in sync.
 */

import { cubicOut, cubicInOut } from 'svelte/easing';
import type { TransitionConfig } from 'svelte/transition';

export const DUR = { fast: 120, base: 180, slow: 280 } as const;

/** Live check — the user can flip the OS setting mid-session. */
export function reducedMotion(): boolean {
  return (
    typeof window !== 'undefined' &&
    window.matchMedia('(prefers-reduced-motion: reduce)').matches
  );
}

/** Duration, or ~0 when motion is reduced (0 would skip the tick entirely). */
export function dur(ms: number): number {
  return reducedMotion() ? 1 : ms;
}

/** Stagger delay for the nth item, capped so long lists never crawl. */
export function stagger(i: number, step = 18, max = 220): number {
  if (reducedMotion()) return 0;
  return Math.min(i * step, max);
}

/** `animate:flip` duration — pass as `{ duration: flipDur() }`. */
export function flipDur(): number {
  return dur(DUR.slow);
}

/** Fade up: default entrance for rows, cards and list items. */
export function fadeUp(
  _node: Element,
  { y = 6, duration = DUR.base, delay = 0 }: { y?: number; duration?: number; delay?: number } = {},
): TransitionConfig {
  return {
    delay,
    duration: dur(duration),
    easing: cubicOut,
    css: (t, u) => `opacity: ${t}; transform: translateY(${u * y}px)`,
  };
}

/** Scale + fade, for things that pop into existence (menus, badges, toasts). */
export function pop(
  _node: Element,
  { start = 0.94, duration = DUR.fast, delay = 0 }: { start?: number; duration?: number; delay?: number } = {},
): TransitionConfig {
  return {
    delay,
    duration: dur(duration),
    easing: cubicOut,
    css: (t, u) => `opacity: ${t}; transform: scale(${start + (1 - start) * t}) translateY(${u * -2}px)`,
  };
}

/** Opacity only — for overlays and anything already moving on its own. */
export function fadeOnly(
  _node: Element,
  { duration = DUR.fast, delay = 0 }: { duration?: number; delay?: number } = {},
): TransitionConfig {
  return { delay, duration: dur(duration), easing: cubicInOut, css: (t) => `opacity: ${t}` };
}

/** Horizontal slide, for drawers/sidebars. `x` is the offscreen offset. */
export function slideX(
  _node: Element,
  { x = -16, duration = DUR.base, delay = 0 }: { x?: number; duration?: number; delay?: number } = {},
): TransitionConfig {
  return {
    delay,
    duration: dur(duration),
    easing: cubicOut,
    css: (t, u) => `opacity: ${t}; transform: translateX(${u * x}px)`,
  };
}

/** Collapse height — for banners/bars that push layout instead of overlaying. */
export function collapse(
  node: Element,
  { duration = DUR.base, delay = 0 }: { duration?: number; delay?: number } = {},
): TransitionConfig {
  const style = getComputedStyle(node);
  const h = parseFloat(style.height);
  const pt = parseFloat(style.paddingTop);
  const pb = parseFloat(style.paddingBottom);
  const mb = parseFloat(style.marginBottom);
  return {
    delay,
    duration: dur(duration),
    easing: cubicOut,
    css: (t) =>
      `overflow: hidden; opacity: ${t}; height: ${t * h}px; padding-top: ${t * pt}px;` +
      `padding-bottom: ${t * pb}px; margin-bottom: ${t * mb}px`,
  };
}
