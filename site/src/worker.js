// jolta.app: the pages in public/, and the release files in the R2 bucket under the same
// paths they had on the old server, because installed copies of Jolta check
// /updates/mac/appcast.xml for updates.

const RELEASE_PREFIXES = ["updates/", "download/"];

const CONTENT_TYPES = {
  xml: "application/xml",
  zip: "application/zip",
  delta: "application/octet-stream",
  html: "text/html; charset=utf-8",
};

// A versioned zip never changes once published; the appcast and download/Jolta.zip change
// with every release.
const IMMUTABLE = /^updates\/mac\/[^/]+-\d[^/]*\.(zip|delta)$/;

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.hostname === "www.jolta.app") {
      url.hostname = "jolta.app";
      return Response.redirect(url.toString(), 301);
    }
    const key = url.pathname.slice(1);
    if (RELEASE_PREFIXES.some((prefix) => key.startsWith(prefix))) {
      return release(request, env, key);
    }
    return env.ASSETS.fetch(request);
  },
};

async function release(request, env, key) {
  if (request.method !== "GET" && request.method !== "HEAD") {
    return new Response(null, { status: 405, headers: { allow: "GET, HEAD" } });
  }
  const object =
    request.method === "HEAD"
      ? await env.RELEASES.head(key)
      : await env.RELEASES.get(key, { range: request.headers, onlyIf: request.headers });
  if (object === null) {
    return new Response("Not found\n", { status: 404 });
  }

  const headers = new Headers();
  object.writeHttpMetadata(headers);
  if (!headers.has("content-type")) {
    headers.set("content-type", CONTENT_TYPES[key.split(".").pop()] ?? "application/octet-stream");
  }
  headers.set("etag", object.httpEtag);
  headers.set("last-modified", object.uploaded.toUTCString());
  headers.set("accept-ranges", "bytes");
  headers.set(
    "cache-control",
    IMMUTABLE.test(key) ? "public, max-age=31536000, immutable" : "public, max-age=300",
  );

  if (request.method === "HEAD") {
    headers.set("content-length", String(object.size));
    return new Response(null, { headers });
  }
  // R2 leaves out the body when If-None-Match or If-Modified-Since says the client's copy is current.
  if (!("body" in object)) {
    return new Response(null, { status: 304, headers });
  }
  if (object.range && request.headers.has("range")) {
    const { offset, length, suffix } = object.range;
    const start = suffix !== undefined ? object.size - suffix : (offset ?? 0);
    const end = length !== undefined && suffix === undefined ? start + length - 1 : object.size - 1;
    headers.set("content-range", `bytes ${start}-${end}/${object.size}`);
    headers.set("content-length", String(end - start + 1));
    return new Response(object.body, { status: 206, headers });
  }
  headers.set("content-length", String(object.size));
  return new Response(object.body, { headers });
}
