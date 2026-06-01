// content.js — extracts text from the current page on demand.
//
// Runs in the page context. The popup posts a message asking for the
// page text; we strip out script/style/nav/footer/aside, return the
// document.title plus the visible text. Real implementation should
// use Readability.js (or similar) for cleaner extraction, but for
// the wiring proof-of-concept this is enough.
//
// TODO: integrate Mozilla Readability for better extraction; chunk
// long pages so the index_page payload doesn't blow request limits;
// add language detection.

function extractPageText() {
  const clone = document.cloneNode(true);
  const removeSelectors = [
    "script",
    "style",
    "noscript",
    "nav",
    "footer",
    "header",
    "aside",
    "form",
    "iframe",
    "[aria-hidden='true']",
  ];
  for (const sel of removeSelectors) {
    for (const node of clone.querySelectorAll(sel)) {
      node.remove();
    }
  }
  const text = (clone.body && clone.body.innerText) || "";
  return text.replace(/[ \t]+/g, " ").replace(/\n{3,}/g, "\n\n").trim();
}

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (message && message.kind === "extract_page") {
    sendResponse({
      ok: true,
      data: {
        url: location.href,
        title: document.title,
        text: extractPageText(),
      },
    });
    return false;
  }
  return false;
});
