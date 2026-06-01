"""Data extraction utilities."""

import logging
from typing import Dict, Any
from bs4 import BeautifulSoup

logger = logging.getLogger(__name__)


class Extractor:
    """Extract structured data from HTML."""

    def extract_by_rules(self, html: str, rules: Dict[str, Any]) -> Dict[str, Any]:
        """
        Extract data from HTML using rules.

        Rules format:
        {
          "field_name": {
            "selector": "css-selector",
            "attribute": "text" | "href" | "class" | etc
          }
        }

        Args:
            html: HTML content to extract from
            rules: Extraction rules dict

        Returns:
            Extracted data dict
        """
        try:
            soup = BeautifulSoup(html, "html.parser")
            result: dict[str, Any] = {}

            for field_name, rule in rules.items():
                try:
                    selector = rule.get("selector", "")
                    attribute = rule.get("attribute", "text")
                    multiple = rule.get("multiple", False)

                    if not selector:
                        result[field_name] = None
                        continue

                    elements = soup.select(selector)

                    if not elements:
                        result[field_name] = None
                        continue

                    if multiple:
                        # Return list of values
                        values: list[str] = []
                        for element in elements:
                            if attribute == "text":
                                values.append(element.get_text(strip=True))
                            else:
                                attr_val = element.get(attribute, "")
                                values.append(
                                    str(attr_val) if attr_val is not None else ""
                                )
                        result[field_name] = values
                    else:
                        # Return first value only
                        element = elements[0]
                        if attribute == "text":
                            result[field_name] = element.get_text(strip=True)
                        else:
                            result[field_name] = element.get(attribute, "")

                except Exception as e:
                    logger.warning(f"Error extracting field {field_name}: {e}")
                    result[field_name] = None

            return result

        except Exception as e:
            logger.error(f"Extract error: {e}")
            return {"error": str(e)}


# Create singleton instance
extractor = Extractor()
