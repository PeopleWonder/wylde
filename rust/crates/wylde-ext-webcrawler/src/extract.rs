//! Rule-based HTML extraction — port of `Extensions/Webcrawler/extractor.py`
//! (`Extractor.extract_by_rules`) onto the `scraper` crate.
//!
//! Rule shape (unchanged from the Python):
//! ```json
//! {
//!   "field_name": {
//!     "selector": "css-selector",
//!     "attribute": "text" | "href" | "src" | ...,   // default "text"
//!     "multiple": false                              // default false
//!   }
//! }
//! ```
//! A field with no `selector`, or whose selector matches nothing, yields
//! `null`. `attribute == "text"` returns the element's stripped text; any
//! other attribute returns that attribute's value (or `""` when absent). A
//! per-field error (e.g. an unparseable selector) is swallowed to `null`,
//! exactly as the Python `try/except` does.

use scraper::{Html, Selector};
use serde_json::{Map, Value};

use crate::scrape::element_text;

/// Apply `rules` to `html`. Returns a JSON object keyed by field name. On a
/// catastrophic failure returns `{"error": "..."}` (parity with the Python
/// outer `except`), though `scraper`'s parser is infallible so this is only a
/// defensive shape.
pub fn extract_by_rules(html: &str, rules: &Map<String, Value>) -> Value {
    let doc = Html::parse_document(html);
    let mut result = Map::with_capacity(rules.len());

    for (field, rule) in rules {
        result.insert(field.clone(), extract_field(&doc, rule));
    }

    Value::Object(result)
}

fn extract_field(doc: &Html, rule: &Value) -> Value {
    let rule = match rule.as_object() {
        Some(r) => r,
        None => return Value::Null,
    };
    let selector_str = rule.get("selector").and_then(Value::as_str).unwrap_or("");
    let attribute = rule.get("attribute").and_then(Value::as_str).unwrap_or("text");
    let multiple = rule.get("multiple").and_then(Value::as_bool).unwrap_or(false);

    if selector_str.is_empty() {
        return Value::Null;
    }
    // A bad selector is swallowed to null (the Python per-field `except`).
    let selector = match Selector::parse(selector_str) {
        Ok(s) => s,
        Err(_) => return Value::Null,
    };

    let mut elements = doc.select(&selector);

    if multiple {
        let values: Vec<Value> = doc
            .select(&selector)
            .map(|el| Value::String(value_for(&el, attribute)))
            .collect();
        if values.is_empty() {
            return Value::Null;
        }
        Value::Array(values)
    } else {
        match elements.next() {
            Some(el) => Value::String(value_for(&el, attribute)),
            None => Value::Null,
        }
    }
}

/// One element's value for `attribute`: stripped text, or the named attribute
/// (or `""` when the attribute is absent).
fn value_for(el: &scraper::ElementRef, attribute: &str) -> String {
    if attribute == "text" {
        element_text(el)
    } else {
        el.value().attr(attribute).unwrap_or("").to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HTML: &str = r#"
        <html><body>
          <h1 class="title">  Hello World  </h1>
          <a href="/one">First</a>
          <a href="/two">Second</a>
          <span class="empty"></span>
        </body></html>
    "#;

    fn rules(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn single_text_is_stripped() {
        let r = rules(json!({ "title": { "selector": "h1.title" } }));
        let out = extract_by_rules(HTML, &r);
        assert_eq!(out["title"], "Hello World");
    }

    #[test]
    fn single_attribute() {
        let r = rules(json!({ "first": { "selector": "a", "attribute": "href" } }));
        let out = extract_by_rules(HTML, &r);
        assert_eq!(out["first"], "/one");
    }

    #[test]
    fn multiple_collects_all() {
        let r = rules(json!({ "links": { "selector": "a", "attribute": "href", "multiple": true } }));
        let out = extract_by_rules(HTML, &r);
        assert_eq!(out["links"], json!(["/one", "/two"]));
    }

    #[test]
    fn missing_selector_is_null() {
        let r = rules(json!({ "nope": { "attribute": "text" } }));
        assert_eq!(extract_by_rules(HTML, &r)["nope"], Value::Null);
    }

    #[test]
    fn no_match_is_null() {
        let r = rules(json!({ "ghost": { "selector": ".does-not-exist" } }));
        assert_eq!(extract_by_rules(HTML, &r)["ghost"], Value::Null);
    }

    #[test]
    fn bad_selector_is_null() {
        let r = rules(json!({ "bad": { "selector": ">>>" } }));
        assert_eq!(extract_by_rules(HTML, &r)["bad"], Value::Null);
    }

    #[test]
    fn missing_attribute_is_empty_string() {
        let r = rules(json!({ "x": { "selector": "a", "attribute": "data-nope" } }));
        assert_eq!(extract_by_rules(HTML, &r)["x"], "");
    }
}
