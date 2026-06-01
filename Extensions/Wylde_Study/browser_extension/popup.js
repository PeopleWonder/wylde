// popup.js — UI glue for the Wylde Study popup.
//
// Pure dispatcher: every action sends a runtime message to the
// service worker, which makes the actual fetch call to the Gateway.
// Keeps the popup free of CORS / endpoint concerns and means the
// popup tab closing mid-request doesn't kill the call.
//
// TODO: real implementation should render structured results — this
// proof-of-concept just dumps JSON into a <pre>.

const $ = (id) => document.getElementById(id);
const out = $("output");

function show(payload) {
  out.textContent =
    typeof payload === "string" ? payload : JSON.stringify(payload, null, 2);
}

function call(kind, payload) {
  show("Working…");
  return new Promise((resolve) => {
    chrome.runtime.sendMessage({ kind, payload }, (response) => {
      show(response);
      resolve(response);
    });
  });
}

async function getActiveTab() {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  return tab;
}

async function getPageContent() {
  const tab = await getActiveTab();
  if (!tab) return null;
  return new Promise((resolve) => {
    chrome.tabs.sendMessage(tab.id, { kind: "extract_page" }, (response) => {
      // Content script may not be injected on chrome:// or extension pages.
      if (chrome.runtime.lastError || !response || !response.ok) {
        resolve(null);
        return;
      }
      resolve(response.data);
    });
  });
}

async function getSelection() {
  const tab = await getActiveTab();
  if (!tab) return "";
  const results = await chrome.scripting.executeScript({
    target: { tabId: tab.id },
    func: () => window.getSelection ? window.getSelection().toString() : "",
  });
  return (results && results[0] && results[0].result) || "";
}

$("indexBtn").addEventListener("click", async () => {
  const page = await getPageContent();
  if (!page) {
    show("Could not read the current page (extension pages and chrome:// URLs are excluded).");
    return;
  }
  await call("index", page);
});

$("summarizeBtn").addEventListener("click", async () => {
  const page = await getPageContent();
  if (!page || !page.text) {
    show("No text to summarize.");
    return;
  }
  await call("summarize", { text: page.text });
});

$("askBtn").addEventListener("click", async () => {
  const q = $("qInput").value.trim();
  if (!q) {
    show("Type a question first.");
    return;
  }
  await call("query", { q });
});

$("explainBtn").addEventListener("click", async () => {
  const selection = await getSelection();
  if (!selection) {
    show("Select some text on the page first.");
    return;
  }
  await call("explain", { text: selection });
});

$("cardsBtn").addEventListener("click", async () => {
  const page = await getPageContent();
  if (!page || !page.text) {
    show("No text to turn into flashcards.");
    return;
  }
  await call("flashcards", { text: page.text });
});
