import type { APIRoute } from 'astro';

// Generated rather than static so the domain follows `site` in astro.config.mjs.
// Cloudflare appends its managed block (the AI-crawler opt-outs) below this.
export const GET: APIRoute = ({ site }) => {
  const sitemap = new URL('sitemap-index.xml', site).href;
  return new Response(
    `User-agent: *\nAllow: /\n\nSitemap: ${sitemap}\n`,
    { headers: { 'Content-Type': 'text/plain; charset=utf-8' } },
  );
};
