//! CSS-selector scraping — port of the `run_scrape` selector loop in
//! `Extensions/Webcrawler/handler.py`.
//!
//! For each selector the Python does `[el.get_text(strip=True) for el in
//! soup.select(sel)]`; an unparseable selector becomes `{"error": "..."}`.
//! [`apply_selectors`] reproduces both. [`element_text`] is the shared
//! `get_text(strip=True)` equivalent (also used by the extract rule engine).

use scraper::{ElementRef, Html, Selector};
use serde_json::{json, Map, Value};

/// `BeautifulSoup`'s `get_text(separator="", strip=True)`: strip each text
/// node, drop empties, concatenate with no separator.
pub fn element_text(el: &ElementRef) -> String {
    el.text()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("")
}

/// Apply a list of CSS selectors to `html`. Returns an object keyed by the
/// selector string: a successful selector maps to an array of matched element
/// texts; an unparseable selector maps to `{"error": "..."}` (parity with the
/// Python per-selector `except`).
pub fn apply_selectors(html: &str, selectors: &[String]) -> Value {
    let doc = Html::parse_document(html);
    let mut out = Map::with_capacity(selectors.len());

    for sel in selectors {
        match Selector::parse(sel) {
            Ok(selector) => {
                let texts: Vec<Value> = doc
                    .select(&selector)
                    .map(|el| Value::String(element_text(&el)))
                    .collect();
                out.insert(sel.clone(), Value::Array(texts));
            }
            Err(e) => {
                out.insert(sel.clone(), json!({ "error": e.to_string() }));
            }
        }
    }

    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTML: &str = r#"
        <ul>
          <li class="item"> Apple </li>
          <li class="item">Banana</li>
        </ul>
        <p id="lead">Lead   paragraph</p>
    "#;

    #[test]
    fn selects_and_strips_text() {
        let out = apply_selectors(HTML, &["li.item".to_owned()]);
        assert_eq!(out["li.item"], json!(["Apple", "Banana"]));
    }

    #[test]
    fn unmatched_selector_is_empty_array() {
        let out = apply_selectors(HTML, &[".missing".to_owned()]);
        assert_eq!(out[".missing"], json!([]));
    }

    #[test]
    fn bad_selector_maps_to_error() {
        let out = apply_selectors(HTML, &[">>bad".to_owned()]);
        assert!(out[">>bad"].get("error").is_some());
    }

    #[test]
    fn element_text_joins_descendants() {
        let html = Html::parse_fragment("<p>Hello <b>bold</b> world</p>");
        let sel = Selector::parse("p").unwrap();
        let el = html.select(&sel).next().unwrap();
        assert_eq!(element_text(&el), "Helloboldworld");
    }
}
