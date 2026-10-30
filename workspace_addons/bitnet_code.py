#!/usr/bin/env python3
import sys
import subprocess
import os

def generate_code(prompt):
    full_prompt = f"# Write clean, concise Python code.\n# Task: {prompt}\n```python\n"
    cli_path = os.path.expanduser("~/orion-dev/BitNet/build/bin/llama-cli")
    model_path = os.path.expanduser("~/orion-dev/BitNet/models/bitnet_b1_58-3B/ggml-model-i2_s.gguf")
    cmd = [
        cli_path,
        "-m", model_path,
        "-p", full_prompt,
        "-n", "256",
        "-t", "4",
        "-c", "2048",
        "--temp", "0.2"
    ]
    print(f"Running BitNet code generator for: {prompt}")
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, check=True)
        print(result.stdout)
    except Exception as e:
        print(f"Error during generation: {e}")

if __name__ == "__main__":
    if len(sys.argv) > 1:
        generate_code(" ".join(sys.argv[1:]))
    else:
        print("Usage: python3 bitnet_code.py 'your prompt here'")
