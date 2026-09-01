if ("serviceWorker" in navigator) {
  let reloading = false;
  navigator.serviceWorker.addEventListener("controllerchange", () => {
    if (reloading) return;
    reloading = true;
    location.reload();
  });

  navigator.serviceWorker.register("/sw.js").then((registration) => {
    const activate = (worker) => {
      if (worker?.state === "installed" && navigator.serviceWorker.controller) {
        worker.postMessage("skip-waiting");
      }
    };

    activate(registration.waiting);
    registration.addEventListener("updatefound", () => {
      const worker = registration.installing;
      worker?.addEventListener("statechange", () => activate(worker));
    });
    registration.update().catch(() => {});
  }).catch(() => {});
}
