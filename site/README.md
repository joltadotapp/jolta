# jolta.app

The website. A Cloudflare Worker (`src/worker.js`) serves the pages in `public/` and, from the R2
bucket `jolta-releases`, the release files that `scripts/release.sh --public` uploads:

- `/updates/mac/appcast.xml` and the zips next to it: the update feed every public build checks.
  These paths must keep working for as long as copies of Jolta 0.4.0 or later are installed.
- `/download/Jolta.zip`: the latest public build.

`www.jolta.app` redirects to `jolta.app`. The privacy policy and terms (`/privacy`,
`/terms-and-conditions`) are the addresses in the Google and Microsoft app registrations, and
`/.well-known/microsoft-identity-association.json` proves the Microsoft publisher domain. Keep all
three in place.

The homepage's sky follows the visitor's clock (dawn, day, dusk, night) and the light or dark
theme, which follows the system unless the visitor picks one in the footer. `public/theme.js`
applies the stored theme before the first paint on every page; `public/site.js` paints the sky
from the colours in `apple/Jolta/Scenery.swift`, places the sun, the moon and the trees, and runs
the live inbox demo, whose mail is invented. The Worker's Content-Security-Policy allows scripts only from jolta.app itself, so
pages may not use inline scripts, inline `style` attributes or anything from another site.
The fonts in `public/fonts/` are self-hosted under the SIL Open Font License, with their
license files beside them. `DESIGN.md` at the repository root records the design system.

## Work on it

```sh
cd site
npm ci
npm run dev
```

`npm run dev` serves the site on port 8790 and reloads on save. Locally the R2 bucket is empty, so
`/updates/` and `/download/` answer 404.

## Deploy

```sh
cd site
npx wrangler deploy
```

Deploying needs `npx wrangler login` to the Cloudflare account named in `wrangler.jsonc`. Pages
are served with `max-age=0, must-revalidate`, so a deploy shows up on the next page load.
