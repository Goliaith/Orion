import subprocess
from pathlib import Path

WORKSPACE = Path("/home/mrsilhouette/orion-workspace")

def run_verification_suite():
    print("=== Orion Comprehensive Verification Suite ===")
    
    tests = [
        ("Master CLI Tool Help", ["python3", str(WORKSPACE / "orion_tool.py"), "help"]),
        ("Financial Ledger Report", ["python3", str(WORKSPACE / "orion_tool.py"), "finance", "report"]),
        ("PDF Invoice Generator", ["python3", str(WORKSPACE / "invoice_generator.py"), "INV-1001"]),
        ("Smart Model Router Test", ["python3", str(WORKSPACE / "orion_router.py"), "Hello"]),
        ("Semantic Search Indexer", ["python3", str(WORKSPACE / "semantic_search.py")]),
        ("Workspace Self-Healer", ["python3", str(WORKSPACE / "auto_healer.py")]),
        ("Text-to-Speech Engine", ["python3", str(WORKSPACE / "orion_tts.py"), "Verification complete."]),
        ("Whisper STT Module", ["python3", str(WORKSPACE / "test_whisper_script.py")]),
        ("Tesseract OCR Module", ["python3", str(WORKSPACE / "test_ocr_script.py")]),
    ]
    
    passed = 0
    failed = 0
    
    for name, cmd in tests:
        print(f"\n[TEST] Running {name}...")
        try:
            res = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
            if res.returncode == 0:
                print(f" -> PASSED: {name}")
                passed += 1
            else:
                print(f" -> FAILED: {name} (Exit code {res.returncode})")
                print(f"Stderr: {res.stderr.strip()}")
                failed += 1
        except Exception as e:
            print(f" -> ERROR: {name} ({e})")
            failed += 1
            
    print(f"\n=== Verification Summary ===")
    print(f"Passed: {passed}")
    print(f"Failed: {failed}")
    if failed == 0:
        print("All Orion workstation subsystems are fully operational!")
    else:
        print("Some subsystems require attention.")

if __name__ == "__main__":
    run_verification_suite()
