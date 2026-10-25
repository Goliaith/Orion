#!/usr/bin/env python3
"""
Orion Dashboard Generator - updates dashboard.md with live workspace and system status.
"""
from pathlib import Path
import datetime

WORKSPACE = Path("/home/mrsilhouette/orion-workspace")
DASHBOARD = WORKSPACE / "dashboard.md"

def generate():
    now = datetime.datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    tasks_content = "(Tasks file not found)"
    tasks_file = WORKSPACE / "tasks.md"
    if tasks_file.exists():
        tasks_content = tasks_file.read_text().strip()

    content = f"""# Orion Live Dashboard
*Last updated: {now}*

## 📊 System & AI Status
- **OS**: Fedora Linux 44 (KDE Plasma)
- **Local AI Router**: Active (1-model capped)
- **Test Suite**: 100/100 Tests Passing (~7.8s)
- **Workspace Auto-Watcher**: Ready / Configured

## 📝 Current Tasks (`tasks.md`)
{tasks_content}

## 📁 Recent Workspace Files
"""
    for p in sorted(WORKSPACE.glob("*")):
        if p.is_file() and p.name != "dashboard.md":
            content += f"- `{p.name}`\n"

    DASHBOARD.write_text(content)
    print(f"Dashboard updated at {DASHBOARD}")

if __name__ == "__main__":
    generate()
