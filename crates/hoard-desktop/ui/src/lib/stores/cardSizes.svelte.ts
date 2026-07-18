import { LazyStore } from "@tauri-apps/plugin-store";

type SectionKey = "tracked" | "orphans" | "playtime" | "detected";

const STORE = new LazyStore("card_sizes.json");
const STORE_KEY = "sizes";

const DEFAULTS: Record<SectionKey, number> = {
  tracked: 220,
  orphans: 220,
  playtime: 220,
  detected: 280,
};

const MIN = 140;
const MAX = 500;

let sizes = $state<Record<SectionKey, number>>({ ...DEFAULTS });

export async function hydrateCardSizes(): Promise<void> {
  try {
    const saved = await STORE.get<Record<SectionKey, number>>(STORE_KEY);
    if (saved) {
      sizes = { ...DEFAULTS, ...saved };
    }
  } catch { /* ignore */ }
}

async function save(): Promise<void> {
  try {
    await STORE.set(STORE_KEY, sizes);
    await STORE.save();
  } catch { /* ignore */ }
}

export function cardWidth(key: SectionKey): number {
  return sizes[key];
}

export function setCardWidth(key: SectionKey, w: number): void {
  sizes[key] = Math.round(Math.max(MIN, Math.min(MAX, w)));
  void save();
}

export function resetCardWidths(): void {
  sizes = { ...DEFAULTS };
  void save();
}

export type { SectionKey };