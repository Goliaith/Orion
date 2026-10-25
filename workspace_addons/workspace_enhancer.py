import time
import subprocess
from pathlib import Path

WORKSPACE = Path("/home/mrsilhouette/orion-workspace")
DASHBOARD_FILE = WORKSPACE / "dashboard.md"

def run_checks():
    lint_res = subprocess.run(["flake8", str(WORKSPACE)], capture_output=True, text=True)
    lint_status = "Passing" if lint_res.returncode == 0 else f"Issues found ({len(lint_res.stdout.splitlines())})"
    
    # Git status check
    git_res = subprocess.run(["git", "-C", str(WORKSPACE), "status", "--porcelain"], capture_output=True, text=True)
    uncommitted = len(git_res.stdout.splitlines())
    
    timestamp = time.strftime("%Y-%m-%d %H:%M:%S")
    
    content = f"""# Orion Enhanced Workspace Dashboard
*Last Updated: {timestamp}*

## System & Code Health
- **Linter Status (flake8)**: {lint_status}
- **Uncommitted Workspace Files**: {uncommitted}
- **Vector Embedding Cache**: Active (SQLite backed)
- **Automatic Git Snapshotter**: Active

---
*Maintained by Orion Auto-Watcher & Enhancer Daemon.*
"""
    DASHBOARD_FILE.write_text(content)
    
    # Auto-commit if changes exist
    if uncommitted > 0:
        subprocess.run(["git", "-C", str(WORKSPACE), "add", "."])
        subprocess.run(["git", "-C", str(WORKSPACE), "commit", "-m", f"Auto-snapshot at {timestamp}"])
        print(f"[{timestamp}] Workspace auto-snapshot committed.")

if __name__ == "__main__":
    run_checks()
