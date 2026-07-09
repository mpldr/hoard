<script lang="ts">
  // Hoard-Wrapped entry. Free for everyone (Cloud and self-hosted) — no Pro
  // gate. The recap reads play stats that already exist for both session types
  // (cloud `/v1/me` + merged per-day playtime, or the local/self-hosted server),
  // so it renders directly with no entitlement check.
  import { Wrapped } from "$pro";
</script>

<Wrapped />
