import os
import subprocess
from pathlib import Path

WORKSPACE = Path("/home/mrsilhouette/orion-workspace")

def refactor_code(target_path, instruction):
    path = Path(target_path)
    if not path.exists():
        print(f"Target file not found: {target_path}")
        return False
    
    original_code = path.read_text()
    print(f"Analyzing {target_path} for autonomous refactoring...")
    
    # Prompt the local 7B model to refactor the code based on instruction
    prompt = f"Refactor the following Python code according to this instruction: '{instruction}'. Return ONLY the updated valid Python code inside standard code blocks.\n\n```python\n{original_code}\n```"
    
    cmd = ["ollama", "run", "qwen2.5:7b", prompt]
    res = subprocess.run(cmd, capture_output=True, text=True)
    
    if res.returncode == 0:
        output = res.stdout
        if "```python" in output:
            code_block = output.split("```python")[1].split("```")[0].strip()
            path.write_text(code_block)
            print(f"Successfully refactored and updated {target_path}!")
            return True
    
    print("Refactoring failed or returned invalid output.")
    return False

if __name__ == "__main__":
    print("Autonomous Code Refactoring Agent ready.")
