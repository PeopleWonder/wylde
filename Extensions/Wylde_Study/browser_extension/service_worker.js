// service_worker.js — background script for the Wylde Study extension.
//
// All popup ↔ content script communication routes through here so we
// have one place that talks to the Gateway. The popup sends a
// message like { kind: "summarize", payload: { text } }; we POST to
// the Gateway, then reply with { ok, data } or { ok: false, error }.
//
// TODO: real implementation should batch repeated index_page calls
// (avoid hammering the Gateway when the user opens many tabs in
// quick succession), and cache the result of identical query calls
// for a few seconds.

import { getGatewayBase, endpointURL, ENDPOINT } from "./config.js";

const ROUTES = {
  index: ENDPOINT.indexPage,
  query: ENDPOINT.query,
  summarize: ENDPOINT.summarize,
  explain: ENDPOINT.explain,
  flashcards: ENDPOINT.flashcards,
};

async function dispatch(kind, payload) {
  const route = ROUTES[kind];
  if (!route) {
    return { ok: false, error: `unknown kind: ${kind}` };
  }
  const base = await getGatewayBase();
  const url = endpointURL(base, route);
  try {
    const response = await fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload || {}),
    });
    const data = await response.json().catch(() => null);
    if (!response.ok) {
      return {
        ok: false,
        status: response.status,
        error: (data && data.error) || `HTTP ${response.status}`,
      };
    }
    return { ok: true, data };
  } catch (exc) {
    return { ok: false, error: String(exc) };
  }
}

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (!message || !message.kind) {
    sendResponse({ ok: false, error: "missing 'kind'" });
    return false;
  }
  dispatch(message.kind, message.payload).then(sendResponse);
  // returning true tells Chrome we'll call sendResponse asynchronously.
  return true;
});

// Optional: on install, populate a default config so the popup
// doesn't show empty fields the first time the user opens it.
chrome.runtime.onInstalled.addListener(() => {
  chrome.storage.local.get({ gatewayBase: null }, (result) => {
    if (!result.gatewayBase) {
      chrome.storage.local.set({ gatewayBase: "http://127.0.0.1:8020" });
    }
  });
});
