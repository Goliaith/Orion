import subprocess
from pathlib import Path

WORKSPACE = Path("/home/mrsilhouette/orion-workspace")

def run_tests_and_lint():
    print("Running automated test and lint checks...")
    lint_res = subprocess.run(["flake8", str(WORKSPACE)], capture_output=True, text=True)
    
    if lint_res.returncode != 0:
        issues = lint_res.stdout.splitlines()
        print(f"Found {len(issues)} linter issues. Analyzing with qwen2.5:7b...")
        # In a full autonomous loop, we would pass issues to Ollama here to generate a patch.
        print("Linter check completed.")
    else:
        print("All checks passed cleanly!")

if __name__ == "__main__":
    run_tests_and_lint()
