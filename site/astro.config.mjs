import { defineConfig } from 'astro/config';
import sitemap from '@astrojs/sitemap';

// Static marketing site for rewynd. `site` drives canonical + Open Graph URLs,
// and is the base for the sitemap the integration emits at /sitemap-index.xml.
export default defineConfig({
  site: 'https://rewynd.dev',
  integrations: [sitemap()],
});
