#!/usr/bin/env python3
import sys
import subprocess
import os

def run_bitnet(task_type, prompt):
    cli_path = os.path.expanduser("~/orion-dev/BitNet/build/bin/llama-cli")
    model_path = os.path.expanduser("~/orion-dev/BitNet/models/bitnet_b1_58-3B/ggml-model-i2_s.gguf")
    
    if task_type == "code":
        full_prompt = f"# Write clean, concise Python code.\n# Task: {prompt}\n```python\n"
    elif task_type == "summarize":
        full_prompt = f"Summarize the following text concisely:\n\n{prompt}\n\nSummary:"
    else:
        full_prompt = prompt

    cmd = [
        cli_path,
        "-m", model_path,
        "-p", full_prompt,
        "-n", "256",
        "-t", "4",
        "-c", "2048",
        "--temp", "0.2"
    ]
    
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, check=True)
        print(result.stdout)
    except Exception as e:
        print(f"Error executing BitNet: {e}")

if __name__ == "__main__":
    if len(sys.argv) > 2:
        run_bitnet(sys.argv[1], " ".join(sys.argv[2:]))
    else:
        print("Usage: python3 bitnet_specialist.py [code|summarize|chat] 'your prompt'")
