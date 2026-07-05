import { writable } from "svelte/store";

// True while the guided tour is running. `ProFeature` reads this to render the
// Pro sections (Hoard-Screen / Hoard-Wrapped) in preview mode: the tour walks
// past them so it must show the feature without starting its one-week trial
// (opening the page normally spends the clock on first view).
export const tourActive = writable(false);
