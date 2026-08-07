import { vitePreprocess } from "@sveltejs/vite-plugin-svelte";

export default {
  preprocess: vitePreprocess(),
  compilerOptions: {
    // Runas para toda la app. Antes no se podía: `lucide-svelte` 0.4x seguía
    // usando `$$props`, sintaxis legacy que el modo runas prohíbe, y como
    // `compilerOptions` alcanza también a los `.svelte` de las dependencias,
    // activarlo aquí rompía la compilación de los iconos. Ese paquete quedó
    // deprecado; el sustituto (`@lucide/svelte` 1.x) y `svelte-spa-router` 5
    // son nativos de Svelte 5, así que ya no queda nada legacy en el árbol.
    //
    // Qué gana: el compilador deja de emitir el puente de compatibilidad
    // (props mutables, `$$restProps`, invalidación por asignación) y compila
    // con señales directas. De paso convierte en error de compilación
    // cualquier recaída en `export let` / `$:`, que es la regla que CLAUDE.md
    // venía pidiendo a mano.
    runes: true,
  },
};
