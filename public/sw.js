const CACHE = "revebot-shell-v2";
const SHELL = [
  "/",
  "/manifest.webmanifest",
  "/icon.svg",
  "/css/app.css",
  "/js/app.mjs",
  "/js/events.mjs",
  "/js/lib/bloub.mjs",
  "/js/lib/log.mjs",
  "/js/components/reve-feed.mjs",
  "/js/components/reve-autocomplete.mjs",
  "/js/components/reve-composer.mjs",
];
self.addEventListener("install", (event) => {
  event.waitUntil(
    caches.open(CACHE).then((cache) => cache.addAll(SHELL)).then(() => self.skipWaiting())
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches.keys().then((keys) =>
      Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k)))
    ).then(() => self.clients.claim())
  );
});

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  if (url.origin !== self.location.origin) return;
  if (url.pathname === "/" || url.pathname.startsWith("/api/")) return;
  event.respondWith(
    caches.match(event.request).then((hit) => hit || fetch(event.request))
  );
});
