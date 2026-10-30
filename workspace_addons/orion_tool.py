#!/usr/bin/env python3
import sys
import subprocess
from pathlib import Path

WORKSPACE = Path("/home/mrsilhouette/orion-workspace")

def print_help():
    print("""
Orion Master Workspace Tool (orion-tool)
Usage: python3 orion_tool.py <command> [arguments]

Commands:
  speak <text>              - Synthesize and play neural text-to-speech audio
  route <prompt>            - Intelligently route prompt between 3B and 7B local models
  finance report            - Generate business profit & loss summary report
  finance invoice <id> <client> <amount> [Paid/Unpaid] - Add/update a client invoice
  finance expense <cat> <amount> <desc> - Log a business expense
  search                    - Inspect cached persistent vector embeddings
  heal                      - Run automated workspace linter and test checks
  help                      - Show this help message
""")

def main():
    if len(sys.argv) < 2:
        print_help()
        return

    cmd = sys.argv[1].lower()
    
    if cmd == "speak":
        text = " ".join(sys.argv[2:]) if len(sys.argv) > 2 else "Hello MrSilhouette."
        subprocess.run(["python3", str(WORKSPACE / "orion_tts.py"), text])
        subprocess.run(["paplay", str(WORKSPACE / "output.wav")])
        print("Speech spoken aloud.")

    elif cmd == "route":
        prompt = " ".join(sys.argv[2:]) if len(sys.argv) > 2 else "Hello"
        subprocess.run(["python3", str(WORKSPACE / "orion_router.py"), prompt])

    elif cmd == "finance":
        if len(sys.argv) > 2 and sys.argv[2] == "report":
            subprocess.run(["python3", str(WORKSPACE / "finance_ledger.py")])
        elif len(sys.argv) > 5 and sys.argv[2] == "invoice":
            inv_id, client, amount, status = sys.argv[3], sys.argv[4], float(sys.argv[5]), sys.argv[6] if len(sys.argv) > 6 else "Unpaid"
            script = f"""
from finance_ledger import BusinessLedger
ledger = BusinessLedger()
ledger.add_invoice('{inv_id}', '{client}', {amount}, '{status}')
ledger.generate_report()
"""
            subprocess.run(["python3", "-c", script], cwd=str(WORKSPACE))
        elif len(sys.argv) > 5 and sys.argv[2] == "expense":
            cat, amount, desc = sys.argv[3], float(sys.argv[4]), sys.argv[5]
            script = f"""
from finance_ledger import BusinessLedger
ledger = BusinessLedger()
ledger.add_expense('{cat}', {amount}, '{desc}')
ledger.generate_report()
"""
            subprocess.run(["python3", "-c", script], cwd=str(WORKSPACE))
        else:
            print("Invalid finance subcommand. Use: report, invoice, or expense")

    elif cmd == "search":
        subprocess.run(["python3", str(WORKSPACE / "semantic_search.py")])

    elif cmd == "heal":
        subprocess.run(["python3", str(WORKSPACE / "auto_healer.py")])

    else:
        print(f"Unknown command: {cmd}")
        print_help()

if __name__ == "__main__":
    main()
