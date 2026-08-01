/**
 * Shape of the cover frame in the Dashboard grid — a local, per-device
 * preference, like `cardSizes`.
 *
 * 2:3 is the convention every store shows a game cover in (Steam's
 * `library_600x900`, GOG, Epic), so it's the default. Square is offered
 * because plenty of people curate their own art — emulated or non-Steam games,
 * custom covers — and that art is often 1:1; forcing it into a poster would
 * crop it just as badly as the old landscape capsule cropped in a poster.
 */
import { LazyStore } from "@tauri-apps/plugin-store";

export type CoverShape = "portrait" | "square";

const STORE = new LazyStore("cover_shape.json");
const STORE_KEY = "shape";
const DEFAULT: CoverShape = "portrait";

let shape = $state<CoverShape>(DEFAULT);

export async function hydrateCoverShape(): Promise<void> {
  try {
    const saved = await STORE.get<CoverShape>(STORE_KEY);
    if (saved === "portrait" || saved === "square") shape = saved;
  } catch {
    /* first run / unreadable store — the default stands */
  }
}

export function coverShape(): CoverShape {
  return shape;
}

export async function setCoverShape(next: CoverShape): Promise<void> {
  shape = next;
  try {
    await STORE.set(STORE_KEY, next);
    await STORE.save();
  } catch {
    /* the choice still applies this session */
  }
}

/** Tailwind aspect class for the current shape. */
export function coverAspectClass(s: CoverShape = shape): string {
  return s === "square" ? "aspect-square" : "aspect-[2/3]";
}
