import { LazyStore } from "@tauri-apps/plugin-store";

type SectionKey =
  | "tracked"
  | "orphans"
  | "playtime"
  | "detected"
  | "dashboard";

const STORE = new LazyStore("card_sizes.json");
const STORE_KEY = "sizes";

const DEFAULTS: Record<SectionKey, number> = {
  tracked: 220,
  orphans: 220,
  playtime: 220,
  detected: 280,
  // El panel son carátulas grandes, no fichas. 360 está calculado para que la
  // rejilla elástica caiga en la misma retícula que tenían los breakpoints
  // fijos (lg:3 / 2xl:4): a pantalla completa en 1080p el contenido mide
  // 1536px y con gap-5 salen 4 columnas de 369px, las mismas de antes; y en
  // ventanas de ~1280 siguen saliendo 3.
  dashboard: 360,
};

// Por sección, porque no todas aguantan lo mismo: una ficha de la biblioteca
// se sigue leyendo a 140px, pero la tarjeta del panel lleva pastilla de estado
// y botón de copia en la misma fila y se rompe mucho antes.
const BOUNDS: Record<SectionKey, { min: number; max: number }> = {
  tracked: { min: 140, max: 500 },
  orphans: { min: 140, max: 500 },
  playtime: { min: 140, max: 500 },
  detected: { min: 140, max: 500 },
  dashboard: { min: 220, max: 640 },
};

let sizes = $state<Record<SectionKey, number>>({ ...DEFAULTS });

export async function hydrateCardSizes(): Promise<void> {
  try {
    const saved = await STORE.get<Record<SectionKey, number>>(STORE_KEY);
    if (saved) {
      sizes = { ...DEFAULTS, ...saved };
    }
  } catch { /* ignore */ }
}

// Persistencia diferida: esto se escribe desde un arrastre, así que guardar en
// el acto es un viaje a disco (ida y vuelta por IPC) en cada pointermove.
// 250ms tras soltar no se notan y dejan una única escritura.
let persistTimer: ReturnType<typeof setTimeout> | null = null;

function schedulePersist(): void {
  if (persistTimer) clearTimeout(persistTimer);
  persistTimer = setTimeout(() => {
    persistTimer = null;
    void save();
  }, 250);
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
  const { min, max } = BOUNDS[key];
  sizes[key] = Math.round(Math.max(min, Math.min(max, w)));
  schedulePersist();
}

export function resetCardWidths(): void {
  sizes = { ...DEFAULTS };
  schedulePersist();
}

export type { SectionKey };
