"""Webcrawler extension package marker.

Present so pytest gives ``Extensions/Webcrawler/tests/smoke_test.py`` a
unique qualified module name (``Extensions.Webcrawler.tests.smoke_test``)
instead of bare ``tests.smoke_test``, which collides with the
``Core/harness/tests/`` package when the full suite runs together.
"""
