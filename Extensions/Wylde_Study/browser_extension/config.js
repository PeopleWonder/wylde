// config.js — endpoint configuration for the Wylde Study browser extension.
//
// All requests go through the Wylde Gateway. The Gateway terminates the
// HTTP ingress, then forwards to the extension_bridge dispatcher, which
// calls into Wylde_Study/handler.py. Endpoints are addressed by
//   /extensions/Wylde_Study/<endpoint>
// where <endpoint> matches the manifest's per-tool 'endpoint' field.
//
// TODO: once the Gateway is fully wired, the user should be able to
// override the host:port from chrome.storage so this file isn't a
// hard-coded localhost target. For now we read from chrome.storage if
// set, falling back to a sensible default.

export const DEFAULT_GATEWAY = "http://127.0.0.1:8020";

export const EXT_NAME = "Wylde_Study";

export const ENDPOINT = {
  indexPage: "index_page",
  query: "query",
  summarize: "summarize",
  explain: "explain",
  flashcards: "flashcards",
};

export async function getGatewayBase() {
  if (typeof chrome === "undefined" || !chrome.storage) return DEFAULT_GATEWAY;
  return new Promise((resolve) => {
    chrome.storage.local.get({ gatewayBase: DEFAULT_GATEWAY }, (result) => {
      resolve(result.gatewayBase || DEFAULT_GATEWAY);
    });
  });
}

export function endpointURL(base, endpoint) {
  return `${base.replace(/\/$/, "")}/extensions/${EXT_NAME}/${endpoint}`;
}
