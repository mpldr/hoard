// Client-only: the page reads the Supabase session and calls the API with the
// user's JWT. No SSR/prerender (there's nothing to render server-side and the
// session lives in the browser).
export const prerender = false;
export const ssr = false;
