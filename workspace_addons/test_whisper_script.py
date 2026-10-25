import sys
from pathlib import Path
from pywhispercpp.model import Model

WORKSPACE = Path("/home/mrsilhouette/orion-workspace")

def verify_whisper():
    print("Initializing pywhispercpp (Whisper.cpp)...")
    # Using tiny model for lightweight offline verification
    model = Model('tiny', print_realtime=False, print_progress=False)
    print("Whisper model loaded successfully.")
    return True

if __name__ == "__main__":
    verify_whisper()
