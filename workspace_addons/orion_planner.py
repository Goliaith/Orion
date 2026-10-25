import sys
from pathlib import Path

WORKSPACE = Path("/home/mrsilhouette/orion-workspace")
PLAN_FILE = WORKSPACE / "current_plan.md"

def create_plan(title, steps):
    content = f"""# Orion Execution Plan: {title}
## Status: In Progress

### Steps:
"""
    for i, step in enumerate(steps, 1):
        content += f"{i}. [ ] {step}\n"
    
    PLAN_FILE.write_text(content)
    print(f"Plan created at {PLAN_FILE}")

def update_step(step_num):
    if not PLAN_FILE.exists():
        print("No active plan found.")
        return
    
    lines = PLAN_FILE.read_text().splitlines()
    new_lines = []
    for line in lines:
        if line.strip().startswith(f"{step_num}. [ ]"):
            line = line.replace("[ ]", "[x]")
        new_lines.append(line)
    
    PLAN_FILE.write_text("\n".join(new_lines) + "\n")
    print(f"Updated step {step_num} as complete.")

if __name__ == "__main__":
    if len(sys.argv) > 2 and sys.argv[1] == "new":
        title = sys.argv[2]
        steps = sys.argv[3:]
        create_plan(title, steps)
    elif len(sys.argv) > 2 and sys.argv[1] == "done":
        update_step(sys.argv[2])
    else:
        if PLAN_FILE.exists():
            print(PLAN_FILE.read_text())
        else:
            print("No active plan.")
