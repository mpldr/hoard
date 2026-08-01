<script lang="ts">
  // Marcador que el router enseña mientras carga el chunk de una ruta.
  //
  // Es deliberadamente invisible al principio: una navegación a un chunk ya
  // cacheado resuelve en un microtask, y sin el retraso el spinner parpadearía
  // en *cada* cambio de página. Con él, sólo aparece cuando la carga tarda lo
  // bastante como para que no enseñar nada resulte raro.
</script>

<div class="route-fallback flex h-full items-center justify-center bg-zinc-950">
  <div
    class="h-6 w-6 animate-spin rounded-full border-2 border-zinc-700 border-t-emerald-500"
  ></div>
</div>

<style>
  .route-fallback {
    opacity: 0;
    animation: route-fallback-in 120ms ease-out 200ms forwards;
  }

  @keyframes route-fallback-in {
    to {
      opacity: 1;
    }
  }

  /* Respeta a quien pide menos movimiento: sin fundido, pero manteniendo el
     retraso para que las navegaciones rápidas sigan sin parpadeo. */
  @media (prefers-reduced-motion: reduce) {
    .route-fallback {
      animation: route-fallback-in 0ms linear 200ms forwards;
    }
  }
</style>
