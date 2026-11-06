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
