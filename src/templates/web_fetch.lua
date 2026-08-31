-- Workspace plugin. Bot-editable. `ctx.sh` runs in the microVM.

tool("web_fetch", {
  description = "Fetch a URL over HTTP and return the response body (via curl in the VM)",
  replay = "safe",
  params = {
    { name = "url", type = "string", description = "https URL to fetch", required = true },
  },
  run = function(args, ctx)
    local url = args.url or ""
    if url == "" then
      return "url is required"
    end
    return ctx.sh("curl -fsSL --max-time 30 -- " .. ctx.shellescape(url))
  end,
})
