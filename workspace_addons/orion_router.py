import sys
import subprocess
import json

def route_and_generate(prompt):
    # Simple heuristic: If prompt requires complex logic/coding/analysis, use qwen2.5:7b.
    # If prompt is conversational, fast chat, or simple, use llama3.2:3b.
    complex_keywords = ["code", "function", "calculate", "analyze", "debug", "refactor", "algorithm", "derive", "solve"]
    
    is_complex = any(kw in prompt.lower() for kw in complex_keywords)
    model = "qwen2.5:7b" if is_complex else "llama3.2:3b"
    
    print(f"[Orion Router] Selected model: {model} (based on complexity heuristic)")
    
    cmd = ["ollama", "run", model, prompt]
    res = subprocess.run(cmd, capture_output=True, text=True)
    return res.stdout.strip()

if __name__ == "__main__":
    if len(sys.argv) > 1:
        prompt = " ".join(sys.argv[1:])
    else:
        prompt = "Hello, how are you today?"
    
    output = route_and_generate(prompt)
    print("\n--- Response ---")
    print(output)
