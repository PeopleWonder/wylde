#!/usr/bin/env python3
"""
startup.py — Windows Startup folder integration for wylde-memgraph.

Usage:
    python startup.py install    # Add to Windows Startup folder
    python startup.py uninstall  # Remove from Startup folder
    python startup.py status     # Check if installed
"""

import os
import sys
from pathlib import Path

STARTUP_DIR = (
    Path(os.environ.get("APPDATA", ""))
    / "Microsoft"
    / "Windows"
    / "Start Menu"
    / "Programs"
    / "Startup"
)
BAT_NAME = "wylde-memgraph.bat"
BAT_PATH = STARTUP_DIR / BAT_NAME
SERVICE_BAT = Path(__file__).parent / "start_graph.bat"


def install() -> bool:
    bat_content = f'@echo off\nstart /min "" "{SERVICE_BAT}"\n'
    STARTUP_DIR.mkdir(parents=True, exist_ok=True)
    BAT_PATH.write_text(bat_content)
    print(f"Installed: {BAT_PATH}")
    print(f"  Runs minimised on Windows login: {SERVICE_BAT}")
    return True


def uninstall() -> bool:
    if BAT_PATH.exists():
        BAT_PATH.unlink()
        print(f"Removed: {BAT_PATH}")
    else:
        print(f"Not installed: {BAT_PATH}")
    return True


def status() -> bool:
    if BAT_PATH.exists():
        print(f"Installed: {BAT_PATH}")
        print(f"  {BAT_PATH.read_text().strip()}")
        return True
    print(f"Not installed: {BAT_PATH}")
    return False


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Usage: python startup.py [install|uninstall|status]")
        sys.exit(1)
    action = sys.argv[1].lower()
    handlers = {"install": install, "uninstall": uninstall, "status": status}
    fn = handlers.get(action)
    if fn is None:
        print(f"Unknown action: {action}")
        sys.exit(1)
    sys.exit(0 if fn() else 1)
